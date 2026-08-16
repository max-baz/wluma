mod mutter;
pub mod ramp;
mod wayland;

use anyhow::{anyhow, Result};
use smol::channel::{Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const TRANSITION_DURATION: Duration = Duration::from_millis(500);
const IDLE_INTERVAL: Duration = Duration::from_millis(100);
const WAYLAND_TRANSITION_INTERVAL: Duration = Duration::from_millis(20);
const MUTTER_TRANSITION_INTERVAL: Duration = Duration::from_millis(50);

pub enum Backend {
    Wayland(wayland::Backend),
    Mutter(mutter::Backend),
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

    fn set(&mut self, dim: u64, temperature: u64) -> Result<()> {
        match self {
            Self::Wayland(backend) => backend.set(dim, temperature),
            Self::Mutter(backend) => backend.set(dim, temperature),
        }
    }

    fn transition_interval(&self) -> Duration {
        match self {
            Self::Wayland(_) => WAYLAND_TRANSITION_INTERVAL,
            Self::Mutter(_) => MUTTER_TRANSITION_INTERVAL,
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct Values {
    dim: u64,
    temperature: u64,
}

struct Transition {
    from: Values,
    started: Instant,
}

pub struct Controller {
    backend: Option<Backend>,
    last_attempt: Option<Instant>,
    current: Values,
    target: Values,
    transition: Option<Transition>,
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
}

impl Controller {
    pub fn new(
        inputs: Inputs,
        paused: Arc<AtomicBool>,
        available: Arc<AtomicBool>,
        status: crate::control::Hub,
        output: String,
    ) -> Self {
        Self {
            backend: None,
            last_attempt: None,
            current: Values::neutral(),
            target: Values::neutral(),
            transition: None,
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
            self.connect().await;
            while let Ok(command) = self.commands.try_recv() {
                self.command(command).await;
            }
            let dim = last(&self.dim_predictions);
            let temperature = last(&self.temperature_predictions);
            if !self.paused.load(Ordering::Relaxed) && (dim.is_some() || temperature.is_some()) {
                self.set_target(Values {
                    dim: dim.unwrap_or(self.target.dim).min(100),
                    temperature: temperature
                        .unwrap_or(self.target.temperature)
                        .clamp(ramp::MIN_TEMPERATURE, ramp::MAX_TEMPERATURE),
                });
            }
            if let Err(error) = self.advance().await {
                log::debug!("Gamma control failed for '{}': {error:#}", self.output);
            }
            let interval = if self.transition.is_some() {
                self.backend
                    .as_ref()
                    .map_or(IDLE_INTERVAL, Backend::transition_interval)
            } else {
                IDLE_INTERVAL
            };
            smol::Timer::after(interval).await;
        }
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
        let current = self.current;
        let result = smol::unblock(move || {
            let mut backend = Backend::new(&output).map_err(ConnectError::Backend)?;
            backend
                .set(current.dim, current.temperature)
                .map_err(ConnectError::Initialize)?;
            Ok(backend)
        })
        .await;
        match result {
            Ok(backend) => {
                log::debug!("Acquired gamma control for '{}'", self.output);
                self.backend = Some(backend);
                self.available.store(true, Ordering::Relaxed);
                if self.current != self.target {
                    self.transition = Some(Transition {
                        from: self.current,
                        started: Instant::now(),
                    });
                }
                self.update_status();
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
                self.set_target(target);
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
        }
    }

    fn value(&self, property: crate::control::Property) -> u64 {
        match property {
            crate::control::Property::Dim => self.target.dim,
            crate::control::Property::Temperature => self.target.temperature,
            crate::control::Property::Brightness => unreachable!(),
        }
    }

    fn set_target(&mut self, target: Values) {
        if target == self.target {
            return;
        }
        self.target = target;
        self.transition = (self.backend.is_some() && self.current != target).then(|| Transition {
            from: self.current,
            started: Instant::now(),
        });
    }

    async fn advance(&mut self) -> Result<()> {
        let Some(transition) = &self.transition else {
            return Ok(());
        };
        let progress = (transition.started.elapsed().as_secs_f64()
            / TRANSITION_DURATION.as_secs_f64())
        .min(1.0);
        let progress = progress * progress * (3.0 - 2.0 * progress);
        let next = Values {
            dim: interpolate(transition.from.dim, self.target.dim, progress),
            temperature: interpolate(
                transition.from.temperature,
                self.target.temperature,
                progress,
            ),
        };
        if next != self.current {
            self.apply(next).await?;
        }
        if progress >= 1.0 {
            self.current = self.target;
            self.transition = None;
        }
        Ok(())
    }

    async fn apply(&mut self, values: Values) -> Result<()> {
        let mut backend = self
            .backend
            .take()
            .ok_or_else(|| anyhow!("gamma control is unavailable"))?;
        let (result, backend) =
            smol::unblock(move || (backend.set(values.dim, values.temperature), backend)).await;
        if let Err(error) = result {
            self.available.store(false, Ordering::Relaxed);
            self.current = Values::neutral();
            self.transition = None;
            self.status.clear_gamma(&self.output);
            return Err(error);
        }
        self.backend = Some(backend);
        self.current = values;
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
    use super::interpolate;

    #[test]
    fn interpolates_gamma_values_in_both_directions() {
        assert_eq!(interpolate(0, 100, 0.0), 0);
        assert_eq!(interpolate(0, 100, 0.5), 50);
        assert_eq!(interpolate(0, 100, 1.0), 100);
        assert_eq!(interpolate(6500, 4000, 0.5), 5250);
        assert_eq!(interpolate(6500, 4000, 1.0), 4000);
    }
}
