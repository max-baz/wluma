use smol::channel::{Receiver, Sender};
use smol::Timer;

use crate::channel_ext::ReceiverExt;

use super::Brightness;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_TRANSITION_MS: u64 = 200;
// A longer gap between two transition steps is an interruption, not step time.
const MAX_MEASURED_STEP_MS: u64 = 5000;

pub struct Controller {
    brightness: Brightness,
    user_tx: Sender<u64>,
    prediction_rx: Receiver<u64>,
    prediction: Option<u64>,
    current: Option<u64>,
    target: Option<Target>,
    commands: Option<Receiver<Command>>,
    status: Option<(crate::control::Hub, String)>,
    paused_until: Option<Option<Instant>>,
    idle: Option<u8>,
    paused: Option<Arc<AtomicBool>>,
    resuming: bool,
    transition_ms: Option<u64>,
    // Measured time per transition step on this display, once there is one.
    step_ms: Option<u64>,
    last_step: Option<Instant>,
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

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct Target {
    desired: u64,
    step: i64,
    deadline: Option<Instant>,
}

impl Target {
    fn reached(&self, current: u64) -> bool {
        (self.step > 0 && current >= self.desired) || (self.step < 0 && current <= self.desired)
    }
}

impl Controller {
    pub fn new(brightness: Brightness, user_tx: Sender<u64>, prediction_rx: Receiver<u64>) -> Self {
        Self {
            brightness,
            user_tx,
            prediction_rx,
            prediction: None,
            current: None,
            target: None,
            commands: None,
            status: None,
            paused_until: None,
            idle: None,
            paused: None,
            resuming: false,
            transition_ms: None,
            step_ms: None,
            last_step: None,
        }
    }

    /// Finish each brightness change in about `transition_ms`, measured in real
    /// time on this display, instead of the default number of steps.
    pub fn with_transition_ms(mut self, transition_ms: u64) -> Self {
        self.transition_ms = Some(transition_ms);
        self
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

    pub async fn run(&mut self) {
        loop {
            self.step().await;
        }
    }

    async fn step(&mut self) {
        self.expire_pause().await;
        while let Some(command) = self
            .commands
            .as_ref()
            .and_then(|commands| commands.try_recv().ok())
        {
            self.command(command).await;
        }
        if self.is_paused() {
            self.wait().await;
            return;
        }
        match self.brightness.get().await {
            Ok(new_brightness) => {
                let predicted_value = self
                    .prediction_rx
                    .recv_maybe_last()
                    .await
                    .expect("prediction_rx closed unexpectedly");

                // 1. check if user wants to learn a new value - this overrides any ongoing activity
                if self.resuming {
                    self.resuming = false;
                    self.current = Some(new_brightness);
                    self.target = None;
                } else if Some(new_brightness) != self.current {
                    return self.update_current(new_brightness).await;
                }

                if let Some((status, output)) = &self.status {
                    status.set_brightness(output, self.brightness.percent(new_brightness));
                }

                // 2. check if predictor wants to set a new value
                if let Some(prediction) = predicted_value {
                    self.prediction = Some(prediction);
                    self.update_target(self.apply_idle_reduction(prediction));
                }

                // 3. continue the transition if there is one in progress
                if self.target.is_some() {
                    return self.transition().await;
                }
            }
            Err(err) => log::error!("Unable to get brightness value: {:?}", err),
        };

        // 4. nothing to do, sleep and check again
        // TODO: replace with inotify events on brightness device file and avoid sleep loop
        self.wait().await;
    }

    async fn wait(&mut self) {
        let delay = Duration::from_millis(self.brightness.waiting_sleep_ms());
        if let Some(commands) = self.commands.clone() {
            let command = smol::future::race(async { commands.recv().await.ok() }, async {
                Timer::after(delay).await;
                None
            })
            .await;
            if let Some(command) = command {
                self.command(command).await;
            }
        } else {
            Timer::after(delay).await;
        }
    }

    async fn update_current(&mut self, new_brightness: u64) {
        self.current = Some(new_brightness);
        if let Some((status, output)) = &self.status {
            status.set_brightness(output, self.brightness.percent(new_brightness));
        }
        if !self.is_paused() {
            self.learn(new_brightness)
                .await
                .expect("Unable to send new brightness value set by user, channel is dead");
        }
        self.target = None;
    }

    async fn learn(&mut self, value: u64) -> anyhow::Result<()> {
        if self.idle.is_some() {
            log::warn!(
                "[{}] Ignoring brightness adjustment for learning while idle",
                self.output_name()
            );
            return Ok(());
        }
        self.prediction = Some(value);
        self.user_tx.send(value).await?;
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
                let value = self.brightness.get().await?;
                if !self.is_paused() && Some(value) != self.current {
                    self.update_current(value).await;
                }
                result = Some(value);
            }
            CommandAction::Set(adjustment) => {
                let current = if self.is_paused() {
                    self.brightness.get().await?
                } else if let Some(current) = self.current {
                    current
                } else {
                    let current = self.brightness.get().await?;
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
                let value = self.brightness.set(value).await?;
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
        let current = self.brightness.get().await?;
        let prediction = match self.prediction {
            Some(prediction) => prediction,
            None => {
                self.prediction = Some(current);
                if self.current.is_none() {
                    self.user_tx.send(current).await?;
                }
                current
            }
        };
        let value = self
            .brightness
            .set(self.apply_idle_reduction(prediction))
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

    fn update_target(&mut self, desired: u64) {
        match (&self.target, self.current) {
            (Some(old_target), _) if old_target.desired == desired => (),
            (_, Some(current))
                if desired.abs_diff(current) < self.brightness.change_threshold() => {}
            (_, Some(current)) => {
                // Whatever ended the last transition (it finished, or a manual
                // change, pause or idle cut it short), the time since its last
                // step is not step time.
                if self.target.is_none() {
                    self.last_step = None;
                }
                let deadline = self
                    .transition_ms
                    .map(|ms| Instant::now() + Duration::from_millis(ms));
                let step = self.step_towards(current, desired, deadline);
                self.target = Some(Target {
                    desired,
                    step,
                    deadline,
                });
            }
            _ => unreachable!("Current value cannot be None at this point"),
        };
    }

    // By default a change takes as many steps as fit in 200 ms of the device's
    // step interval, however long the device then takes for each. With a
    // deadline it takes as many as fit in the time left at the step time
    // measured on this device, so a slow display takes bigger steps rather than
    // more time.
    fn step_towards(&self, current: u64, desired: u64, deadline: Option<Instant>) -> i64 {
        let steps = match deadline {
            Some(deadline) => (deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64)
                .div_ceil(
                    self.step_ms
                        .unwrap_or_else(|| self.brightness.transition_step_ms())
                        .max(1),
                ),
            None => DEFAULT_TRANSITION_MS.div_ceil(self.brightness.transition_step_ms()),
        }
        .max(1);
        let step = desired.abs_diff(current).div_ceil(steps) as i64;
        if desired > current {
            step
        } else {
            -step
        }
    }

    fn measure_step(&mut self, now: Instant) {
        if let Some(last) = self.last_step {
            let elapsed = now.saturating_duration_since(last).as_millis() as u64;
            if elapsed <= MAX_MEASURED_STEP_MS {
                self.step_ms = Some(match self.step_ms {
                    Some(previous) => smoothed_step_ms(previous, elapsed),
                    None => elapsed,
                });
            }
        }
        self.last_step = Some(now);
    }

    async fn transition(&mut self) {
        match (&self.target, self.current) {
            (Some(target), Some(current)) => {
                let target = *target;
                // Measure on the call that finds the target reached too: a
                // change made in one step has no other sample.
                self.measure_step(Instant::now());
                if target.reached(current) {
                    self.target = None;
                    self.last_step = None;
                } else {
                    let step = match target.deadline {
                        Some(_) => self.step_towards(current, target.desired, target.deadline),
                        None => target.step,
                    };
                    // The step is rounded up, so the last one can be larger than
                    // what is left. Stop at the target instead of passing it.
                    let new_value = current.saturating_add_signed(step);
                    let new_value = if step > 0 {
                        new_value.min(target.desired)
                    } else {
                        new_value.max(target.desired)
                    };
                    match self.brightness.set(new_value).await {
                        Ok(set_value) => {
                            if set_value == current {
                                log::warn!("Unable to change brightness from {current} to {new_value}, please report if you know how to reproduce this");
                            }
                            self.current = Some(set_value);
                        }
                        Err(err) => log::error!(
                            "Unable to set brightness to value '{}': {:?}",
                            new_value,
                            err
                        ),
                    };
                    thread::sleep(Duration::from_millis(self.brightness.transition_step_ms()));
                }
            }
            _ => unreachable!("Current and target values cannot be None at this point"),
        }
    }
}

fn smoothed_step_ms(previous: u64, measured: u64) -> u64 {
    (previous + measured).div_ceil(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use macro_rules_attribute::apply;
    use smol::channel;
    use smol_macros::test;

    // Intentionally not in main code to prevent confusing fields by accident
    fn target(desired: u64, step: i64) -> Target {
        Target {
            desired,
            step,
            deadline: None,
        }
    }

    fn brightness_mock(get: Vec<u64>, set: Vec<u64>) -> Brightness {
        Brightness::Mock {
            get,
            set,
            change_threshold: 1,
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

    #[test]
    fn test_update_target_ignore_when_desired_didnt_change() {
        let old_target = Some(target(10, -20));
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![]));
        controller.target = old_target;
        controller.current = Some(7);

        controller.update_target(10);

        assert_eq!(old_target, controller.target);
    }

    #[test]
    fn test_update_target_ignore_when_desired_equals_current() {
        let old_target = Some(target(10, -20));
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![]));
        controller.target = old_target;
        controller.current = Some(7);

        controller.update_target(7);

        assert_eq!(old_target, controller.target);
    }

    #[test]
    fn test_update_target_ignores_changes_below_device_threshold() {
        let (user_tx, _) = channel::bounded(1);
        let (_, prediction_rx) = channel::bounded(1);
        let brightness = Brightness::Mock {
            get: vec![],
            set: vec![],
            change_threshold: 20,
        };
        let mut controller = Controller::new(brightness, user_tx, prediction_rx);
        controller.current = Some(10000);

        controller.update_target(10019);

        assert_eq!(None, controller.target);

        controller.update_target(10020);

        assert_eq!(Some(target(10020, 1)), controller.target);
    }

    #[test]
    fn test_update_target_finds_minimal_step_that_reaches_target_within_transition_duration() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![]));

        let test_cases = vec![
            (0, 1, 1),
            (10000, 10001, 1),
            (10000, 10013, 1),
            (10000, 10199, 1),
            (10000, 10200, 1),
            (10000, 10413, 3),
            (10000, 11732, 9),
            (10000, 9999, -1),
            (10000, 9983, -1),
            (10000, 9801, -1),
            (10000, 9800, -1),
            (10000, 9473, -3),
            (10000, 8433, -8),
        ];

        for (current, desired, expected_step) in test_cases {
            controller.current = Some(current);
            controller.update_target(desired);
            assert_eq!(Some(target(desired, expected_step)), controller.target);
        }
    }

    #[test]
    fn test_update_target_sizes_steps_to_finish_within_transition() {
        let (controller, _, _) = setup(brightness_mock(vec![], vec![]));
        let mut controller = controller.with_transition_ms(4000);
        controller.step_ms = Some(800);
        controller.current = Some(49);

        controller.update_target(18);

        // 4000 ms at 800 ms per step is 5 steps for 31.
        let target = controller.target.unwrap();
        assert_eq!((18, -7), (target.desired, target.step));
        assert!(target.deadline.is_some());
    }

    #[apply(test!)]
    async fn test_transition_resizes_step_to_the_time_left() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![15]));
        controller.step_ms = Some(500);
        controller.current = Some(10);
        controller.target = Some(Target {
            desired: 20,
            step: 1,
            deadline: Some(Instant::now() + Duration::from_millis(1000)),
        });

        controller.transition().await;

        assert_eq!(Some(15), controller.current);
        assert!(is_brightness_spent(&controller.brightness));
    }

    #[apply(test!)]
    async fn test_transition_past_its_deadline_finishes() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![20]));
        controller.current = Some(10);
        controller.target = Some(Target {
            desired: 20,
            step: 1,
            deadline: Some(Instant::now()),
        });

        controller.transition().await;

        assert_eq!(Some(20), controller.current);
        assert!(is_brightness_spent(&controller.brightness));
    }

    #[test]
    fn test_first_step_measurement_replaces_the_nominal_step_time() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![]));
        let start = Instant::now();
        controller.last_step = Some(start);

        controller.measure_step(start + Duration::from_millis(850));
        assert_eq!(Some(850), controller.step_ms);

        controller.measure_step(start + Duration::from_millis(1600));
        assert_eq!(Some(800), controller.step_ms);
    }

    #[test]
    fn test_gaps_longer_than_a_step_are_not_measured() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![]));
        controller.step_ms = Some(800);
        let start = Instant::now();

        controller.last_step = Some(start);
        controller.measure_step(start + Duration::from_millis(MAX_MEASURED_STEP_MS + 1));
        assert_eq!(Some(800), controller.step_ms);

        controller.last_step = Some(start);
        controller.measure_step(start + Duration::from_millis(MAX_MEASURED_STEP_MS));
        assert_eq!(Some(2900), controller.step_ms);
    }

    #[apply(test!)]
    async fn test_single_step_transitions_still_measure_their_step() {
        // A slow outlier pushed the estimate past the whole transition time,
        // so every change is one step. Reaching the target must still take a
        // sample, or the estimate could never come back down.
        let (controller, _, _) = setup(brightness_mock(vec![], vec![]));
        let mut controller = controller.with_transition_ms(1000);
        controller.step_ms = Some(1150);
        controller.current = Some(20);
        controller.target = Some(target(20, -30));
        controller.last_step = Instant::now().checked_sub(Duration::from_millis(300));

        controller.transition().await;

        assert_eq!(None, controller.target);
        assert!(controller.step_ms.unwrap() < 1150);
        assert_eq!(None, controller.last_step);
    }

    #[test]
    fn test_new_transition_does_not_measure_the_gap_before_it() {
        let (controller, _, _) = setup(brightness_mock(vec![], vec![]));
        let mut controller = controller.with_transition_ms(3000);
        controller.step_ms = Some(800);
        controller.current = Some(60);
        // A transition that a manual change cut short three seconds ago.
        controller.last_step = Instant::now().checked_sub(Duration::from_secs(3));

        controller.update_target(29);

        assert_eq!(None, controller.last_step);
    }

    #[apply(test!)]
    async fn test_transition_reset_target_when_reached() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![]));
        controller.current = Some(10);
        controller.target = Some(target(10, 20));

        controller.transition().await;

        assert_eq!(None, controller.target);
    }

    #[apply(test!)]
    async fn test_transition_increases_brightness_with_next_step() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![12]));
        controller.current = Some(10);
        controller.target = Some(target(20, 2));

        controller.transition().await;

        assert_eq!(Some(12), controller.current);
        assert!(is_brightness_spent(&controller.brightness));
    }

    #[apply(test!)]
    async fn test_transition_decreases_brightness_with_next_step() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![9]));
        controller.current = Some(10);
        controller.target = Some(target(9, -1));

        controller.transition().await;

        assert_eq!(Some(9), controller.current);
        assert!(is_brightness_spent(&controller.brightness));
    }

    #[apply(test!)]
    async fn test_transition_doesnt_decrease_below_0() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![0]));
        controller.current = Some(1);
        controller.target = Some(target(0, -2)); // step of -2 should not overshoot

        controller.transition().await;

        assert_eq!(Some(0), controller.current);
        assert!(is_brightness_spent(&controller.brightness));
    }

    #[apply(test!)]
    async fn test_transition_last_step_stops_at_target_when_increasing() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![13]));
        controller.current = Some(12);
        controller.target = Some(target(13, 4));

        controller.transition().await;

        assert_eq!(Some(13), controller.current);
        assert!(is_brightness_spent(&controller.brightness));
    }

    #[apply(test!)]
    async fn test_transition_last_step_stops_at_target_when_decreasing() {
        let (mut controller, _, _) = setup(brightness_mock(vec![], vec![50]));
        controller.current = Some(52);
        controller.target = Some(target(50, -4));

        controller.transition().await;

        assert_eq!(Some(50), controller.current);
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
            setup(brightness_mock(vec![80, 24, 23], vec![24, 23, 70]));
        controller.current = Some(80);

        controller.execute(CommandAction::IdleEnter(30)).await?;
        assert_eq!(Some(24), controller.current);
        assert!(!controller.is_paused());

        prediction_tx.send(70).await?;
        controller.step().await;
        assert_eq!(Some(23), controller.current);
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

    #[test]
    fn test_target_reached() {
        assert!(!target(10, 1).reached(9));
        assert!(target(10, 1).reached(10));
        assert!(target(10, 1).reached(11));

        assert!(target(10, -1).reached(9));
        assert!(target(10, -1).reached(10));
        assert!(!target(10, -1).reached(11));
    }
}
