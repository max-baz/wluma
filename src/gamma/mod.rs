mod mutter;
pub mod ramp;
mod wayland;

use crate::transition::{IoTiming, Scalar};
use anyhow::{anyhow, Result};
use smol::channel::{Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const RESTORE_DURATION: Duration = Duration::from_millis(500);
const IDLE_INTERVAL: Duration = Duration::from_millis(100);
const WAYLAND_TRANSITION_INTERVAL: Duration = Duration::from_millis(20);
const MUTTER_TRANSITION_INTERVAL: Duration = Duration::from_millis(50);

pub enum Backend {
    Wayland(wayland::Backend),
    Mutter(mutter::Backend),
    #[cfg(test)]
    Mock(MockBackend),
}

#[cfg(test)]
pub struct MockBackend {
    initial: Values,
    writes: Arc<std::sync::Mutex<Vec<Values>>>,
    fail: Arc<AtomicBool>,
    preserved: Arc<AtomicBool>,
    gate: Option<Receiver<()>>,
    entered: Option<Sender<()>>,
}

pub enum BackendError {
    Unsupported(anyhow::Error),
    Rejected(anyhow::Error),
}

enum ConnectError {
    Backend(BackendError),
    Initialize(anyhow::Error),
}

impl Backend {
    pub fn new(output: &str) -> std::result::Result<Self, BackendError> {
        match wayland::Backend::new(output) {
            Ok(backend) => Ok(Self::Wayland(backend)),
            Err(wayland::Failure::Unavailable(wayland_error)) => {
                match mutter::Backend::new(output) {
                    Ok(backend) => Ok(Self::Mutter(backend)),
                    Err(mutter_error) => Err(BackendError::Unsupported(anyhow!(
                        "Wayland: {wayland_error:#}; Mutter: {mutter_error:#}"
                    ))),
                }
            }
            Err(wayland::Failure::Rejected(wayland_error)) => match mutter::Backend::new(output) {
                Ok(backend) => Ok(Self::Mutter(backend)),
                Err(_) => Err(BackendError::Rejected(wayland_error)),
            },
        }
    }

    fn initialize(&mut self) -> Result<Values> {
        match self {
            // Acquiring the Wayland protocol does not provide an existing LUT.
            Self::Wayland(backend) => {
                backend.set(0, ramp::NEUTRAL_TEMPERATURE)?;
                Ok(Values::neutral())
            }
            // Mutter's LUT is already active. Leaving it untouched also avoids
            // a flash before the first absolute transition begins.
            Self::Mutter(backend) => {
                let (dim, temperature) = backend.initial();
                Ok(Values { dim, temperature })
            }
            #[cfg(test)]
            Self::Mock(backend) => Ok(backend.initial),
        }
    }

    fn set(&mut self, dim: u64, temperature: u64) -> Result<()> {
        match self {
            Self::Wayland(backend) => backend.set(dim, temperature),
            Self::Mutter(backend) => backend.set(dim, temperature),
            #[cfg(test)]
            Self::Mock(backend) => {
                if let Some(entered) = backend.entered.take() {
                    entered.send_blocking(())?;
                }
                if let Some(gate) = backend.gate.take() {
                    gate.recv_blocking()?;
                }
                anyhow::ensure!(!backend.fail.load(Ordering::Relaxed), "Gamma write failed");
                backend
                    .writes
                    .lock()
                    .unwrap()
                    .push(Values { dim, temperature });
                Ok(())
            }
        }
    }

    fn preserve_current(&mut self) {
        if let Self::Mutter(backend) = self {
            backend.preserve_current();
        }
        #[cfg(test)]
        if let Self::Mock(backend) = self {
            backend.preserved.store(true, Ordering::Relaxed);
        }
    }

    fn transition_interval(&self) -> Duration {
        match self {
            Self::Wayland(_) => WAYLAND_TRANSITION_INTERVAL,
            Self::Mutter(_) => MUTTER_TRANSITION_INTERVAL,
            #[cfg(test)]
            Self::Mock(_) => WAYLAND_TRANSITION_INTERVAL,
        }
    }
}

pub struct Inputs {
    pub dim_user: Sender<u64>,
    pub temperature_user: Sender<u64>,
    pub dim_predictions: Receiver<u64>,
    pub temperature_predictions: Receiver<u64>,
    pub commands: Receiver<Command>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Values {
    dim: u64,
    temperature: u64,
}

pub struct Controller {
    backend: Option<Backend>,
    last_attempt: Option<Instant>,
    current: Values,
    target: Values,
    target_initialized: bool,
    dim: Scalar,
    temperature: Scalar,
    timing: IoTiming,
    dim_user: Sender<u64>,
    temperature_user: Sender<u64>,
    dim_predictions: Receiver<u64>,
    temperature_predictions: Receiver<u64>,
    commands: Receiver<Command>,
    paused: Arc<AtomicBool>,
    available: Arc<AtomicBool>,
    status: crate::control::Hub,
    output: String,
}

pub struct Command {
    pub action: CommandAction,
    pub response: Sender<Result<u64, String>>,
}

pub enum CommandAction {
    Get(crate::control::Property),
    Set(crate::control::Property, crate::control::ValueAdjustment),
    Restore,
}

impl Controller {
    pub fn new(
        inputs: Inputs,
        paused: Arc<AtomicBool>,
        available: Arc<AtomicBool>,
        status: crate::control::Hub,
        output: String,
    ) -> Self {
        let now = Instant::now();
        let timing = IoTiming::default();
        let tau = timing.tau(WAYLAND_TRANSITION_INTERVAL);
        Self {
            backend: None,
            last_attempt: None,
            current: Values::neutral(),
            target: Values::neutral(),
            target_initialized: false,
            dim: Scalar::new(0, 0, tau, now),
            temperature: Scalar::new(
                ramp::NEUTRAL_TEMPERATURE,
                ramp::NEUTRAL_TEMPERATURE,
                tau,
                now,
            ),
            timing,
            dim_user: inputs.dim_user,
            temperature_user: inputs.temperature_user,
            dim_predictions: inputs.dim_predictions,
            temperature_predictions: inputs.temperature_predictions,
            commands: inputs.commands,
            paused,
            available,
            status,
            output,
        }
    }

    pub async fn run(mut self) {
        let _ = self.dim_user.send(self.target.dim).await;
        let _ = self.temperature_user.send(self.target.temperature).await;
        loop {
            self.step().await;
        }
    }

    async fn step(&mut self) {
        self.connect().await;
        while let Ok(command) = self.commands.try_recv() {
            self.command(command).await;
        }
        let dim = last(&self.dim_predictions);
        let temperature = last(&self.temperature_predictions);
        if !self.paused.load(Ordering::Relaxed) && (dim.is_some() || temperature.is_some()) {
            self.set_target(
                Values {
                    dim: dim.unwrap_or(self.target.dim).min(100),
                    temperature: temperature
                        .unwrap_or(self.target.temperature)
                        .clamp(ramp::MIN_TEMPERATURE, ramp::MAX_TEMPERATURE),
                },
                Instant::now(),
            );
        }
        let cycle_started = Instant::now();
        let interval = if self.tracking() {
            self.backend
                .as_ref()
                .map_or(IDLE_INTERVAL, Backend::transition_interval)
        } else {
            IDLE_INTERVAL
        };
        if let Err(error) = self.advance(cycle_started).await {
            log::debug!("Gamma control failed for '{}': {error:#}", self.output);
        }
        self.wait_until(cycle_started + interval).await;
    }

    async fn connect(&mut self) {
        if self.backend.is_some()
            || self
                .last_attempt
                .is_some_and(|attempt| attempt.elapsed() < RETRY_INTERVAL)
        {
            return;
        }
        self.last_attempt = Some(Instant::now());
        let output = self.output.clone();
        let result = smol::unblock(move || {
            let mut backend = Backend::new(&output).map_err(ConnectError::Backend)?;
            let current = backend.initialize().map_err(ConnectError::Initialize)?;
            Ok((backend, current))
        })
        .await;
        match result {
            Ok((backend, current)) => {
                log::debug!("Acquired gamma control for '{}'", self.output);
                self.connected(backend, current, Instant::now());
            }
            Err(ConnectError::Initialize(error)) => {
                log::debug!(
                    "Unable to initialize gamma control for '{}': {error:#}",
                    self.output
                );
            }
            Err(ConnectError::Backend(BackendError::Unsupported(error))) => {
                log::debug!(
                    "No gamma control backend is available for '{}': {error:#}",
                    self.output
                );
            }
            Err(ConnectError::Backend(BackendError::Rejected(error))) => {
                log::debug!(
                    "Gamma control was rejected for '{}'; another client may own it or the output may not support gamma ramps: {error:#}",
                    self.output
                );
            }
        }
    }

    async fn command(&mut self, command: Command) {
        let result = self
            .execute(command.action)
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = command.response.send(result).await;
    }

    async fn execute(&mut self, action: CommandAction) -> Result<u64> {
        if self.backend.is_none() {
            return Err(anyhow!(
                "gamma control is unavailable for '{}'",
                self.output
            ));
        }
        match action {
            CommandAction::Get(property) => Ok(self.value(property)),
            CommandAction::Set(property, adjustment) => {
                let current = self.value(property);
                let (min, max) = match property {
                    crate::control::Property::Dim => (0, 100),
                    crate::control::Property::Temperature => {
                        (ramp::MIN_TEMPERATURE, ramp::MAX_TEMPERATURE)
                    }
                    crate::control::Property::Brightness => unreachable!(),
                };
                let value = if adjustment.relative {
                    if adjustment.increase {
                        current.saturating_add(adjustment.value)
                    } else {
                        current.saturating_sub(adjustment.value)
                    }
                } else {
                    adjustment.value
                }
                .clamp(min, max);
                let mut target = self.target;
                match property {
                    crate::control::Property::Dim => {
                        last(&self.dim_predictions);
                        target.dim = value;
                    }
                    crate::control::Property::Temperature => {
                        last(&self.temperature_predictions);
                        target.temperature = value;
                    }
                    crate::control::Property::Brightness => unreachable!(),
                }
                self.set_target(target, Instant::now());
                if !self.paused.load(Ordering::Relaxed) {
                    match property {
                        crate::control::Property::Dim => self.dim_user.send(value).await?,
                        crate::control::Property::Temperature => {
                            self.temperature_user.send(value).await?
                        }
                        crate::control::Property::Brightness => unreachable!(),
                    }
                }
                Ok(value)
            }
            CommandAction::Restore => {
                let from = self.current;
                self.target = Values::neutral();
                self.target_initialized = true;
                let started = Instant::now();
                while self.current != self.target {
                    let cycle_started = Instant::now();
                    let progress = cycle_started.duration_since(started).as_secs_f64()
                        / RESTORE_DURATION.as_secs_f64();
                    if progress >= 1.0 {
                        break;
                    }
                    let progress = progress * progress * (3.0 - 2.0 * progress);
                    let next = Values {
                        dim: interpolate(from.dim, self.target.dim, progress),
                        temperature: interpolate(
                            from.temperature,
                            self.target.temperature,
                            progress,
                        ),
                    };
                    if next != self.current {
                        self.apply(next).await?;
                    }
                    let interval = self
                        .backend
                        .as_ref()
                        .map_or(IDLE_INTERVAL, Backend::transition_interval);
                    smol::Timer::at(cycle_started + interval).await;
                }
                // Always write the neutral LUT. An estimated initial value can
                // round to neutral even when its channel gains are slightly off.
                self.apply(Values::neutral()).await?;
                last(&self.dim_predictions);
                last(&self.temperature_predictions);
                self.reset_tracking(Instant::now());
                if let Some(backend) = &mut self.backend {
                    backend.preserve_current();
                }
                Ok(ramp::NEUTRAL_TEMPERATURE)
            }
        }
    }

    fn value(&self, property: crate::control::Property) -> u64 {
        match property {
            crate::control::Property::Dim => self.target.dim,
            crate::control::Property::Temperature => self.target.temperature,
            crate::control::Property::Brightness => unreachable!(),
        }
    }

    fn connected(&mut self, backend: Backend, current: Values, now: Instant) {
        self.current = current;
        self.backend = Some(backend);
        self.timing = IoTiming::default();
        self.available.store(true, Ordering::Relaxed);
        // Until a command or prediction provides a target, retain the
        // LUT that Mutter already has instead of flashing to neutral.
        if !self.target_initialized {
            self.target = current;
            self.target_initialized = true;
        }
        self.reset_tracking(now);
        self.update_status();
    }

    fn reset_tracking(&mut self, now: Instant) {
        let interval = self
            .backend
            .as_ref()
            .map_or(IDLE_INTERVAL, Backend::transition_interval);
        let tau = self.timing.tau(interval);
        self.dim = Scalar::new(self.current.dim, self.target.dim, tau, now);
        self.temperature = Scalar::new(self.current.temperature, self.target.temperature, tau, now);
    }

    async fn wait_until(&mut self, mut deadline: Instant) {
        let commands = self.commands.clone();
        while Instant::now() < deadline {
            let command = smol::future::race(
                async {
                    if let Ok(command) = commands.recv().await {
                        Some(command)
                    } else {
                        smol::Timer::at(deadline).await;
                        None
                    }
                },
                async {
                    smol::Timer::at(deadline).await;
                    None
                },
            )
            .await;
            if let Some(command) = command {
                self.command(command).await;
                if self.tracking() {
                    let interval = self.backend.as_ref().unwrap().transition_interval();
                    deadline = deadline.min(Instant::now() + interval);
                }
            } else {
                break;
            }
        }
    }

    fn set_target(&mut self, target: Values, now: Instant) {
        self.target_initialized = true;
        self.target = target;
        self.dim.retarget(target.dim, now);
        self.temperature.retarget(target.temperature, now);
    }

    fn tracking(&self) -> bool {
        self.backend.is_some()
            && (!self.dim.settled() || !self.temperature.settled() || self.current != self.target)
    }

    async fn advance(&mut self, now: Instant) -> Result<()> {
        if !self.tracking() {
            return Ok(());
        }
        let next = Values {
            dim: self.dim.advance(now),
            temperature: self.temperature.advance(now),
        };
        if next != self.current {
            self.apply(next).await?;
        }
        Ok(())
    }

    async fn apply(&mut self, values: Values) -> Result<()> {
        let mut backend = self
            .backend
            .take()
            .ok_or_else(|| anyhow!("gamma control is unavailable"))?;
        let started = Instant::now();
        let (result, backend) =
            smol::unblock(move || (backend.set(values.dim, values.temperature), backend)).await;
        let elapsed = started.elapsed();
        if let Err(error) = result {
            self.available.store(false, Ordering::Relaxed);
            self.current = Values::neutral();
            self.reset_tracking(Instant::now());
            self.status.clear_gamma(&self.output);
            return Err(error);
        }
        self.backend = Some(backend);
        self.current = values;
        self.timing.record(elapsed);
        let tau = self
            .timing
            .tau(self.backend.as_ref().unwrap().transition_interval());
        let now = Instant::now();
        self.dim.set_tau(tau, now);
        self.temperature.set_tau(tau, now);
        self.update_status();
        Ok(())
    }

    fn update_status(&self) {
        self.status.set_gamma(
            &self.output,
            self.current.dim as u8,
            self.current.temperature as u32,
        );
    }
}

impl Values {
    fn neutral() -> Self {
        Self {
            dim: 0,
            temperature: ramp::NEUTRAL_TEMPERATURE,
        }
    }
}

fn interpolate(from: u64, to: u64, progress: f64) -> u64 {
    (from as f64 + (to as f64 - from as f64) * progress).round() as u64
}

fn last(receiver: &Receiver<u64>) -> Option<u64> {
    let mut value = None;
    while let Ok(next) = receiver.try_recv() {
        value = Some(next);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Property, ValueAdjustment};
    use macro_rules_attribute::apply;
    use smol::{channel, future::poll_once};
    use smol_macros::test;

    fn backend(initial: Values) -> Backend {
        Backend::Mock(MockBackend {
            initial,
            writes: Arc::default(),
            fail: Arc::default(),
            preserved: Arc::default(),
            gate: None,
            entered: None,
        })
    }

    fn setup() -> (Controller, Sender<Command>) {
        let (commands_tx, commands) = channel::bounded(32);
        let (dim_user, _) = channel::bounded(128);
        let (temperature_user, _) = channel::bounded(128);
        let (_, dim_predictions) = channel::bounded(128);
        let (_, temperature_predictions) = channel::bounded(128);
        let mut controller = Controller::new(
            Inputs {
                dim_user,
                temperature_user,
                dim_predictions,
                temperature_predictions,
                commands,
            },
            Arc::default(),
            Arc::default(),
            crate::control::Hub::new("test"),
            "test".into(),
        );
        controller.connected(
            backend(Values::neutral()),
            Values::neutral(),
            Instant::now(),
        );
        (controller, commands_tx)
    }

    fn writes(controller: &Controller) -> Arc<std::sync::Mutex<Vec<Values>>> {
        let Some(Backend::Mock(backend)) = &controller.backend else {
            panic!("Expected mock backend")
        };
        backend.writes.clone()
    }

    #[test]
    fn changed_properties_do_not_restart_each_other() {
        for dim_changes in [true, false] {
            let (mut controller, _) = setup();
            let now = Instant::now();
            let target = Values {
                dim: 80,
                temperature: 4500,
            };
            controller.set_target(target, now);
            let at = now + Duration::from_millis(100);
            controller.dim.advance(at);
            controller.temperature.advance(at);
            let dim = controller.dim;
            let temperature = controller.temperature;
            let next = if dim_changes {
                Values { dim: 20, ..target }
            } else {
                Values {
                    temperature: 8000,
                    ..target
                }
            };
            controller.set_target(next, at);
            assert_eq!(dim.output(), controller.dim.output());
            assert_eq!(temperature.output(), controller.temperature.output());
            if dim_changes {
                assert_eq!(temperature, controller.temperature);
                assert_eq!(20, controller.dim.destination());
            } else {
                assert_eq!(dim, controller.dim);
                assert_eq!(8000, controller.temperature.destination());
            }
            let dim = controller.dim;
            let temperature = controller.temperature;
            controller.set_target(next, at + Duration::from_secs(1));
            assert_eq!(dim, controller.dim);
            assert_eq!(temperature, controller.temperature);
        }
    }

    #[apply(test!)]
    async fn duplicates_preserve_fractional_progress_and_endpoints() -> Result<()> {
        for (from, to) in [(0, 1), (1, 0)] {
            let (mut controller, _) = setup();
            let now = Instant::now();
            controller.current.dim = from;
            controller.target.dim = from;
            controller.reset_tracking(now);
            controller.set_target(
                Values {
                    dim: to,
                    ..Values::neutral()
                },
                now,
            );
            let writes = writes(&controller);
            for ms in (10..=130).step_by(10) {
                let at = now + Duration::from_millis(ms);
                controller.set_target(controller.target, at);
                controller.advance(at).await?;
                assert_eq!(from, controller.current.dim);
                assert!(controller.tracking());
                assert!(writes.lock().unwrap().is_empty());
                assert_eq!(IoTiming::default(), controller.timing);
            }
            controller.advance(now + Duration::from_millis(140)).await?;
            assert_eq!(to, controller.current.dim);
            assert!(!controller.tracking());
            controller.advance(now + Duration::from_secs(10)).await?;
            assert_eq!(1, writes.lock().unwrap().len());
        }
        Ok(())
    }

    #[apply(test!)]
    async fn failed_final_write_drops_backend_and_reconnect_starts_from_initial() -> Result<()> {
        let (mut controller, _) = setup();
        let now = Instant::now();
        let target = Values {
            dim: 80,
            temperature: 4500,
        };
        controller.set_target(target, now);
        let Some(Backend::Mock(mock)) = &controller.backend else {
            unreachable!()
        };
        let fail = mock.fail.clone();
        let old_writes = mock.writes.clone();
        fail.store(true, Ordering::Relaxed);
        assert!(controller
            .advance(now + Duration::from_secs(10))
            .await
            .is_err());
        assert!(controller.backend.is_none());
        assert!(!controller.available.load(Ordering::Relaxed));
        assert_eq!(Values::neutral(), controller.current);
        assert_eq!(target, controller.target);
        assert_eq!(IoTiming::default(), controller.timing);
        assert!(old_writes.lock().unwrap().is_empty());
        let initial = Values {
            dim: 30,
            temperature: 6000,
        };
        let mut backend = backend(initial);
        let observed = backend.initialize()?;
        let reconnected = now + Duration::from_secs(60);
        controller.connected(backend, observed, reconnected);
        controller.advance(reconnected).await?;
        assert_eq!(initial, controller.current);
        assert!(writes(&controller).lock().unwrap().is_empty());
        assert!(controller.available.load(Ordering::Relaxed));
        controller
            .advance(reconnected + Duration::from_secs(10))
            .await?;
        assert_eq!(target, controller.current);
        assert!(!controller.tracking());
        Ok(())
    }

    #[apply(test!)]
    async fn initial_lut_is_preserved_until_a_target_exists() -> Result<()> {
        let (mut controller, _) = setup();
        controller.target_initialized = false;
        let initial = Values {
            dim: 23,
            temperature: 4321,
        };
        let now = Instant::now();
        controller.connected(backend(initial), initial, now);
        controller.advance(now + Duration::from_secs(60)).await?;
        assert_eq!(initial, controller.current);
        assert_eq!(initial, controller.target);
        assert!(!controller.tracking());
        assert!(writes(&controller).lock().unwrap().is_empty());
        Ok(())
    }

    #[apply(test!)]
    async fn manual_commands_use_targets_and_remain_smooth_while_paused() -> Result<()> {
        let (mut controller, _) = setup();
        controller.paused.store(true, Ordering::Relaxed);
        let (predictions, rx) = channel::bounded(8);
        controller.dim_predictions = rx;
        predictions.send(99).await?;
        for expected in [10, 20] {
            assert_eq!(
                expected,
                controller
                    .execute(CommandAction::Set(
                        Property::Dim,
                        ValueAdjustment {
                            value: 10,
                            relative: true,
                            increase: true
                        },
                    ))
                    .await?
            );
            assert_eq!(0, controller.current.dim);
            assert_eq!(expected, controller.value(Property::Dim));
            assert!(writes(&controller).lock().unwrap().is_empty());
        }
        assert!(controller.dim_predictions.is_empty());
        controller
            .advance(Instant::now() + Duration::from_secs(2))
            .await?;
        assert_eq!(20, controller.current.dim);
        Ok(())
    }

    #[apply(test!)]
    async fn waits_handle_commands_and_start_tracking_without_an_idle_delay() -> Result<()> {
        let (mut controller, commands) = setup();
        controller.paused.store(true, Ordering::Relaxed);
        let writes = writes(&controller);
        let mut wait = Box::pin(controller.wait_until(Instant::now() + Duration::from_secs(60)));
        assert!(poll_once(wait.as_mut()).await.is_none());
        let (response, rx) = channel::bounded(1);
        commands
            .send(Command {
                action: CommandAction::Get(Property::Temperature),
                response,
            })
            .await?;
        assert!(poll_once(wait.as_mut()).await.is_none());
        assert_eq!(Ok(ramp::NEUTRAL_TEMPERATURE), rx.try_recv()?);
        let (response, rx) = channel::bounded(1);
        commands
            .send(Command {
                action: CommandAction::Set(
                    Property::Dim,
                    ValueAdjustment {
                        value: 80,
                        relative: false,
                        increase: true,
                    },
                ),
                response,
            })
            .await?;
        assert!(poll_once(wait.as_mut()).await.is_none());
        assert_eq!(Ok(80), rx.try_recv()?);
        assert!(writes.lock().unwrap().is_empty());
        smol::Timer::after(WAYLAND_TRANSITION_INTERVAL + Duration::from_millis(10)).await;
        assert!(poll_once(wait.as_mut()).await.is_some());
        drop(wait);
        let mut elapsed = Box::pin(controller.wait_until(Instant::now() - Duration::from_secs(1)));
        assert!(poll_once(elapsed.as_mut()).await.is_some());
        Ok(())
    }

    #[apply(test!)]
    async fn slow_io_keeps_backend_owned_and_only_work_trains_timing() -> Result<()> {
        let (mut controller, commands) = setup();
        let now = Instant::now();
        controller.set_target(
            Values {
                dim: 80,
                ..Values::neutral()
            },
            now,
        );
        controller.reset_tracking(now - Duration::from_secs(10));
        let writes = writes(&controller);
        let (release, gate) = channel::bounded(1);
        let (entered_tx, entered) = channel::bounded(1);
        let Some(Backend::Mock(mock)) = &mut controller.backend else {
            unreachable!()
        };
        mock.gate = Some(gate);
        mock.entered = Some(entered_tx);
        let mut step = Box::pin(controller.step());
        smol::future::race(
            async {
                step.as_mut().await;
                anyhow::bail!("Step finished before its backend write")
            },
            async {
                entered.recv().await?;
                Ok(())
            },
        )
        .await?;
        let (response, rx) = channel::bounded(1);
        commands
            .send(Command {
                action: CommandAction::Get(Property::Dim),
                response,
            })
            .await?;
        assert!(poll_once(step.as_mut()).await.is_none());
        assert!(rx.is_empty());
        let blocked = Instant::now();
        smol::Timer::after(Duration::from_millis(30)).await;
        let blocked = blocked.elapsed();
        release.send(()).await?;
        step.await;
        assert_eq!(80, controller.current.dim);
        assert_eq!(1, writes.lock().unwrap().len());
        assert!(controller.backend.is_some());
        assert!(
            controller.timing.tau(WAYLAND_TRANSITION_INTERVAL)
                >= (blocked * 8).min(Duration::from_secs(1))
        );
        controller.command(controller.commands.try_recv()?).await;
        assert_eq!(Ok(80), rx.try_recv()?);
        let timing = controller.timing;
        controller
            .wait_until(Instant::now() + Duration::from_millis(20))
            .await;
        assert_eq!(timing, controller.timing);
        Ok(())
    }

    #[apply(test!)]
    async fn restore_always_writes_neutral_and_preserves_only_after_success() -> Result<()> {
        for fail in [false, true] {
            let (mut controller, _) = setup();
            let Some(Backend::Mock(mock)) = &controller.backend else {
                unreachable!()
            };
            let writes = mock.writes.clone();
            let preserved = mock.preserved.clone();
            mock.fail.store(fail, Ordering::Relaxed);
            let result = controller.execute(CommandAction::Restore).await;
            assert_eq!(fail, result.is_err());
            assert_eq!(!fail, preserved.load(Ordering::Relaxed));
            if fail {
                assert!(writes.lock().unwrap().is_empty());
                assert!(controller.backend.is_none());
            } else {
                assert_eq!([Values::neutral()], writes.lock().unwrap().as_slice());
                assert_eq!(Values::neutral(), controller.current);
                assert!(!controller.tracking());
            }
        }
        Ok(())
    }

    #[apply(test!)]
    async fn restore_discards_predictions_queued_before_neutral_was_applied() -> Result<()> {
        let (mut controller, _) = setup();
        let (dim, dim_rx) = channel::bounded(8);
        let (temperature, temperature_rx) = channel::bounded(8);
        controller.dim_predictions = dim_rx;
        controller.temperature_predictions = temperature_rx;
        dim.send(80).await?;
        temperature.send(25000).await?;
        controller.execute(CommandAction::Restore).await?;
        assert!(controller.dim_predictions.is_empty());
        assert!(controller.temperature_predictions.is_empty());
        assert_eq!(Values::neutral(), controller.target);
        assert!(!controller.tracking());
        Ok(())
    }

    #[apply(test!)]
    async fn restore_uses_acknowledged_values_and_not_the_slow_automatic_tau() -> Result<()> {
        let (mut controller, _) = setup();
        let from = Values {
            dim: 100,
            temperature: ramp::MAX_TEMPERATURE,
        };
        controller.current = from;
        controller.target = Values {
            dim: 0,
            temperature: ramp::MIN_TEMPERATURE,
        };
        controller.timing.record(Duration::from_secs(1));
        let now = Instant::now();
        controller.reset_tracking(now - Duration::from_secs(60));
        controller.dim.advance(now);
        controller.temperature.advance(now);
        assert_ne!(
            controller.current.temperature,
            controller.temperature.output()
        );
        let writes = writes(&controller);
        let (release, gate) = channel::bounded(1);
        let (entered_tx, entered) = channel::bounded(1);
        let Some(Backend::Mock(mock)) = &mut controller.backend else {
            unreachable!()
        };
        mock.gate = Some(gate);
        mock.entered = Some(entered_tx);
        let mut restore = Box::pin(controller.execute(CommandAction::Restore));
        smol::future::race(
            async {
                restore.as_mut().await?;
                anyhow::bail!("Restore finished before its first backend write")
            },
            async {
                entered.recv().await?;
                Ok(())
            },
        )
        .await?;
        smol::Timer::after(RESTORE_DURATION + Duration::from_millis(20)).await;
        release.send(()).await?;
        restore.await?;
        let writes = writes.lock().unwrap();
        assert!(writes.len() <= 2);
        assert_eq!(Some(&Values::neutral()), writes.last());
        assert!(writes.first().unwrap().temperature > ramp::NEUTRAL_TEMPERATURE);
        assert_eq!(Values::neutral(), controller.current);
        Ok(())
    }

    #[test]
    fn interpolates_gamma_values_in_both_directions() {
        assert_eq!(interpolate(0, 100, 0.0), 0);
        assert_eq!(interpolate(0, 100, 0.5), 50);
        assert_eq!(interpolate(0, 100, 1.0), 100);
        assert_eq!(interpolate(6500, 4000, 0.5), 5250);
        assert_eq!(interpolate(6500, 4000, 1.0), 4000);
    }
}
