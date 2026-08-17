use crate::{als, brightness, config, frame, gamma, idle, predictor};
use anyhow::Result;
use futures_util::future::join_all;
use smol::channel::{self, Receiver, Sender};
use smol::Task;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOPOLOGY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const TOPOLOGY_SETTLE_INTERVAL: Duration = Duration::from_secs(2);
const DISCOVERY_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const START_RETRY_INTERVAL: Duration = Duration::from_secs(5);

pub struct Runtime {
    configured: Vec<config::Output>,
    als_scale: als::Scale,
    legacy_thresholds: HashMap<u64, String>,
    registrations: Sender<Sender<als::Reading>>,
    topology: Vec<String>,
    desired: HashMap<String, config::Output>,
    sessions: HashMap<String, Session>,
    failures: HashMap<String, Instant>,
    last_discovery: Option<Instant>,
    settling_until: Option<Instant>,
    commands: Receiver<crate::control::Command>,
    status: crate::control::Hub,
    idle: Option<config::Idle>,
    idle_events: Option<Receiver<idle::Event>>,
    idle_brightness: Option<u8>,
}

struct Session {
    output: config::Output,
    active: Arc<AtomicBool>,
    brightness: Task<()>,
    capturer: Task<()>,
    gamma: Option<Task<()>>,
    commands: Sender<brightness::Command>,
    gamma_commands: Option<Sender<gamma::Command>>,
}

impl Runtime {
    pub fn new(
        configured: Vec<config::Output>,
        als_scale: als::Scale,
        legacy_thresholds: HashMap<u64, String>,
        registrations: Sender<Sender<als::Reading>>,
        commands: Receiver<crate::control::Command>,
        status: crate::control::Hub,
        idle: Option<(config::Idle, Receiver<idle::Event>)>,
    ) -> Self {
        let (idle, idle_events) = idle.map_or((None, None), |(config, events)| {
            (Some(config), Some(events))
        });
        Self {
            configured,
            als_scale,
            legacy_thresholds,
            registrations,
            topology: Vec::new(),
            desired: HashMap::new(),
            sessions: HashMap::new(),
            failures: HashMap::new(),
            last_discovery: None,
            settling_until: None,
            commands,
            status,
            idle,
            idle_events,
            idle_brightness: None,
        }
    }

    pub async fn run(&mut self) {
        enum Event {
            Command(Result<crate::control::Command, smol::channel::RecvError>),
            Idle(Result<idle::Event, smol::channel::RecvError>),
            Tick,
        }

        self.step().await;
        loop {
            let event = smol::future::race(
                async { Event::Command(self.commands.recv().await) },
                smol::future::race(
                    async {
                        smol::Timer::after(TOPOLOGY_POLL_INTERVAL).await;
                        Event::Tick
                    },
                    async {
                        match &self.idle_events {
                            Some(events) => Event::Idle(events.recv().await),
                            None => std::future::pending().await,
                        }
                    },
                ),
            )
            .await;
            match event {
                Event::Command(Ok(command)) => self.command(command).await,
                Event::Command(Err(_)) => return,
                Event::Idle(Ok(event)) => self.idle_event(event).await,
                Event::Idle(Err(_)) => self.idle_events = None,
                Event::Tick => self.step().await,
            }
        }
    }

    pub async fn stop(&mut self) {
        let sessions = self
            .sessions
            .drain()
            .map(|(_, session)| session.stop())
            .collect::<Vec<_>>();
        join_all(sessions).await;
    }

    async fn idle_event(&mut self, event: idle::Event) {
        let result = match event {
            idle::Event::PowerSourceChanged(source) => {
                let idle = self
                    .idle
                    .expect("Power source event received without idle configuration");
                let (name, profile) = match source {
                    idle::PowerSource::Ac => ("ac", idle.ac),
                    idle::PowerSource::Battery => ("battery", idle.battery),
                };
                self.status.set_idle_profile(
                    name,
                    profile.enabled,
                    profile.timeout,
                    profile.brightness,
                );
                return;
            }
            idle::Event::Idled(source) => {
                if self.idle_brightness.is_some() {
                    return;
                }
                let idle = self
                    .idle
                    .expect("Idle event received without configuration");
                let percent = match source {
                    idle::PowerSource::Ac => idle.ac.brightness,
                    idle::PowerSource::Battery => idle.battery.brightness,
                };
                self.idle_brightness = Some(percent);
                self.status.set_idled(true);
                log::debug!("User became idle on {source:?}");
                self.all_output_commands(|session| {
                    brightness::CommandAction::IdleEnter(if is_keyboard(&session.output) {
                        0
                    } else {
                        percent
                    })
                })
                .await
            }
            idle::Event::Resumed => {
                if self.idle_brightness.take().is_none() {
                    return;
                }
                self.status.set_idled(false);
                log::debug!("User became active");
                self.output_commands(None, || brightness::CommandAction::IdleLeave)
                    .await
            }
        };
        if let Err(error) = result {
            log::warn!("Unable to update idle brightness: {error}");
        }
    }

    async fn command(&mut self, command: crate::control::Command) {
        let result = self.execute(command.action).await;
        let _ = command.response.send(result).await;
    }

    async fn execute(
        &mut self,
        action: crate::control::Action,
    ) -> std::result::Result<String, String> {
        use crate::control::Action;
        match action {
            Action::Get(property, name) => match property {
                crate::control::Property::Brightness => Ok(format!(
                    "{}%",
                    self.output_command(&name, brightness::CommandAction::Get)
                        .await?
                )),
                crate::control::Property::Dim => Ok(format!(
                    "{}%",
                    self.gamma_output_command(&name, gamma::CommandAction::Get(property))
                        .await?
                )),
                crate::control::Property::Temperature => Ok(format!(
                    "{}K",
                    self.gamma_output_command(&name, gamma::CommandAction::Get(property))
                        .await?
                )),
            },
            Action::Set(property, name, adjustment) => match property {
                crate::control::Property::Brightness => Ok(format!(
                    "{}%",
                    self.output_command(
                        &name,
                        brightness::CommandAction::Set(
                            adjustment
                                .try_into()
                                .map_err(|error: anyhow::Error| error.to_string())?
                        ),
                    )
                    .await?
                )),
                crate::control::Property::Dim => Ok(format!(
                    "{}%",
                    self.gamma_output_command(
                        &name,
                        gamma::CommandAction::Set(property, adjustment),
                    )
                    .await?
                )),
                crate::control::Property::Temperature => Ok(format!(
                    "{}K",
                    self.gamma_output_command(
                        &name,
                        gamma::CommandAction::Set(property, adjustment),
                    )
                    .await?
                )),
            },
            Action::Pause(name, duration) => {
                self.output_commands(name.as_deref(), || {
                    brightness::CommandAction::Pause(duration)
                })
                .await?;
                Ok("ok".to_string())
            }
            Action::Resume(name) => {
                self.output_commands(name.as_deref(), || brightness::CommandAction::Resume)
                    .await?;
                Ok("ok".to_string())
            }
            Action::Toggle(name) => {
                self.output_commands(name.as_deref(), || brightness::CommandAction::Toggle)
                    .await?;
                Ok("ok".to_string())
            }
        }
    }

    async fn output_commands(
        &self,
        name: Option<&str>,
        action: impl Fn() -> brightness::CommandAction,
    ) -> std::result::Result<(), String> {
        if let Some(name) = name {
            self.output_command(name, action()).await?;
            return Ok(());
        }
        self.all_output_commands(|_| action()).await
    }

    async fn all_output_commands(
        &self,
        action: impl Fn(&Session) -> brightness::CommandAction,
    ) -> std::result::Result<(), String> {
        let mut failures = Vec::new();
        for (name, session) in &self.sessions {
            if let Err(error) = send_command(&session.commands, action(session)).await {
                failures.push(format!("{name}: {error}"));
            }
        }
        failures.sort();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!("failed for {}", failures.join("; ")))
        }
    }

    async fn output_command(
        &self,
        name: &str,
        action: brightness::CommandAction,
    ) -> std::result::Result<u8, String> {
        let session = self
            .sessions
            .get(name)
            .ok_or_else(|| format!("unknown or disconnected output '{name}'"))?;
        send_command(&session.commands, action).await
    }

    async fn gamma_output_command(
        &self,
        name: &str,
        action: gamma::CommandAction,
    ) -> std::result::Result<u64, String> {
        let session = self
            .sessions
            .get(name)
            .ok_or_else(|| format!("unknown or disconnected output '{name}'"))?;
        let commands = session
            .gamma_commands
            .as_ref()
            .ok_or_else(|| format!("gamma control is unavailable for '{name}'"))?;
        let (response_tx, response_rx) = channel::bounded(1);
        commands
            .send(gamma::Command {
                action,
                response: response_tx,
            })
            .await
            .map_err(|_| "gamma controller stopped responding".to_string())?;
        response_rx
            .recv()
            .await
            .map_err(|_| "gamma controller stopped responding".to_string())?
    }

    async fn step(&mut self) {
        let topology = config::topology();
        let topology_changed = topology != self.topology;
        let initial = self.last_discovery.is_none();
        let settled = self
            .settling_until
            .is_some_and(|deadline| Instant::now() >= deadline);
        let refresh = self
            .last_discovery
            .is_none_or(|last| last.elapsed() >= DISCOVERY_REFRESH_INTERVAL);

        if topology_changed {
            self.topology = topology;
            if !initial {
                self.settling_until = Some(Instant::now() + TOPOLOGY_SETTLE_INTERVAL);
            }
            self.discover().await;
        } else if settled {
            self.settling_until = None;
            self.discover().await;
        } else if self.settling_until.is_none() && refresh {
            self.discover().await;
        }
        self.reconcile().await;
    }

    async fn discover(&mut self) {
        let configured = self.configured.clone();
        let outputs = smol::unblock(move || config::detected_outputs(configured)).await;
        self.last_discovery = Some(Instant::now());
        let mut desired = HashMap::new();
        for output in outputs {
            let name = output_name(&output).to_string();
            if desired.contains_key(&name) {
                log::warn!("Skipping duplicate discovered output '{name}'");
                continue;
            }
            desired.insert(name, output);
        }
        for (name, output) in &desired {
            if self.desired.get(name) != Some(output) {
                log_discovered(output);
            }
        }
        self.desired = desired;
    }

    async fn reconcile(&mut self) {
        let stopped = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.brightness.is_finished()
                    || session.capturer.is_finished()
                    || session.gamma.as_ref().is_some_and(Task::is_finished)
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in stopped {
            if let Some(session) = self.sessions.remove(&name) {
                session.stop().await;
                self.status.remove_output(&name);
                log::warn!("Output '{name}' stopped unexpectedly; it will be retried");
                self.failures.insert(name, Instant::now());
            }
        }

        let removed = self
            .sessions
            .iter()
            .filter(|(name, session)| self.desired.get(*name) != Some(&session.output))
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in removed {
            if let Some(session) = self.sessions.remove(&name) {
                session.stop().await;
                self.status.remove_output(&name);
                log::info!("Stopped using output '{name}'");
            }
            self.failures.remove(&name);
        }

        let pending = self
            .desired
            .iter()
            .filter(|(name, _)| !self.sessions.contains_key(*name))
            .filter(|_| self.settling_until.is_none())
            .filter(|(name, _)| {
                self.failures
                    .get(*name)
                    .is_none_or(|failed| failed.elapsed() >= START_RETRY_INTERVAL)
            })
            .map(|(name, output)| (name.clone(), output.clone()))
            .collect::<Vec<_>>();
        for (name, output) in pending {
            match Session::start(
                output,
                self.als_scale,
                &self.legacy_thresholds,
                &self.registrations,
                self.status.clone(),
            )
            .await
            {
                Ok(session) => {
                    log::info!("Started using output '{name}'");
                    let keyboard = is_keyboard(&session.output);
                    self.sessions.insert(name.clone(), session);
                    self.failures.remove(&name);
                    if let Some(percent) = self.idle_brightness {
                        let percent = if keyboard { 0 } else { percent };
                        if let Err(error) = self
                            .output_command(&name, brightness::CommandAction::IdleEnter(percent))
                            .await
                        {
                            log::warn!("Unable to update idle brightness for '{name}': {error}");
                        }
                    }
                }
                Err(error) => {
                    log::warn!("Unable to initialize output '{name}': {error:#}");
                    self.failures.insert(name, Instant::now());
                }
            }
        }
    }
}

impl Session {
    async fn start(
        output: config::Output,
        als_scale: als::Scale,
        legacy_thresholds: &HashMap<u64, String>,
        registrations: &Sender<Sender<als::Reading>>,
        status: crate::control::Hub,
    ) -> Result<Self> {
        let (als_tx, als_rx) = channel::bounded(1);
        let (user_tx, user_rx) = channel::bounded(128);
        let (prediction_tx, prediction_rx) = channel::bounded(128);
        let (command_tx, command_rx) = channel::bounded(32);
        let paused = Arc::new(AtomicBool::new(false));

        let (name, capturer, vulkan_device, output_predictor, als_direction, gamma_enabled) =
            match &output {
                config::Output::Backlight(output) => (
                    output.name.clone(),
                    output.capturer.clone(),
                    output.vulkan_device.clone(),
                    output.predictor.clone(),
                    output.als_direction,
                    output.gamma,
                ),
                config::Output::DdcUtil(output) => (
                    output.name.clone(),
                    output.capturer.clone(),
                    output.vulkan_device.clone(),
                    output.predictor.clone(),
                    predictor::AlsDirection::Increasing,
                    output.gamma,
                ),
            };

        let backend = match &output {
            config::Output::Backlight(output) => {
                brightness::Backlight::new(&output.path, output.min_brightness)
                    .await
                    .map(brightness::Brightness::Backlight)?
            }
            config::Output::DdcUtil(output) => {
                let identifier = output.identifier.clone();
                let min_brightness = output.min_brightness;
                brightness::Brightness::DdcUtil(
                    smol::unblock(move || brightness::DdcUtil::new(&identifier, min_brightness))
                        .await?,
                )
            }
        };

        let keyboard = is_keyboard(&output);
        let kind = match &output {
            config::Output::Backlight(_) if keyboard => "keyboard",
            config::Output::Backlight(_) => "backlight",
            config::Output::DdcUtil(_) => "ddc",
        };
        let capturer_name = match &capturer {
            config::Capturer::None => "none",
            _ => "initializing",
        };
        status.add_output(&name, kind, (!keyboard).then_some(capturer_name));

        registrations.send(als_tx).await?;
        let brightness_status = status.clone();
        let brightness_name = name.clone();
        let brightness_paused = paused.clone();
        let brightness = smol::spawn(async move {
            brightness::Controller::new(backend, user_tx, prediction_rx)
                .with_control(
                    command_rx,
                    brightness_status,
                    brightness_name,
                    brightness_paused,
                )
                .run()
                .await;
        });

        let mut additional = Vec::new();
        let (gamma, gamma_commands) = if gamma_enabled && !keyboard {
            let gamma_available = Arc::new(AtomicBool::new(false));
            let (dim_als_tx, dim_als_rx) = channel::bounded(1);
            let (temperature_als_tx, temperature_als_rx) = channel::bounded(1);
            registrations.send(dim_als_tx).await?;
            registrations.send(temperature_als_tx).await?;
            let (dim_user_tx, dim_user_rx) = channel::bounded(128);
            let (temperature_user_tx, temperature_user_rx) = channel::bounded(128);
            let (dim_prediction_tx, dim_prediction_rx) = channel::bounded(128);
            let (temperature_prediction_tx, temperature_prediction_rx) = channel::bounded(128);
            let (gamma_command_tx, gamma_command_rx) = channel::bounded(32);
            additional.push(
                predictor::Controller::adaptive(
                    predictor::controller::adaptive::Controller::new_kind(
                        (dim_prediction_tx, dim_user_rx, dim_als_rx),
                        true,
                        &name,
                        als_scale,
                        legacy_thresholds,
                        predictor::Kind::Dim,
                    )
                    .without_initial_value()
                    .with_als_direction(predictor::AlsDirection::Decreasing)
                    .with_luma_direction(
                        predictor::controller::adaptive::LumaDirection::Increasing,
                    ),
                )
                .with_enabled(gamma_available.clone()),
            );
            additional.push(
                predictor::Controller::adaptive(
                    predictor::controller::adaptive::Controller::new_kind(
                        (
                            temperature_prediction_tx,
                            temperature_user_rx,
                            temperature_als_rx,
                        ),
                        true,
                        &name,
                        als_scale,
                        legacy_thresholds,
                        predictor::Kind::Temperature,
                    )
                    .without_initial_value()
                    .without_luma(),
                )
                .with_enabled(gamma_available.clone()),
            );
            let gamma_controller = gamma::Controller::new(
                gamma::Inputs {
                    dim_user: dim_user_tx,
                    temperature_user: temperature_user_tx,
                    dim_predictions: dim_prediction_rx,
                    temperature_predictions: temperature_prediction_rx,
                    commands: gamma_command_rx,
                },
                paused.clone(),
                gamma_available,
                status.clone(),
                name.clone(),
            );
            (
                Some(smol::spawn(gamma_controller.run())),
                Some(gamma_command_tx),
            )
        } else {
            (None, None)
        };

        let controller = match output_predictor {
            config::Predictor::Manual { points } => {
                predictor::Controller::manual(predictor::controller::manual::Controller::new(
                    prediction_tx,
                    user_rx,
                    als_rx,
                    points
                        .into_iter()
                        .map(|point| predictor::Entry::new(point.als, point.luma, point.reduction))
                        .collect(),
                    &name,
                    als_scale,
                ))
            }
            config::Predictor::Adaptive => predictor::Controller::adaptive(
                predictor::controller::adaptive::Controller::new(
                    prediction_tx,
                    user_rx,
                    als_rx,
                    true,
                    &name,
                    als_scale,
                    legacy_thresholds,
                )
                .with_als_direction(als_direction),
            ),
        }
        .with_additional(additional)
        .with_status(status.clone(), name.clone(), paused);
        let frame_capturer = match capturer {
            config::Capturer::Auto => frame::capturer::Capturer::Auto,
            config::Capturer::Wayland(protocol) => frame::capturer::Capturer::Wayland(
                frame::capturer::wayland::Capturer::new(protocol),
            ),
            config::Capturer::Pipewire(protocol) => frame::capturer::Capturer::Pipewire(protocol),
            config::Capturer::None => frame::capturer::Capturer::None(Default::default()),
        };
        let active = Arc::new(AtomicBool::new(true));
        let capture_active = active.clone();
        let capture_status = status.clone();
        let capturer = smol::spawn(async move {
            frame_capturer
                .run(
                    &name,
                    controller,
                    vulkan_device.as_deref(),
                    capture_active,
                    capture_status,
                )
                .await;
        });

        Ok(Self {
            output,
            active,
            brightness,
            capturer,
            gamma,
            commands: command_tx,
            gamma_commands,
        })
    }

    async fn stop(self) {
        // Stop prediction producers before their receivers. Otherwise a final
        // frame can race with controller cancellation and send into a closed
        // channel during graceful shutdown.
        self.active.store(false, Ordering::Relaxed);
        self.capturer.await;

        if let Some(commands) = &self.gamma_commands {
            let (response_tx, response_rx) = channel::bounded(1);
            let result: Result<Result<u64, String>, String> = async {
                commands
                    .send(gamma::Command {
                        action: gamma::CommandAction::Restore,
                        response: response_tx,
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                response_rx.recv().await.map_err(|error| error.to_string())
            }
            .await;
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => log::debug!(
                    "Unable to restore neutral gamma for '{}': {error}",
                    output_name(&self.output)
                ),
                Err(error) => log::debug!(
                    "Gamma controller for '{}' stopped before restoration: {error}",
                    output_name(&self.output)
                ),
            }
        }
        self.brightness.cancel().await;
        if let Some(gamma) = self.gamma {
            gamma.cancel().await;
        }
    }
}

async fn send_command(
    commands: &Sender<brightness::Command>,
    action: brightness::CommandAction,
) -> std::result::Result<u8, String> {
    let (response_tx, response_rx) = channel::bounded(1);
    commands
        .send(brightness::Command {
            action,
            response: response_tx,
        })
        .await
        .map_err(|_| "output stopped responding".to_string())?;
    response_rx
        .recv()
        .await
        .map_err(|_| "output stopped responding".to_string())?
}

fn output_name(output: &config::Output) -> &str {
    match output {
        config::Output::Backlight(output) => &output.name,
        config::Output::DdcUtil(output) => &output.name,
    }
}

fn is_keyboard(output: &config::Output) -> bool {
    matches!(output, config::Output::Backlight(output) if output.kind == config::BacklightKind::Keyboard)
}

fn log_discovered(output: &config::Output) {
    match output {
        config::Output::Backlight(output) if output.kind == config::BacklightKind::Keyboard => {
            log::debug!(
                "Discovered keyboard '{}' using backlight {}",
                output.name,
                output.path
            )
        }
        config::Output::Backlight(output) => log::debug!(
            "Discovered output '{}' using backlight {}",
            output.name,
            output.path
        ),
        config::Output::DdcUtil(output) => log::debug!(
            "Discovered output '{}' using DDC identifier '{}'",
            output.name,
            output.identifier
        ),
    }
}
