use smol::channel::{Receiver, Sender};
use smol::Timer;

use crate::channel_ext::ReceiverExt;

use super::Brightness;
use std::thread;
use std::time::Duration;

const TRANSITION_MAX_MS: u64 = 200;

pub struct Controller {
    brightness: Brightness,
    user_tx: Sender<u64>,
    prediction_rx: Receiver<u64>,
    current: Option<u64>,
    target: Option<Target>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct Target {
    desired: u64,
    step: i64,
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
            current: None,
            target: None,
        }
    }

    pub async fn run(&mut self) {
        loop {
            self.step().await;
        }
    }

    async fn step(&mut self) {
        match self.brightness.get().await {
            Ok(new_brightness) => {
                let predicted_value = self
                    .prediction_rx
                    .recv_maybe_last()
                    .await
                    .expect("prediction_rx closed unexpectedly");

                // 1. check if user wants to learn a new value - this overrides any ongoing activity
                if Some(new_brightness) != self.current {
                    return self.update_current(new_brightness).await;
                }

                // 2. check if predictor wants to set a new value
                if let Some(desired) = predicted_value {
                    self.update_target(desired);
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
        Timer::after(Duration::from_millis(self.brightness.waiting_sleep_ms())).await;
    }

    async fn update_current(&mut self, new_brightness: u64) {
        self.current = Some(new_brightness);
        self.user_tx
            .send(new_brightness)
            .await
            .expect("Unable to send new brightness value set by user, channel is dead");
        self.target = None;
    }

    fn update_target(&mut self, desired: u64) {
        match (&self.target, self.current) {
            (Some(old_target), _) if old_target.desired == desired => (),
            (_, Some(current)) if desired == current => (),
            (_, Some(current)) => {
                let max_transition_steps = TRANSITION_MAX_MS
                    .div_ceil(self.brightness.transition_step_ms())
                    .max(1);
                let step = if desired > current {
                    (desired - current).div_ceil(max_transition_steps) as i64
                } else {
                    -((current - desired).div_ceil(max_transition_steps) as i64)
                };
                self.target = Some(Target { desired, step });
            }
            _ => unreachable!("Current value cannot be None at this point"),
        };
    }

    async fn transition(&mut self) {
        match (&self.target, self.current) {
            (Some(target), Some(current)) => {
                if target.reached(current) {
                    self.target = None;
                } else {
                    let new_value = current.saturating_add_signed(target.step);
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use macro_rules_attribute::apply;
    use smol::channel;
    use smol_macros::test;

    // Intentionally not in main code to prevent confusing fields by accident
    fn target(desired: u64, step: i64) -> Target {
        Target { desired, step }
    }

    fn brightness_mock(get: Vec<u64>, set: Vec<u64>) -> Brightness {
        Brightness::Mock { get, set }
    }

    fn is_brightness_spent(mock: &Brightness) -> bool {
        match mock {
            Brightness::Mock { get, set } => get.is_empty() && set.is_empty(),
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
