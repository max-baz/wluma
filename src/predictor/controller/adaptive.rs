use smol::channel::{Receiver, Sender};

use super::{distance, monotonic, Cooldown, INITIAL_TIMEOUT, PENDING_COOLDOWN};
use crate::{
    als::{Reading, Scale},
    channel_ext::ReceiverExt,
    predictor::{
        data::{Data, Entry, Kind},
        AlsDirection,
    },
};
use std::collections::{BTreeMap, HashMap};

const REPLACEMENT_DISTANCE: f64 = 0.25;

fn collapse_luma(entries: &[Entry]) -> Vec<Entry> {
    let mut grouped = BTreeMap::<u64, (u128, u64)>::new();
    for entry in entries {
        let values = grouped.entry(entry.als).or_default();
        values.0 += entry.brightness as u128;
        values.1 += 1;
    }
    grouped
        .into_iter()
        .map(|(als, (total, count))| {
            Entry::new(als, 0, ((total + count as u128 / 2) / count as u128) as u64)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LumaDirection {
    Increasing,
    Decreasing,
}

pub struct Controller {
    prediction_tx: Sender<u64>,
    user_rx: Receiver<u64>,
    als_rx: Receiver<Reading>,
    pending_cooldown: Cooldown,
    pending: Option<Entry>,
    data: Data,
    stateful: bool,
    initial_brightness: Option<u64>,
    learn_initial: bool,
    luma_aware: bool,
    last_als: Option<Reading>,
    output_name: String,
    scale: Scale,
    als_direction: AlsDirection,
    luma_direction: LumaDirection,
    model: Option<monotonic::Model>,
    kind: Kind,
}

impl Controller {
    pub fn new(
        prediction_tx: Sender<u64>,
        user_rx: Receiver<u64>,
        als_rx: Receiver<Reading>,
        stateful: bool,
        output_name: &str,
        scale: Scale,
        legacy_thresholds: &HashMap<u64, String>,
    ) -> Self {
        Self::new_kind(
            (prediction_tx, user_rx, als_rx),
            stateful,
            output_name,
            scale,
            legacy_thresholds,
            Kind::Brightness,
        )
    }

    pub fn new_kind(
        channels: (Sender<u64>, Receiver<u64>, Receiver<Reading>),
        stateful: bool,
        output_name: &str,
        scale: Scale,
        legacy_thresholds: &HashMap<u64, String>,
        kind: Kind,
    ) -> Self {
        let (prediction_tx, user_rx, als_rx) = channels;
        let data = if stateful {
            Data::load_kind(output_name, kind, legacy_thresholds, scale)
        } else {
            Data::new_kind(output_name, kind, scale, legacy_thresholds)
        };

        Self {
            prediction_tx,
            user_rx,
            als_rx,
            pending_cooldown: Cooldown::default(),
            pending: None,
            data,
            stateful,
            initial_brightness: None,
            learn_initial: true,
            luma_aware: true,
            last_als: None,
            output_name: output_name.to_string(),
            scale,
            als_direction: AlsDirection::Increasing,
            luma_direction: LumaDirection::Decreasing,
            model: None,
            kind,
        }
    }

    pub fn without_initial_value(mut self) -> Self {
        self.learn_initial = false;
        self
    }

    pub fn without_luma(mut self) -> Self {
        self.luma_aware = false;
        self.model = None;
        self
    }

    pub fn with_als_direction(mut self, als_direction: AlsDirection) -> Self {
        self.als_direction = als_direction;
        self.model = None;
        self
    }

    pub fn with_luma_direction(mut self, luma_direction: LumaDirection) -> Self {
        self.luma_direction = luma_direction;
        self.model = None;
        self
    }

    pub async fn discard_inputs(&mut self) {
        if self.last_als.is_none() {
            self.last_als = Some(
                self.als_rx
                    .recv_or_panic_after_timeout(INITIAL_TIMEOUT)
                    .await
                    .expect("als_rx closed unexpectedly"),
            );
            self.user_rx
                .recv_or_panic_after_timeout(INITIAL_TIMEOUT)
                .await
                .expect("user_rx closed unexpectedly");
        }
        if let Some(als) = self
            .als_rx
            .recv_maybe_last()
            .await
            .expect("als_rx closed unexpectedly")
        {
            self.last_als = Some(als);
        }
        while self.user_rx.try_recv().is_ok() {}
        self.initial_brightness = None;
        self.pending = None;
        self.pending_cooldown.clear();
    }

    pub fn discard_stale_inputs(&mut self) {
        // Without a current frame, queued brightness changes cannot be associated
        // with the luma at which they happened. Preserve startup inputs until the
        // controller has initialized, but discard stale learning state afterwards.
        if self.last_als.is_none() {
            return;
        }
        while let Ok(als) = self.als_rx.try_recv() {
            self.last_als = Some(als);
        }
        while self.user_rx.try_recv().is_ok() {}
        self.initial_brightness = None;
        self.pending = None;
        self.pending_cooldown.clear();
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

            // Brightness controller is expected to send the initial value on this channel asap
            let initial_brightness = self
                .user_rx
                .recv_or_panic_after_timeout(INITIAL_TIMEOUT)
                .await
                .expect("user_rx closed unexpectedly");

            // If there are no learned entries yet, we will use this as the first data point,
            // assuming that user is happy with the current brightness settings
            if self.learn_initial && self.data.entries.is_empty() {
                self.initial_brightness = Some(initial_brightness);
            };
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
        let initial_brightness = self.initial_brightness.take();
        let user_changed_brightness = self
            .user_rx
            .recv_maybe_last()
            .await
            .expect("user_rx closed unexpectedly")
            .or(initial_brightness);

        if let Some(brightness) = user_changed_brightness {
            self.pending = match &self.pending {
                None => Some(Entry::new(reading.value, luma, brightness)),
                Some(Entry { als, .. }) => Some(Entry::new(
                    if reading.stable { reading.value } else { *als },
                    luma,
                    brightness,
                )),
            };
            self.pending_cooldown.reset(PENDING_COOLDOWN);
            return;
        }

        if !reading.stable {
            return;
        }

        if let Some(pending) = self.pending.as_mut() {
            pending.als = reading.value;
            if !self.pending_cooldown.is_active() {
                self.pending_cooldown.clear();
                self.learn();
            }
        } else if !self.pending_cooldown.is_active() {
            self.pending_cooldown.clear();
            self.predict(reading.value, luma).await;
        }
    }

    fn learn(&mut self) {
        let pending = self.pending.take().expect("No pending entry to learn");
        log::debug!(
            "[{}] Learning {}={}{} (als: {}, luma: {})",
            self.output_name,
            self.kind.name(),
            pending.brightness,
            self.kind.unit(),
            pending.als,
            pending.luma
        );

        let luma_aware = self.luma_aware;
        let pending_luma = self.luma(pending.luma);
        let scale = self.scale;
        self.data.entries.retain(|entry| {
            let nearby = if luma_aware {
                distance(scale, pending.als, pending_luma, entry)
            } else {
                (scale.coordinate(pending.als) - scale.coordinate(entry.als)).abs()
            } <= REPLACEMENT_DISTANCE;
            !nearby
        });

        self.data.entries.push(pending);
        self.model = None;

        self.data
            .entries
            .sort_unstable_by(|x, y| x.als.cmp(&y.als).then(x.luma.cmp(&y.luma)));

        if self.stateful {
            self.data.save().expect("Unable to save data");
        }
    }

    fn luma(&self, luma: u8) -> u8 {
        if self.luma_aware {
            luma
        } else {
            0
        }
    }

    async fn predict(&mut self, als: u64, luma: u8) {
        let luma = self.luma(luma);
        if self.model.is_none() {
            let projected = (!self.luma_aware).then(|| collapse_luma(&self.data.entries));
            let entries = projected.as_deref().unwrap_or(&self.data.entries);
            self.model = monotonic::Model::fit(
                entries,
                self.scale,
                self.als_direction,
                self.luma_direction,
                self.luma_aware,
            );
        }
        if let Some(prediction) = self
            .model
            .as_ref()
            .map(|model| model.predict(als, luma).round() as u64)
        {
            log::trace!(
                "[{}] Prediction: {}={prediction}{} (als: {als}, luma: {luma})",
                self.output_name,
                self.kind.name(),
                self.kind.unit()
            );
            // The receiver can disappear while an output is shutting down.
            let _ = self.prediction_tx.send(prediction).await;
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

    const ALS_DARK: u64 = 10;
    const ALS_DIM: u64 = 20;
    const ALS_BRIGHT: u64 = 30;

    async fn setup() -> Result<(Controller, Sender<u64>, Receiver<u64>)> {
        let (als_tx, als_rx) = channel::bounded(128);
        let (user_tx, user_rx) = channel::bounded(128);
        let (prediction_tx, prediction_rx) = channel::bounded(128);
        als_tx
            .send(Reading {
                value: ALS_BRIGHT,
                stable: true,
            })
            .await?;
        user_tx.send(0).await?;
        let controller = Controller::new(
            prediction_tx,
            user_rx,
            als_rx,
            false,
            "Dell 1",
            Scale::Linear,
            &HashMap::new(),
        );
        Ok((controller, user_tx, prediction_rx))
    }

    #[apply(test!)]
    async fn can_ignore_initial_value() -> Result<()> {
        let (controller, _, prediction_rx) = setup().await?;
        let mut controller = controller.without_initial_value();
        controller.adjust(20).await;

        assert!(controller.data.entries.is_empty());
        assert!(controller.pending.is_none());
        assert!(prediction_rx.is_empty());
        Ok(())
    }

    #[apply(test!)]
    async fn discards_brightness_changes_that_cannot_be_matched_to_a_frame() -> Result<()> {
        let (mut controller, user_tx, _) = setup().await?;
        controller.adjust(20).await;
        assert!(controller.pending.is_some());

        user_tx.send(100).await?;
        controller.discard_stale_inputs();

        assert!(controller.pending.is_none());
        assert!(controller.user_rx.is_empty());
        Ok(())
    }

    #[apply(test!)]
    async fn can_ignore_luma_without_discarding_it() -> Result<()> {
        let (mut controller, user_tx, prediction_rx) = setup().await?;
        controller.data.entries =
            vec![Entry::new(ALS_DIM, 10, 4000), Entry::new(ALS_DIM, 90, 5000)];
        controller = controller.without_initial_value().without_luma();

        assert_eq!(
            vec![Entry::new(ALS_DIM, 10, 4000), Entry::new(ALS_DIM, 90, 5000)],
            controller.data.entries
        );
        controller.adjust(10).await;
        controller.process(ALS_DIM, 90).await;

        assert_eq!(4500, prediction_rx.recv().await?);
        assert_eq!(4500, prediction_rx.recv().await?);

        user_tx.send(4600).await?;
        controller.process(ALS_DIM, 70).await;
        controller.pending_cooldown.finish();
        controller.process(ALS_DIM, 20).await;
        assert_eq!(vec![Entry::new(ALS_DIM, 70, 4600)], controller.data.entries);
        Ok(())
    }

    #[apply(test!)]
    async fn test_process_first_user_change() -> Result<()> {
        let (mut controller, user_tx, _) = setup().await?;

        // User changes brightness to value 33 for a given ALS and luma
        user_tx.send(33).await?;
        controller.process(ALS_DIM, 66).await;

        assert_eq!(Some(Entry::new(ALS_DIM, 66, 33)), controller.pending);
        assert!(controller.pending_cooldown.is_active());

        Ok(())
    }

    #[apply(test!)]
    async fn test_process_several_continuous_user_changes() -> Result<()> {
        let (mut controller, user_tx, _) = setup().await?;

        // User initiates brightness change for a given ALS and luma to value 33...
        user_tx.send(33).await?;
        controller.process(ALS_DIM, 66).await;
        // then quickly continues increasing it to 34 (while ALS and luma might already be different)...
        user_tx.send(34).await?;
        controller.process(ALS_BRIGHT, 36).await;
        // and even faster to 36 (which is the indended brightness value they wish to learn for the initial ALS and luma)
        user_tx.send(35).await?;
        user_tx.send(36).await?;
        controller.process(ALS_DARK, 16).await;

        assert_eq!(Some(Entry::new(ALS_DARK, 16, 36)), controller.pending);
        assert!(controller.pending_cooldown.is_active());

        Ok(())
    }

    #[apply(test!)]
    async fn test_process_learns_user_change_after_cooldown() -> Result<()> {
        let (mut controller, user_tx, _) = setup().await?;

        // User changes brightness to a desired value
        user_tx.send(33).await?;
        controller.process(ALS_DIM, 66).await;
        user_tx.send(33).await?;
        controller.process(ALS_BRIGHT, 64).await;
        user_tx.send(35).await?;
        controller.process(ALS_DARK, 62).await;

        // User doesn't change brightness anymore, so even if ALS or luma change, we are in cooldown period
        controller.process(ALS_BRIGHT, 60).await;
        assert!(controller.pending_cooldown.is_active());
        assert_eq!(Some(Entry::new(ALS_BRIGHT, 62, 35)), controller.pending);

        // One final process will trigger the learning
        controller.pending_cooldown.finish();
        controller.process(ALS_DARK, 61).await;

        assert_eq!(None, controller.pending);
        assert!(!controller.pending_cooldown.is_active());
        assert_eq!(vec![Entry::new(ALS_DARK, 62, 35)], controller.data.entries);

        Ok(())
    }

    #[apply(test!)]
    async fn unstable_als_prevents_prediction() -> Result<()> {
        let (mut controller, _, prediction_rx) = setup().await?;
        controller.data.entries = vec![Entry::new(ALS_DIM, 20, 30)];

        controller
            .process_reading(
                Reading {
                    value: ALS_DIM,
                    stable: false,
                },
                20,
            )
            .await;

        assert!(prediction_rx.is_empty());
        Ok(())
    }

    #[apply(test!)]
    async fn unstable_als_delays_learning_after_cooldown() -> Result<()> {
        let (mut controller, user_tx, _) = setup().await?;
        user_tx.send(33).await?;
        controller.process(ALS_DIM, 20).await;
        controller
            .process_reading(
                Reading {
                    value: ALS_DIM,
                    stable: false,
                },
                20,
            )
            .await;
        controller.pending_cooldown.finish();
        controller
            .process_reading(
                Reading {
                    value: ALS_DIM,
                    stable: false,
                },
                20,
            )
            .await;
        assert!(controller.pending.is_some());
        assert!(controller.data.entries.is_empty());

        controller
            .process_reading(
                Reading {
                    value: ALS_BRIGHT,
                    stable: true,
                },
                20,
            )
            .await;

        assert_eq!(None, controller.pending);
        assert_eq!(
            vec![Entry::new(ALS_BRIGHT, 20, 33)],
            controller.data.entries
        );
        Ok(())
    }

    #[apply(test!)]
    async fn learning_replaces_only_observations_of_the_same_conditions() -> Result<()> {
        let (mut controller, _, _) = setup().await?;
        controller.data.entries = vec![
            Entry::new(21, 20, 31),
            Entry::new(10, 30, 31),
            Entry::new(30, 10, 29),
        ];
        controller.pending = Some(Entry::new(ALS_DIM, 20, 30));

        controller.learn();

        assert_eq!(
            vec![
                Entry::new(10, 30, 31),
                Entry::new(20, 20, 30),
                Entry::new(30, 10, 29),
            ],
            controller.data.entries
        );
        Ok(())
    }

    #[apply(test!)]
    async fn test_predict_no_data_points() -> Result<()> {
        let (mut controller, _, prediction_rx) = setup().await?;
        controller.data.entries = vec![];

        // predict() should not be called with no data, but just in case confirm we don't panic
        controller.predict(ALS_DIM, 20).await;

        assert!(prediction_rx.try_recv().is_err());

        Ok(())
    }

    #[apply(test!)]
    async fn one_observation_is_used_across_the_domain() -> Result<()> {
        let (mut controller, _, prediction_rx) = setup().await?;
        controller.data.entries = vec![Entry::new(100, 100, 42)];

        controller.predict(ALS_DIM, 20).await;

        assert_eq!(42, prediction_rx.try_recv()?);
        Ok(())
    }

    #[apply(test!)]
    async fn reported_non_monotonic_case_is_monotonic() -> Result<()> {
        let (mut controller, _, prediction_rx) = setup().await?;
        controller.scale = Scale::Lux;
        controller.data.entries = vec![
            Entry::new(0, 14, 10),
            Entry::new(1, 61, 10),
            Entry::new(503, 0, 100),
            Entry::new(1264, 18, 100),
        ];

        controller.predict(0, 20).await;
        controller.predict(0, 47).await;
        let darker = prediction_rx.try_recv()?;
        let whiter = prediction_rx.try_recv()?;

        assert!(whiter <= darker, "{whiter} > {darker}");
        Ok(())
    }
}
