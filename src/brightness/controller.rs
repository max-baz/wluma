use smol::channel::{Receiver, Sender};
use smol::Timer;

use crate::channel_ext::ReceiverExt;
use crate::transition::{IoTiming, Scalar};

use super::{BackendChanged, Brightness, Lifetime};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TRANSITION_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone, Default)]
pub struct PauseState(Arc<Mutex<Option<Option<Instant>>>>);

impl PauseState {
    fn current(&self) -> Option<Option<Instant>> {
        let mut state = self.0.lock().unwrap();
        if state
            .flatten()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            *state = None;
        }
        *state
    }

    pub fn is_paused(&self) -> bool {
        self.current().is_some()
    }
}

pub struct Controller {
    brightness: Brightness,
    lifetime: Lifetime,
    pause_state: PauseState,
    ready: Arc<AtomicBool>,
    user_tx: Sender<u64>,
    prediction_rx: Receiver<u64>,
    prediction: Option<u64>,
    current: Option<u64>,
    target: Option<Scalar>,
    timing: IoTiming,
    io_failed: bool,
    commands: Option<Receiver<Command>>,
    status: Option<(crate::control::Hub, String)>,
    paused_until: Option<Option<Instant>>,
    idle: Option<u8>,
    paused: Option<Arc<AtomicBool>>,
    resuming: bool,
}

pub struct Command {
    pub action: CommandAction,
    pub response: Sender<Result<u8, String>>,
}

pub enum CommandAction {
    Get,
    Set(crate::control::Adjustment),
    Pause(Option<Duration>),
    Resume,
    Toggle,
    IdleEnter(u8),
    IdleLeave,
}

impl Controller {
    pub fn new(brightness: Brightness, user_tx: Sender<u64>, prediction_rx: Receiver<u64>) -> Self {
        Self {
            brightness,
            lifetime: Lifetime::default(),
            pause_state: PauseState::default(),
            ready: Arc::new(AtomicBool::new(false)),
            user_tx,
            prediction_rx,
            prediction: None,
            current: None,
            target: None,
            timing: IoTiming::default(),
            io_failed: false,
            commands: None,
            status: None,
            paused_until: None,
            idle: None,
            paused: None,
            resuming: false,
        }
    }

    pub fn with_control(
        mut self,
        commands: Receiver<Command>,
        status: crate::control::Hub,
        output: String,
        paused: Arc<AtomicBool>,
    ) -> Self {
        self.commands = Some(commands);
        self.status = Some((status, output));
        self.paused = Some(paused);
        self
    }

    pub fn ready(&self) -> Arc<AtomicBool> {
        self.ready.clone()
    }

    pub fn with_lifetime(mut self, lifetime: Lifetime, pause_state: PauseState) -> Self {
        self.lifetime = lifetime;
        self.paused_until = pause_state.current();
        self.pause_state = pause_state;
        self.pause_state_changed(false);
        self
    }

    fn checked<T>(&self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        if let Some(error) = result
            .as_ref()
            .err()
            .filter(|error| error.is::<BackendChanged>())
        {
            log::info!(
                "[{}] Brightness backend changed: {error:#}",
                self.output_name()
            );
            self.lifetime.invalidate();
        }
        result
    }

    async fn read_brightness(&mut self) -> anyhow::Result<u64> {
        anyhow::ensure!(
            self.lifetime.is_valid(),
            "Brightness backend is being replaced"
        );
        let result = self.brightness.get().await;
        let value = self.checked(result)?;
        anyhow::ensure!(
            self.lifetime.is_valid(),
            "Brightness backend is being replaced"
        );
        Ok(value)
    }

    async fn write_brightness(&mut self, value: u64) -> anyhow::Result<u64> {
        anyhow::ensure!(
            self.lifetime.is_valid(),
            "Brightness backend is being replaced"
        );
        let result = self.brightness.set(value).await;
        let value = self.checked(result)?;
        anyhow::ensure!(
            self.lifetime.is_valid(),
            "Brightness backend is being replaced"
        );
        Ok(value)
    }

    pub async fn run(&mut self) {
        while self.lifetime.is_valid() {
            self.step().await;
        }
    }

    async fn step(&mut self) {
        if self.is_paused() && self.lifetime.is_valid() {
            if let Err(error) = self.checked(self.brightness.validate()) {
                log::debug!("Brightness backend needs replacement: {error:#}");
            }
        }
        self.expire_pause().await;
        while let Some(command) = self
            .commands
            .as_ref()
            .and_then(|commands| commands.try_recv().ok())
        {
            self.command(command).await;
        }
        if self.is_paused() || !self.lifetime.is_valid() {
            self.wait().await;
            return;
        }
        let cycle_started = Instant::now();
        let observed = self.read_brightness().await;
        let read_elapsed = cycle_started.elapsed();
        match observed {
            Ok(new_brightness) => {
                let Ok(predicted_value) = self.prediction_rx.recv_maybe_last().await else {
                    self.lifetime.invalidate();
                    return;
                };

                // 1. check if user wants to learn a new value - this overrides any ongoing activity
                if self.resuming {
                    self.resuming = false;
                    let initializing = self.current.is_none();
                    self.current = Some(new_brightness);
                    self.target = None;
                    if initializing {
                        if self.user_tx.send(new_brightness).await.is_err() {
                            self.lifetime.invalidate();
                            return;
                        }
                        self.ready.store(true, Ordering::Release);
                    }
                } else if Some(new_brightness) != self.current {
                    return self.update_current(new_brightness).await;
                }

                if self.io_failed {
                    self.rebase_target(Instant::now());
                    self.io_failed = false;
                }
                if let Some((status, output)) = &self.status {
                    status.set_brightness(output, self.brightness.percent(new_brightness));
                }

                // 2. check if predictor wants to set a new value
                if let Some(prediction) = predicted_value {
                    self.prediction = Some(prediction);
                    self.update_target(self.apply_idle_reduction(prediction), Instant::now());
                } else if let Some(target) = self.target {
                    self.update_target(target.destination(), Instant::now());
                }

                // 3. continue the transition if there is one in progress
                if self.target.is_some() {
                    self.transition(read_elapsed, Instant::now()).await;
                    self.wait_until(cycle_started + TRANSITION_INTERVAL).await;
                    return;
                }
            }
            Err(err) => {
                self.io_failed = true;
                self.rebase_target(Instant::now());
                log::error!("Unable to get brightness value: {:?}", err);
            }
        };

        // 4. nothing to do, sleep and check again
        // TODO: replace with inotify events on brightness device file and avoid sleep loop
        self.wait().await;
    }

    async fn wait(&mut self) {
        let delay = Duration::from_millis(self.brightness.waiting_sleep_ms());
        self.wait_until(Instant::now() + delay).await;
    }

    async fn wait_until(&mut self, mut deadline: Instant) {
        if let Some(commands) = self.commands.clone() {
            while Instant::now() < deadline {
                let command = smol::future::race(
                    async {
                        if let Ok(command) = commands.recv().await {
                            Some(command)
                        } else {
                            Timer::at(deadline).await;
                            None
                        }
                    },
                    async {
                        Timer::at(deadline).await;
                        None
                    },
                )
                .await;
                if let Some(command) = command {
                    self.command(command).await;
                    if !self.is_paused() && (self.resuming || self.target.is_some()) {
                        deadline = deadline.min(Instant::now() + TRANSITION_INTERVAL);
                    }
                } else {
                    break;
                }
            }
        } else {
            Timer::at(deadline).await;
        }
    }

    async fn update_current(&mut self, new_brightness: u64) {
        self.current = Some(new_brightness);
        if let Some((status, output)) = &self.status {
            status.set_brightness(output, self.brightness.percent(new_brightness));
        }
        if !self.is_paused() {
            if let Err(error) = self.learn(new_brightness).await {
                log::debug!("Unable to publish brightness adjustment: {error:#}");
                self.lifetime.invalidate();
            }
        }
        self.target = None;
    }

    async fn learn(&mut self, value: u64) -> anyhow::Result<()> {
        if !self.lifetime.is_valid() {
            return Ok(());
        }
        if self.idle.is_some() {
            log::warn!(
                "[{}] Ignoring brightness adjustment for learning while idle",
                self.output_name()
            );
            return Ok(());
        }
        self.prediction = Some(value);
        self.user_tx.send(value).await?;
        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    async fn command(&mut self, command: Command) {
        let result = self
            .execute(command.action)
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = command.response.send(result).await;
    }

    async fn execute(&mut self, action: CommandAction) -> anyhow::Result<u8> {
        let mut result = None;
        match action {
            CommandAction::Get => {
                let value = self.read_brightness().await?;
                if !self.is_paused() && Some(value) != self.current {
                    self.update_current(value).await;
                }
                result = Some(value);
            }
            CommandAction::Set(adjustment) => {
                let current = if self.is_paused() {
                    self.read_brightness().await?
                } else if let Some(current) = self.current {
                    current
                } else {
                    let current = self.read_brightness().await?;
                    self.update_current(current).await;
                    current
                };
                let current_percent = self.brightness.percent(current);
                let percent = if adjustment.relative {
                    if adjustment.increase {
                        current_percent.saturating_add(adjustment.percent).min(100)
                    } else {
                        current_percent.saturating_sub(adjustment.percent)
                    }
                } else {
                    adjustment.percent
                };
                let value = self.brightness.value_at_percent(percent);
                let value = self.write_brightness(value).await?;
                result = Some(value);
                if !self.is_paused() {
                    let _ = self.prediction_rx.recv_maybe_last().await?;
                    self.current = Some(value);
                    self.target = None;
                    if let Some((status, output)) = &self.status {
                        status.set_brightness(output, self.brightness.percent(value));
                    }
                    self.learn(value).await?;
                }
            }
            CommandAction::Pause(duration) => {
                let deadline = duration
                    .map(|duration| {
                        Instant::now()
                            .checked_add(duration)
                            .ok_or_else(|| anyhow::anyhow!("pause duration is too large"))
                    })
                    .transpose()?;
                self.set_manual_paused(Some(deadline));
                match duration {
                    Some(duration) => log::debug!(
                        "[{}] Paused automatic brightness adjustment for {}s",
                        self.output_name(),
                        duration.as_secs()
                    ),
                    None => log::debug!(
                        "[{}] Paused automatic brightness adjustment",
                        self.output_name()
                    ),
                }
            }
            CommandAction::Resume => {
                self.resume_manual_pause().await?;
                log::debug!(
                    "[{}] Resumed automatic brightness adjustment",
                    self.output_name()
                );
            }
            CommandAction::Toggle => {
                if self.paused_until.is_some() {
                    self.resume_manual_pause().await?;
                } else {
                    self.set_manual_paused(Some(None));
                }
                log::debug!(
                    "[{}] Toggled automatic brightness adjustment to {}",
                    self.output_name(),
                    if self.is_paused() { "paused" } else { "active" }
                );
            }
            CommandAction::IdleEnter(percent) => {
                self.idle = Some(percent);
                self.idle_changed();
                if self.is_paused() {
                    self.prediction = None;
                } else {
                    result = Some(self.apply_idle().await?);
                }
            }
            CommandAction::IdleLeave => {
                self.idle = None;
                self.idle_changed();
                if !self.is_paused() {
                    result = Some(self.apply_idle().await?);
                }
            }
        }
        Ok(self
            .brightness
            .percent(result.or(self.current).unwrap_or(self.brightness.min())))
    }

    fn is_paused(&self) -> bool {
        self.paused_until.is_some()
    }

    fn set_manual_paused(&mut self, paused_until: Option<Option<Instant>>) {
        let was_paused = self.is_paused();
        self.paused_until = paused_until;
        *self.pause_state.0.lock().unwrap() = paused_until;
        self.pause_state_changed(was_paused);
    }

    async fn resume_manual_pause(&mut self) -> anyhow::Result<()> {
        let was_manually_paused = self.paused_until.is_some();
        self.set_manual_paused(None);
        if was_manually_paused && self.idle.is_some() {
            self.apply_idle().await?;
        }
        Ok(())
    }

    fn pause_state_changed(&mut self, was_paused: bool) {
        let is_paused = self.is_paused();
        if let Some(paused) = &self.paused {
            paused.store(is_paused, Ordering::Relaxed);
        }
        if was_paused && !is_paused {
            if self.idle.is_none() {
                self.prediction = None;
            }
            self.resuming = true;
            while self.prediction_rx.try_recv().is_ok() {}
        }
        self.target = None;
        if let Some((status, output)) = &self.status {
            status.set_pause(output, self.paused_until.is_some(), self.idle.is_some());
        }
    }

    fn idle_changed(&mut self) {
        self.target = None;
        if let Some((status, output)) = &self.status {
            status.set_pause(output, self.is_paused(), self.idle.is_some());
        }
    }

    fn apply_idle_reduction(&self, value: u64) -> u64 {
        self.idle.map_or(value, |percent| {
            (value.saturating_mul(percent as u64) / 100)
                .clamp(self.brightness.min(), self.brightness.max())
        })
    }

    async fn apply_idle(&mut self) -> anyhow::Result<u64> {
        let current = self.read_brightness().await?;
        let prediction = match self.prediction {
            Some(prediction) => prediction,
            None => {
                self.prediction = Some(current);
                if self.current.is_none() {
                    self.user_tx.send(current).await?;
                    self.ready.store(true, Ordering::Release);
                }
                current
            }
        };
        let value = self
            .write_brightness(self.apply_idle_reduction(prediction))
            .await?;
        self.current = Some(value);
        if let Some((status, output)) = &self.status {
            status.set_brightness(output, self.brightness.percent(value));
        }
        Ok(value)
    }

    async fn expire_pause(&mut self) {
        if self
            .paused_until
            .flatten()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            if let Err(error) = self.resume_manual_pause().await {
                log::error!("Unable to update idle brightness: {error:#}");
            }
            log::debug!(
                "[{}] Timed pause expired; resumed automatic brightness adjustment",
                self.output_name()
            );
        }
    }

    fn output_name(&self) -> &str {
        self.status
            .as_ref()
            .map_or("unknown", |(_, output)| output.as_str())
    }

    fn rebase_target(&mut self, now: Instant) {
        if let (Some(target), Some(current)) = (&mut self.target, self.current) {
            target.rebase(current, now);
        }
    }

    fn update_target(&mut self, desired: u64, now: Instant) {
        let min = self.brightness.min();
        let max = self.brightness.max();
        let desired = desired.clamp(min, max);
        let continuing = self
            .target
            .is_some_and(|target| target.destination().clamp(min, max) == desired);
        let current = self.current.expect("Current value is available");
        if desired == current {
            self.target = None;
            return;
        }
        if self
            .target
            .is_some_and(|target| target.destination() == desired)
        {
            return;
        }
        if !continuing && desired.abs_diff(current) < self.brightness.change_threshold() {
            self.target = None;
        } else if let Some(target) = &mut self.target {
            target.retarget(desired, now);
        } else {
            self.target = Some(Scalar::new(
                current,
                desired,
                self.timing.tau(TRANSITION_INTERVAL),
                now,
            ));
        }
    }

    async fn transition(&mut self, read_elapsed: Duration, now: Instant) {
        if !self.lifetime.is_valid() {
            self.target = None;
            return;
        }
        let target = self.target.as_mut().expect("Target is available");
        let desired = target.destination();
        let current = self.current.expect("Current value is available");
        let new_value = target
            .advance(now)
            .clamp(self.brightness.min(), self.brightness.max());
        if new_value != current {
            let started = Instant::now();
            let result = self.brightness.set_after_read(new_value).await;
            if result
                .as_ref()
                .err()
                .is_some_and(|error| error.is::<BackendChanged>())
            {
                self.lifetime.invalidate();
            }
            let write_elapsed = started.elapsed();
            match result {
                Ok(set_value) => {
                    if set_value == current {
                        log::warn!("Unable to change brightness from {current} to {new_value}, please report if you know how to reproduce this");
                    }
                    self.current = Some(set_value);
                    self.timing
                        .record(read_elapsed.saturating_add(write_elapsed));
                    target.set_tau(self.timing.tau(TRANSITION_INTERVAL), Instant::now());
                }
                Err(err) => {
                    self.io_failed = true;
                    self.rebase_target(Instant::now());
                    log::error!(
                        "Unable to set brightness to value '{}': {:?}",
                        new_value,
                        err
                    );
                }
            }
        }
        if self.current == Some(desired) {
            self.target = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use macro_rules_attribute::apply;
    use smol::channel;
    use smol_macros::test;

    #[apply(test!)]
    async fn resuming_a_replacement_started_paused_supplies_its_initial_baseline() {
        let (mut controller, _, user_rx) = setup(brightness_mock(vec![50], vec![]));
        controller
            .execute(CommandAction::Pause(None))
            .await
            .unwrap();
        controller.execute(CommandAction::Resume).await.unwrap();
        assert!(!controller.ready.load(Ordering::Acquire));
        controller.step().await;
        assert!(controller.ready.load(Ordering::Acquire));
        assert_eq!(50, user_rx.try_recv().unwrap());
        assert!(user_rx.is_empty());
        assert_eq!(Some(50), controller.current);
        assert!(controller.prediction.is_none());
    }

    #[apply(test!)]
    async fn invalidation_waits_for_inflight_io_without_publishing_stale_values() {
        for during_write in [false, true] {
            let (release, gate) = channel::bounded(1);
            let mut backend =
                brightness_mock(vec![50], if during_write { vec![100] } else { vec![] });
            if let Brightness::Mock {
                get_gate, set_gate, ..
            } = &mut backend
            {
                if during_write {
                    *set_gate = Some(gate);
                } else {
                    *get_gate = Some(gate);
                }
            }
            let (mut controller, _, user_rx) = setup(backend);
            controller.current = Some(50);
            controller.target = Some(ready_target(50, 100));
            let lifetime = controller.lifetime.clone();
            let mut running = Box::pin(controller.run());
            assert!(smol::future::poll_once(running.as_mut()).await.is_none());
            lifetime.invalidate();
            assert!(smol::future::poll_once(running.as_mut()).await.is_none());
            release.send(()).await.unwrap();
            smol::future::race(running, async {
                Timer::after(Duration::from_secs(1)).await;
                panic!("Brightness I/O did not finish during shutdown");
            })
            .await;
            assert!(user_rx.is_empty());
            assert!(is_brightness_spent(&controller.brightness));
        }
    }

    #[apply(test!)]
    async fn ddc_transition_reuses_its_immediately_preceding_read() {
        let (mut controller, _, user_rx) =
            setup(Brightness::DdcUtil(super::super::DdcUtil::mock(42, 100)));
        controller.current = Some(42);
        controller.target = Some(ready_target(42, 100));
        controller.step().await;
        assert_eq!(Some(100), controller.current);
        assert!(user_rx.is_empty());
        let Brightness::DdcUtil(ddc) = &controller.brightness else {
            unreachable!()
        };
        assert_eq!(1, ddc.mock_reads());
    }

    #[apply(test!)]
    async fn cached_cli_set_and_idle_restoration_detect_ddc_scale_changes() {
        for action in [
            CommandAction::Set(crate::control::Adjustment {
                percent: 80,
                relative: false,
                increase: true,
            }),
            CommandAction::Get,
            CommandAction::IdleLeave,
        ] {
            let mut ddc = super::super::DdcUtil::mock(42, 100);
            ddc.mock_set_maximum(255);
            let (mut controller, _, user_rx) = setup(Brightness::DdcUtil(ddc));
            controller.current = Some(42);
            controller.prediction = Some(248);
            assert!(controller.execute(action).await.is_err());
            assert!(!controller.lifetime.is_valid());
            assert!(user_rx.is_empty());
        }
    }

    #[apply(test!)]
    async fn invalidated_backends_cannot_restore_idle_or_write_predictions() {
        let (mut controller, _, user_rx) = setup(brightness_mock(vec![], vec![]));
        controller.current = Some(50);
        controller.prediction = Some(248);
        controller.target = Some(ready_target(50, 100));
        controller.lifetime.invalidate();
        assert!(controller.execute(CommandAction::IdleLeave).await.is_err());
        assert!(controller.execute(CommandAction::Get).await.is_err());
        controller.transition(Duration::ZERO, Instant::now()).await;
        assert!(controller.target.is_none());
        assert!(user_rx.is_empty());
        assert!(is_brightness_spent(&controller.brightness));
    }

    #[apply(test!)]
    async fn backend_replacement_preserves_manual_pause_and_absolute_deadline() {
        for duration in [None, Some(Duration::from_secs(60))] {
            let state = PauseState::default();
            let (controller, _, _) = setup(brightness_mock(vec![], vec![]));
            let mut controller = controller.with_lifetime(Lifetime::default(), state.clone());
            controller
                .execute(CommandAction::Pause(duration))
                .await
                .unwrap();
            let deadline = controller.paused_until;
            controller.lifetime.invalidate();
            drop(controller);
            let (replacement, _, _) = setup(brightness_mock(vec![], vec![]));
            let paused = Arc::new(AtomicBool::new(false));
            let mut replacement = replacement.with_lifetime(Lifetime::default(), state.clone());
            replacement.paused = Some(paused.clone());
            replacement.pause_state_changed(false);
            assert_eq!(deadline, replacement.paused_until);
            assert!(paused.load(Ordering::Relaxed));
            replacement.execute(CommandAction::Resume).await.unwrap();
            assert!(!state.is_paused());
        }
        let expired = Instant::now() - Duration::from_secs(1);
        let state = PauseState(Arc::new(Mutex::new(Some(Some(expired)))));
        let (controller, _, _) = setup(brightness_mock(vec![], vec![]));
        let controller = controller.with_lifetime(Lifetime::default(), state.clone());
        assert!(!controller.is_paused());
        assert!(!state.is_paused());
    }

    #[apply(test!)]
    async fn paused_backlight_replacement_invalidates_without_learning() {
        let fixture = crate::brightness::test_support::BacklightFixture::new(255);
        let backend = super::super::Backlight::new(fixture.path.to_str().unwrap(), 1)
            .await
            .unwrap();
        let (mut controller, _, user_rx) = setup(Brightness::Backlight(backend));
        controller
            .execute(CommandAction::Pause(None))
            .await
            .unwrap();
        fixture.replace(100);
        controller.step().await;
        assert!(!controller.lifetime.is_valid());
        assert!(user_rx.is_empty());
        assert_eq!(
            "50\n",
            std::fs::read_to_string(fixture.path.join("brightness")).unwrap()
        );
    }

    // Intentionally not in main code to prevent confusing fields by accident
    fn target(desired: u64, from: u64) -> Scalar {
        Scalar::new(from, desired, Duration::from_millis(200), Instant::now())
    }

    fn ready_target(from: u64, desired: u64) -> Scalar {
        Scalar::new(
            from,
            desired,
            Duration::from_millis(200),
            Instant::now() - Duration::from_secs(10),
        )
    }

    fn brightness_mock(get: Vec<u64>, set: Vec<u64>) -> Brightness {
        Brightness::Mock {
            get,
            set,
            change_threshold: 1,
            min: 0,
            max: 100,
            get_error: false,
            set_error: false,
            get_gate: None,
            set_gate: None,
        }
    }

    fn is_brightness_spent(mock: &Brightness) -> bool {
        match mock {
            Brightness::Mock { get, set, .. } => get.is_empty() && set.is_empty(),
            _ => unreachable!(),
        }
    }

    fn setup(brightness_mock: Brightness) -> (Controller, Sender<u64>, Receiver<u64>) {
        let (user_tx, user_rx) = channel::bounded(128);
        let (prediction_tx, prediction_rx) = channel::bounded(128);
        let controller = Controller::new(brightness_mock, user_tx, prediction_rx);
        (controller, prediction_tx, user_rx)
    }

    #[apply(test!)]
    async fn test_step_first_run() -> Result<()> {
        let (mut controller, prediction_tx, user_rx) = setup(brightness_mock(vec![42], vec![]));

        // even if predictor already wants a change...
        prediction_tx.send(37).await?;

        // when we execute the first step...
        controller.step().await;

        // a real current brightness level is respected and sent to predictor
        assert_eq!(Some(42), controller.current);
        assert_eq!(42, user_rx.try_recv()?);
        assert!(controller.target.is_none());
        assert!(is_brightness_spent(&controller.brightness));

        Ok(())
    }

    #[apply(test!)]
    async fn test_step_first_run_brightness_zero() -> Result<()> {
        // if the current brightness value is zero...
        let (mut controller, prediction_tx, user_rx) = setup(brightness_mock(vec![0], vec![]));

        // even if predictor already wants a change...
        prediction_tx.send(37).await?;

        // when we execute the first step...
        controller.step().await;

        // a brightness value of zero is being sent to predictor
        assert_eq!(Some(0), controller.current);
        assert_eq!(0, user_rx.try_recv()?);
        assert!(controller.target.is_none());
        assert!(is_brightness_spent(&controller.brightness));

        Ok(())
    }

    #[apply(test!)]
    async fn test_step_user_changed_brightness() -> Result<()> {
        let (mut controller, prediction_tx, user_rx) = setup(brightness_mock(vec![42], vec![]));

        // when last brightness differs from the current one
        controller.current = Some(66);

        // even if predictor wants a change...
        prediction_tx.send(37).await?;

        // ... or we were already in a transition
        controller.target = Some(target(77, 1));

        // when we execute the next step...
        controller.step().await;

        // we notice a change in brightness made by user and that takes priority
        assert_eq!(Some(42), controller.current);
        assert_eq!(42, user_rx.try_recv()?);
        assert!(controller.target.is_none());
        assert!(is_brightness_spent(&controller.brightness));

        Ok(())
    }

    #[apply(test!)]
    async fn test_step_ddc_manual_change_overrides_transition_and_prediction() -> Result<()> {
        let brightness = Brightness::DdcUtil(super::super::DdcUtil::mock(42, 100));
        let (mut controller, prediction_tx, user_rx) = setup(brightness);
        controller.current = Some(66);
        controller.target = Some(target(77, 3));
        prediction_tx.send(37).await?;

        controller.step().await;

        assert_eq!(Some(42), controller.current);
        assert_eq!(Some(42), controller.prediction);
        assert_eq!(None, controller.target);
        assert_eq!(42, user_rx.try_recv()?);
        assert!(user_rx.is_empty());
        assert!(controller.prediction_rx.is_empty());
        assert_eq!(42, controller.brightness.get().await?);
        Ok(())
    }

    #[test]
    fn targets_are_normalized_and_same_prediction_keeps_progress() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![]));
        if let Brightness::Mock { min, max, .. } = &mut controller.brightness {
            *min = 5;
            *max = 80;
        }
        let now = Instant::now();
        controller.current = Some(50);
        controller.update_target(1000, now);
        let original = controller.target.unwrap();
        assert_eq!(80, original.destination());
        controller.update_target(u64::MAX, now + Duration::from_millis(100));
        assert_eq!(Some(original), controller.target);
        controller.update_target(0, now + Duration::from_millis(100));
        assert_eq!(5, controller.target.unwrap().destination());
        assert_eq!(62, controller.target.unwrap().output());
        controller.update_target(50, now);
        assert!(controller.target.is_none());
    }

    #[test]
    fn deadband_cancels_new_predictions_but_not_an_existing_endpoint() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![]));
        if let Brightness::Mock {
            change_threshold, ..
        } = &mut controller.brightness
        {
            *change_threshold = 20;
        }
        let now = Instant::now();
        controller.current = Some(50);
        controller.update_target(100, now);
        controller.update_target(69, now);
        assert!(controller.target.is_none());
        controller.update_target(70, now);
        assert_eq!(70, controller.target.unwrap().destination());
        controller.current = Some(65);
        let original = controller.target;
        controller.update_target(70, now + Duration::from_millis(100));
        assert_eq!(original, controller.target);
    }

    #[apply(test!)]
    async fn step_suppresses_duplicates_and_finishes_rounded_endpoint() -> Result<()> {
        let (mut controller, prediction_tx, user_rx) = setup(brightness_mock(vec![50], vec![]));
        controller.current = Some(50);
        controller.target = Some(Scalar::new(
            50,
            100,
            Duration::from_millis(200),
            Instant::now() + Duration::from_secs(10),
        ));
        controller.step().await;
        assert_eq!(Some(50), controller.current);
        assert!(controller.target.is_some());
        assert_eq!(IoTiming::default(), controller.timing);
        assert!(is_brightness_spent(&controller.brightness));
        assert!(user_rx.is_empty());
        prediction_tx.send(50).await?;
        if let Brightness::Mock { get, .. } = &mut controller.brightness {
            get.push(50);
        }
        controller.step().await;
        assert!(controller.target.is_none());

        for (current, desired) in [(0, 1), (1, 0), (0, 2), (2, 3)] {
            let (mut controller, _, _) =
                setup(brightness_mock(vec![current, desired], vec![desired]));
            controller.current = Some(current);
            controller.target = Some(ready_target(current, desired));
            controller.step().await;
            assert_eq!(Some(desired), controller.current);
            assert!(controller.target.is_none());
            controller.step().await;
            assert!(is_brightness_spent(&controller.brightness));
        }
        Ok(())
    }

    #[apply(test!)]
    async fn step_uses_latest_prediction_not_obsolete_direction() -> Result<()> {
        for (current, desired) in [(80, 20), (20, 80)] {
            let (mut controller, prediction_tx, user_rx) =
                setup(brightness_mock(vec![current], vec![]));
            controller.current = Some(current);
            let obsolete = if desired < current { 100 } else { 0 };
            controller.target = Some(target(obsolete, current));
            prediction_tx.send(obsolete).await?;
            prediction_tx.send(desired).await?;
            controller.step().await;
            assert_eq!(Some(current), controller.current);
            assert_eq!(desired, controller.target.unwrap().destination());
            assert!(controller.prediction_rx.is_empty());
            assert!(user_rx.is_empty());
            assert!(is_brightness_spent(&controller.brightness));
        }
        Ok(())
    }

    #[apply(test!)]
    async fn active_target_normalizes_after_maximum_shrinks_with_equivalent_predictions() {
        for prediction in [None, Some(60), Some(1000), Some(u64::MAX)] {
            let (mut controller, prediction_tx, user_rx) =
                setup(brightness_mock(vec![50, 60], vec![60]));
            controller.current = Some(50);
            controller.target = Some(ready_target(50, 100));
            if let Brightness::Mock {
                max,
                change_threshold,
                ..
            } = &mut controller.brightness
            {
                *max = 60;
                *change_threshold = 20;
            }
            if let Some(prediction) = prediction {
                prediction_tx.try_send(prediction).unwrap();
            }
            controller.step().await;
            assert_eq!(Some(60), controller.current);
            assert!(controller.target.is_none());
            let timing = controller.timing;
            controller.step().await;
            assert_eq!(Some(60), controller.current);
            assert!(controller.target.is_none());
            assert_eq!(timing, controller.timing);
            assert!(controller.prediction_rx.is_empty());
            assert!(user_rx.is_empty());
            assert!(is_brightness_spent(&controller.brightness));
        }
    }

    #[apply(test!)]
    async fn transition_writes_intermediate_values_in_both_directions() {
        for (current, desired, expected) in [(10, 20, 14), (20, 10, 16)] {
            let (mut controller, _, user_rx) = setup(brightness_mock(vec![], vec![expected]));
            let now = Instant::now();
            controller.current = Some(current);
            controller.update_target(desired, now);
            controller
                .transition(Duration::ZERO, now + Duration::from_millis(100))
                .await;
            assert_eq!(Some(expected), controller.current);
            assert_eq!(desired, controller.target.unwrap().destination());
            assert_ne!(IoTiming::default(), controller.timing);
            assert!(user_rx.is_empty());
            assert!(is_brightness_spent(&controller.brightness));
        }
    }

    #[apply(test!)]
    async fn slow_read_and_write_consume_cadence_and_only_io_is_measured() -> Result<()> {
        let (get_tx, get_rx) = channel::bounded(1);
        let (set_tx, set_rx) = channel::bounded(1);
        let mut brightness = brightness_mock(vec![0], vec![100]);
        if let Brightness::Mock {
            get_gate, set_gate, ..
        } = &mut brightness
        {
            *get_gate = Some(get_rx);
            *set_gate = Some(set_rx);
        }
        let (mut controller, _, _) = setup(brightness);
        controller.current = Some(0);
        controller.target = Some(ready_target(0, 100));
        let mut step = Box::pin(controller.step());
        assert!(smol::future::poll_once(step.as_mut()).await.is_none());
        let read_started = Instant::now();
        Timer::after(Duration::from_millis(30)).await;
        let read_blocked = read_started.elapsed();
        get_tx.try_send(())?;
        assert!(smol::future::poll_once(step.as_mut()).await.is_none());
        let write_started = Instant::now();
        Timer::after(Duration::from_millis(30)).await;
        let write_blocked = write_started.elapsed();
        set_tx.try_send(())?;
        assert!(smol::future::poll_once(step.as_mut()).await.is_some());
        drop(step);
        assert_eq!(Some(100), controller.current);
        assert!(controller.target.is_none());
        assert!(is_brightness_spent(&controller.brightness));
        assert!(
            controller.timing.tau(TRANSITION_INTERVAL)
                >= ((read_blocked + write_blocked) * 8).min(Duration::from_secs(1))
        );
        let timing = controller.timing;
        controller
            .wait_until(Instant::now() + Duration::from_millis(30))
            .await;
        assert_eq!(timing, controller.timing);
        Ok(())
    }

    #[apply(test!)]
    async fn transition_wait_yields_and_handles_commands() -> Result<()> {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![]));
        let (commands_tx, commands_rx) = channel::bounded(1);
        controller.commands = Some(commands_rx);
        controller.current = Some(50);
        controller.target = Some(ready_target(50, 100));
        let mut wait = Box::pin(controller.wait_until(Instant::now() + Duration::from_secs(60)));
        assert!(smol::future::poll_once(wait.as_mut()).await.is_none());
        let (response_tx, response_rx) = channel::bounded(1);
        commands_tx
            .send(Command {
                action: CommandAction::Pause(None),
                response: response_tx,
            })
            .await?;
        assert!(smol::future::poll_once(wait.as_mut()).await.is_none());
        assert_eq!(Ok(50), response_rx.try_recv()?);
        drop(wait);
        assert!(controller.is_paused());
        assert!(controller.target.is_none());
        assert!(is_brightness_spent(&controller.brightness));
        Ok(())
    }

    #[apply(test!)]
    async fn read_and_write_errors_rebase_and_retry_without_false_completion() {
        for read_fails in [true, false] {
            let mut brightness = brightness_mock(
                if read_fails { vec![] } else { vec![50] },
                if read_fails { vec![] } else { vec![100] },
            );
            if let Brightness::Mock {
                get_error,
                set_error,
                ..
            } = &mut brightness
            {
                *get_error = read_fails;
                *set_error = !read_fails;
            }
            let (mut controller, _, user_rx) = setup(brightness);
            controller.current = Some(50);
            controller.target = Some(ready_target(50, 100));
            let retry_interval = if read_fails {
                Duration::from_millis(controller.brightness.waiting_sleep_ms())
            } else {
                TRANSITION_INTERVAL
            };
            let started = Instant::now();
            controller.step().await;
            assert!(started.elapsed() >= retry_interval);
            assert_eq!(Some(50), controller.current);
            assert_eq!(50, controller.target.unwrap().output());
            assert_eq!(100, controller.target.unwrap().destination());
            assert_eq!(IoTiming::default(), controller.timing);
            assert!(user_rx.is_empty());
            assert!(is_brightness_spent(&controller.brightness));
            if let Brightness::Mock {
                get_error,
                set_error,
                get,
                set,
                ..
            } = &mut controller.brightness
            {
                *get_error = false;
                *set_error = false;
                get.push(50);
                set.push(100);
            }
            controller.step().await;
            assert_eq!(Some(50), controller.current);
            assert_eq!(50, controller.target.unwrap().output());
            assert_eq!(IoTiming::default(), controller.timing);
            controller
                .transition(Duration::ZERO, Instant::now() + Duration::from_secs(2))
                .await;
            assert_eq!(Some(100), controller.current);
            assert!(controller.target.is_none());
            assert_ne!(IoTiming::default(), controller.timing);
            assert!(is_brightness_spent(&controller.brightness));
        }
    }

    #[apply(test!)]
    async fn duplicate_outputs_keep_fractional_progress_without_learning() {
        for (from, to) in [(0, 1), (1, 0)] {
            let (mut controller, _, user_rx) = setup(brightness_mock(vec![to], vec![to]));
            let now = Instant::now();
            controller.current = Some(from);
            controller.update_target(to, now);
            for ms in (10..=130).step_by(10) {
                let at = now + Duration::from_millis(ms);
                controller.update_target(to, at);
                controller.transition(Duration::ZERO, at).await;
                assert_eq!(Some(from), controller.current);
                assert!(controller.target.is_some());
                assert_eq!(IoTiming::default(), controller.timing);
            }
            controller
                .transition(Duration::ZERO, now + Duration::from_millis(140))
                .await;
            assert_eq!(Some(to), controller.current);
            assert!(controller.target.is_none());
            controller.step().await;
            assert!(user_rx.is_empty());
            assert!(is_brightness_spent(&controller.brightness));
        }
    }

    #[apply(test!)]
    async fn accepted_endpoint_finishes_inside_admission_deadband() {
        let (mut controller, _, user_rx) = setup(brightness_mock(vec![65, 70], vec![70]));
        if let Brightness::Mock {
            change_threshold, ..
        } = &mut controller.brightness
        {
            *change_threshold = 20;
        }
        controller.current = Some(65);
        controller.target = Some(ready_target(50, 70));
        controller.step().await;
        assert_eq!(Some(70), controller.current);
        assert!(controller.target.is_none());
        controller.step().await;
        assert!(user_rx.is_empty());
        assert!(is_brightness_spent(&controller.brightness));
    }

    #[apply(test!)]
    async fn cli_set_discards_pending_prediction() -> Result<()> {
        let (mut controller, prediction_tx, user_rx) = setup(brightness_mock(vec![], vec![60]));
        controller.current = Some(50);
        prediction_tx.send(40).await?;

        controller
            .execute(CommandAction::Set(crate::control::Adjustment {
                percent: 60,
                relative: false,
                increase: true,
            }))
            .await?;

        assert!(controller.prediction_rx.is_empty());
        assert_eq!(60, user_rx.try_recv()?);
        assert_eq!(Some(60), controller.current);
        Ok(())
    }

    #[apply(test!)]
    async fn cli_set_while_idle_is_not_learned() -> Result<()> {
        let (mut controller, _, user_rx) = setup(brightness_mock(vec![], vec![60]));
        controller.current = Some(30);
        controller.prediction = Some(100);
        controller.idle = Some(30);

        let result = controller
            .execute(CommandAction::Set(crate::control::Adjustment {
                percent: 60,
                relative: false,
                increase: true,
            }))
            .await?;

        assert_eq!(60, result);
        assert_eq!(Some(60), controller.current);
        assert_eq!(Some(100), controller.prediction);
        assert!(user_rx.is_empty());
        Ok(())
    }

    #[apply(test!)]
    async fn resumes_from_current_brightness_without_learning_it() -> Result<()> {
        let (mut controller, _, user_rx) = setup(brightness_mock(vec![50, 60], vec![60]));
        controller.current = Some(50);
        controller.set_manual_paused(Some(None));

        controller
            .execute(CommandAction::Set(crate::control::Adjustment {
                percent: 60,
                relative: false,
                increase: true,
            }))
            .await?;

        assert_eq!(Some(50), controller.current);
        assert!(user_rx.is_empty());

        controller.set_manual_paused(None);
        controller.step().await;

        assert_eq!(Some(60), controller.current);
        assert!(user_rx.is_empty());
        Ok(())
    }

    #[apply(test!)]
    async fn every_resume_discards_pending_predictions() -> Result<()> {
        let (mut controller, prediction_tx, _) = setup(brightness_mock(vec![], vec![]));
        controller.set_manual_paused(Some(None));
        prediction_tx.send(40).await?;

        controller.set_manual_paused(None);

        assert!(controller.prediction_rx.is_empty());
        assert!(controller.resuming);
        Ok(())
    }

    #[apply(test!)]
    async fn idle_reduction_tracks_and_restores_predictions() -> Result<()> {
        let (mut controller, prediction_tx, user_rx) =
            setup(brightness_mock(vec![80, 24, 21], vec![24, 21, 70]));
        controller.current = Some(80);

        controller.execute(CommandAction::IdleEnter(30)).await?;
        assert_eq!(Some(24), controller.current);
        assert!(!controller.is_paused());

        controller.target = Some(ready_target(24, 21));
        prediction_tx.send(70).await?;
        controller.step().await;
        assert_eq!(Some(21), controller.current);
        assert_eq!(Some(70), controller.prediction);

        controller.execute(CommandAction::IdleLeave).await?;
        assert_eq!(Some(70), controller.current);
        assert!(user_rx.is_empty());
        assert!(is_brightness_spent(&controller.brightness));
        Ok(())
    }

    #[apply(test!)]
    async fn idle_dim_can_turn_a_backlight_off() -> Result<()> {
        let (mut controller, _, user_rx) = setup(brightness_mock(vec![3], vec![0]));
        controller.current = Some(3);

        controller.execute(CommandAction::IdleEnter(0)).await?;

        assert_eq!(Some(0), controller.current);
        assert!(user_rx.is_empty());
        Ok(())
    }

    #[apply(test!)]
    async fn idle_dim_preserves_initial_predictor_brightness() -> Result<()> {
        let (mut controller, _, user_rx) = setup(brightness_mock(vec![80], vec![24]));

        controller.execute(CommandAction::IdleEnter(30)).await?;

        assert_eq!(80, user_rx.try_recv()?);
        assert_eq!(Some(24), controller.current);
        Ok(())
    }

    #[apply(test!)]
    async fn idle_does_not_publish_a_paused_state() -> Result<()> {
        let (mut controller, _, _) = setup(brightness_mock(vec![80], vec![24]));
        let paused = Arc::new(AtomicBool::new(false));
        controller.current = Some(80);
        controller.prediction = Some(80);
        controller.paused = Some(paused.clone());

        controller.execute(CommandAction::IdleEnter(30)).await?;
        assert!(!paused.load(Ordering::Relaxed));

        controller.execute(CommandAction::Pause(None)).await?;
        assert!(paused.load(Ordering::Relaxed));
        Ok(())
    }

    #[apply(test!)]
    async fn manual_pause_blocks_idle_dimming() -> Result<()> {
        let (mut controller, prediction_tx, user_rx) = setup(brightness_mock(vec![], vec![]));
        controller.current = Some(30);
        controller.execute(CommandAction::Pause(None)).await?;
        prediction_tx.send(40).await?;

        controller.execute(CommandAction::IdleEnter(30)).await?;

        assert_eq!(Some(30), controller.current);
        assert!(controller.is_paused());
        assert!(!controller.prediction_rx.is_empty());
        assert!(user_rx.is_empty());
        assert!(is_brightness_spent(&controller.brightness));
        Ok(())
    }

    #[apply(test!)]
    async fn resuming_manual_pause_applies_idle_dimming_to_current_brightness() -> Result<()> {
        let (mut controller, _, user_rx) = setup(brightness_mock(vec![60], vec![18]));
        controller.current = Some(80);
        controller.prediction = Some(80);
        controller.execute(CommandAction::Pause(None)).await?;
        controller.execute(CommandAction::IdleEnter(30)).await?;

        controller.execute(CommandAction::Resume).await?;

        assert_eq!(Some(18), controller.current);
        assert_eq!(Some(60), controller.prediction);
        assert!(!controller.is_paused());
        assert!(controller.paused_until.is_none());
        assert!(user_rx.is_empty());
        assert!(is_brightness_spent(&controller.brightness));
        Ok(())
    }

    #[apply(test!)]
    async fn manual_pause_while_idle_preserves_the_prediction() -> Result<()> {
        let (mut controller, _, user_rx) =
            setup(brightness_mock(vec![80, 24, 24], vec![24, 24, 80]));
        controller.current = Some(80);
        controller.prediction = Some(80);
        controller.execute(CommandAction::IdleEnter(30)).await?;
        controller.execute(CommandAction::Pause(None)).await?;

        controller.execute(CommandAction::Resume).await?;
        controller.execute(CommandAction::IdleLeave).await?;

        assert_eq!(Some(80), controller.current);
        assert_eq!(Some(80), controller.prediction);
        assert!(user_rx.is_empty());
        assert!(is_brightness_spent(&controller.brightness));
        Ok(())
    }

    #[apply(test!)]
    async fn expired_manual_pause_applies_idle_dimming() -> Result<()> {
        let (mut controller, _, user_rx) = setup(brightness_mock(vec![80], vec![24]));
        controller.current = Some(80);
        controller
            .execute(CommandAction::Pause(Some(Duration::ZERO)))
            .await?;
        controller.execute(CommandAction::IdleEnter(30)).await?;

        controller.expire_pause().await;

        assert_eq!(Some(24), controller.current);
        assert!(!controller.is_paused());
        assert!(controller.paused_until.is_none());
        assert!(user_rx.is_empty());
        assert!(is_brightness_spent(&controller.brightness));
        Ok(())
    }

    #[apply(test!)]
    async fn idle_leave_preserves_manual_pause_and_defers_resume() -> Result<()> {
        let (mut controller, prediction_tx, _) = setup(brightness_mock(vec![], vec![]));
        controller.current = Some(30);
        controller.set_manual_paused(Some(None));
        controller.execute(CommandAction::IdleEnter(30)).await?;
        prediction_tx.send(40).await?;

        controller.execute(CommandAction::IdleLeave).await?;
        assert!(controller.is_paused());
        assert!(!controller.prediction_rx.is_empty());

        controller.set_manual_paused(None);
        assert!(!controller.is_paused());
        assert!(controller.prediction_rx.is_empty());
        assert!(controller.resuming);
        Ok(())
    }

    #[apply(test!)]
    async fn rejects_unrepresentable_pause_duration() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![]));
        controller.current = Some(50);

        assert!(controller
            .execute(CommandAction::Pause(Some(Duration::MAX)))
            .await
            .is_err());
        assert!(!controller.is_paused());
    }
}
