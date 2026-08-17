use super::{Cooldown, INITIAL_TIMEOUT, PENDING_COOLDOWN};
use crate::{
    als::{Reading, Scale},
    channel_ext::ReceiverExt,
    predictor::data::Entry,
};
use smol::channel::{Receiver, Sender};

pub struct Controller {
    prediction_tx: Sender<u64>,
    user_rx: Receiver<u64>,
    als_rx: Receiver<Reading>,
    last_brightness: Option<u64>,
    points: Vec<Entry>,
    pre_reduction_brightness: Option<u64>,
    pending_cooldown: Cooldown,
    last_als: Option<Reading>,
    output_name: String,
    scale: Scale,
}

impl Controller {
    pub fn new(
        prediction_tx: Sender<u64>,
        user_rx: Receiver<u64>,
        als_rx: Receiver<Reading>,
        points: Vec<Entry>,
        output_name: &str,
        scale: Scale,
    ) -> Self {
        Self {
            prediction_tx,
            user_rx,
            als_rx,
            last_brightness: None,
            points,
            pre_reduction_brightness: None,
            pending_cooldown: Cooldown::default(),
            last_als: None,
            output_name: output_name.to_string(),
            scale,
        }
    }

    pub async fn adjust(&mut self, luma: u8) {
        if self.last_als.is_none() {
            // ALS controller is expected to send the initial value on this channel asap
            self.last_als = Some(
                self.als_rx
                    .recv_or_panic_after_timeout(INITIAL_TIMEOUT)
                    .await
                    .expect("als_rx closed unexpectedly"),
            );
        }

        if let Some(als) = self
            .als_rx
            .recv_maybe_last()
            .await
            .expect("als_rx closed unexpectedly")
        {
            self.last_als = Some(als);
        }

        let reading = self.last_als.expect("ALS value must be known");
        self.process_reading(reading, luma).await;
    }

    #[cfg(test)]
    async fn process(&mut self, als: u64, luma: u8) {
        self.process_reading(
            Reading {
                value: als,
                stable: true,
            },
            luma,
        )
        .await;
    }

    async fn process_reading(&mut self, reading: Reading, luma: u8) {
        let als = reading.value;
        if self.last_brightness.is_none() {
            // Brightness controller is expected to send the initial value on this channel asap
            self.last_brightness = self
                .user_rx
                .recv_maybe_last()
                .await
                .expect("user_rx closed unexpectedly")
                .or_else(|| panic!("Did not receive initial brightness value"));

            self.process_brightness_change(self.last_brightness.unwrap(), als, luma);
        }

        let current_brightness = self
            .user_rx
            .recv_maybe_last()
            .await
            .expect("user_rx closed unexpectedly")
            .or(self.last_brightness)
            .expect("Current brightness value must be known by now");

        if self.last_brightness != Some(current_brightness) {
            self.process_brightness_change(current_brightness, als, luma);
            self.pending_cooldown.reset(PENDING_COOLDOWN);
        } else if reading.stable && !self.pending_cooldown.is_active() {
            self.pending_cooldown.clear();
            self.predict(current_brightness, als, luma).await;
        }
    }

    async fn predict(&mut self, current_brightness: u64, als: u64, luma: u8) {
        let brightness_reduction = self.get_brightness_reduction(current_brightness, als, luma);

        let prediction = self
            .pre_reduction_brightness
            .expect("Pre-reduction brightness value must be known by now")
            .saturating_sub(brightness_reduction);

        log::trace!(
            "[{}] Prediction: brightness={prediction} (als: {als}, luma: {luma})",
            self.output_name
        );
        // The receiver can disappear while an output is shutting down.
        let _ = self.prediction_tx.send(prediction).await;
    }

    fn get_brightness_reduction(&mut self, current_brightness: u64, als: u64, luma: u8) -> u64 {
        let brightness_reduction = super::interpolate_raw(&self.points, self.scale, als, luma);
        (current_brightness as f64 * brightness_reduction.unwrap_or(0.0) / 100.0).round() as u64
    }

    fn process_brightness_change(&mut self, new_brightness: u64, als: u64, luma: u8) {
        let brightness_reduction = self.get_brightness_reduction(new_brightness, als, luma);
        self.pre_reduction_brightness = Some(new_brightness + brightness_reduction);
        self.last_brightness = Some(new_brightness);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use macro_rules_attribute::apply;
    use smol::channel;
    use smol_macros::test;

    const ALS_DISTANT: u64 = 200;
    const ALS_DIM: u64 = 20;

    async fn setup() -> Result<(Controller, Sender<u64>, Receiver<u64>)> {
        let (als_tx, als_rx) = channel::bounded(128);
        let (user_tx, user_rx) = channel::bounded(128);
        let (prediction_tx, prediction_rx) = channel::bounded(128);
        als_tx
            .send(Reading {
                value: ALS_DIM,
                stable: true,
            })
            .await?;
        user_tx.send(0).await?;

        let points = vec![
            Entry::new(ALS_DIM, 0, 0),
            Entry::new(ALS_DIM, 50, 30),
            Entry::new(ALS_DIM, 100, 60),
        ];
        let controller = Controller::new(
            prediction_tx,
            user_rx,
            als_rx,
            points,
            "eDP-1",
            Scale::Linear,
        );
        Ok((controller, user_tx, prediction_rx))
    }

    #[apply(test!)]
    async fn test_get_brightness_reduction() -> Result<()> {
        let (mut controller, _, _) = setup().await?;

        assert_eq!(controller.get_brightness_reduction(100, ALS_DIM, 0), 0);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DIM, 10), 2);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DIM, 20), 11);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DIM, 30), 23);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DIM, 40), 29);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DIM, 50), 30);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DIM, 60), 31);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DIM, 70), 37);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DIM, 80), 49);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DIM, 90), 58);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DIM, 100), 60);
        assert_eq!(controller.get_brightness_reduction(1000, ALS_DIM, 10), 24);

        Ok(())
    }

    #[apply(test!)]
    async fn test_no_brightness_reduction_for_distant_als() -> Result<()> {
        let (mut controller, _, _) = setup().await?;

        assert_eq!(controller.get_brightness_reduction(100, ALS_DISTANT, 0), 0);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DISTANT, 10), 0);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DISTANT, 20), 0);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DISTANT, 30), 0);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DISTANT, 40), 0);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DISTANT, 50), 0);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DISTANT, 60), 0);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DISTANT, 70), 0);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DISTANT, 80), 0);
        assert_eq!(controller.get_brightness_reduction(100, ALS_DISTANT, 90), 0);
        assert_eq!(
            controller.get_brightness_reduction(100, ALS_DISTANT, 100),
            0
        );

        Ok(())
    }

    #[apply(test!)]
    async fn test_change_in_luma() -> Result<()> {
        let (mut controller, user_tx, prediction_rx) = setup().await?;

        user_tx.send(100).await?;

        controller.process(ALS_DIM, 50).await;
        assert_eq!(prediction_rx.recv().await?, 100);

        controller.process(ALS_DIM, 10).await;
        assert_eq!(prediction_rx.recv().await?, 128);

        controller.process(ALS_DIM, 80).await;
        assert_eq!(prediction_rx.recv().await?, 81);

        Ok(())
    }

    #[apply(test!)]
    async fn test_unstable_als_prevents_prediction() -> Result<()> {
        let (mut controller, user_tx, prediction_rx) = setup().await?;
        user_tx.send(100).await?;
        controller
            .process_reading(
                Reading {
                    value: ALS_DIM,
                    stable: false,
                },
                50,
            )
            .await;

        assert!(prediction_rx.is_empty());
        Ok(())
    }

    #[apply(test!)]
    async fn test_change_in_brightness_by_user() -> Result<()> {
        let (mut controller, user_tx, prediction_rx) = setup().await?;

        // Initial brightness is used to predict right away
        user_tx.send(100).await?;
        controller.process(ALS_DIM, 50).await;
        assert_eq!(prediction_rx.recv().await?, 100);

        // Consequent user change causes prediction only after cooldown
        user_tx.send(123).await?;
        controller.process(ALS_DIM, 0).await;
        assert!(controller.pending_cooldown.is_active());
        assert!(prediction_rx.is_empty());

        // User doesn't change brightness anymore, so even if ALS or luma change, we are in cooldown period
        controller.process(ALS_DIM, 10).await;
        assert!(controller.pending_cooldown.is_active());
        assert!(prediction_rx.is_empty());

        // One final call will generate the actual prediction
        controller.pending_cooldown.finish();
        controller.process(ALS_DIM, 50).await;
        assert!(!controller.pending_cooldown.is_active());
        assert_eq!(86, prediction_rx.recv().await?);

        Ok(())
    }
}
