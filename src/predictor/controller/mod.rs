use super::data::Entry;
use crate::als::Scale;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub mod adaptive;
pub mod manual;
mod monotonic;

const INITIAL_TIMEOUT: Duration = Duration::from_secs(15);
const PENDING_COOLDOWN: Duration = Duration::from_millis(1500);
const LUMA_SCALE: f64 = 20.0;
const MAX_DISTANCE: f64 = 5.0;

#[derive(Default)]
struct Cooldown {
    until: Option<Instant>,
}

impl Cooldown {
    fn reset(&mut self, duration: Duration) {
        self.until = Some(Instant::now() + duration);
    }

    fn is_active(&self) -> bool {
        self.until.is_some_and(|until| Instant::now() < until)
    }

    fn clear(&mut self) {
        self.until = None;
    }

    #[cfg(test)]
    fn finish(&mut self) {
        self.until = Some(Instant::now());
    }
}

#[allow(clippy::large_enum_variant)]
enum Inner {
    Adaptive(adaptive::Controller),
    Manual(manual::Controller),
}

pub struct Controller {
    inner: Inner,
    status: Option<(crate::control::Hub, String)>,
    paused: Option<Arc<AtomicBool>>,
    enabled: Option<Arc<AtomicBool>>,
    additional: Vec<Controller>,
}

impl Controller {
    pub fn adaptive(controller: adaptive::Controller) -> Self {
        Self {
            inner: Inner::Adaptive(controller),
            status: None,
            paused: None,
            enabled: None,
            additional: Vec::new(),
        }
    }

    pub fn manual(controller: manual::Controller) -> Self {
        Self {
            inner: Inner::Manual(controller),
            status: None,
            paused: None,
            enabled: None,
            additional: Vec::new(),
        }
    }

    pub fn with_additional(mut self, controllers: Vec<Controller>) -> Self {
        self.additional = controllers;
        self
    }

    pub fn with_enabled(mut self, enabled: Arc<AtomicBool>) -> Self {
        self.enabled = Some(enabled);
        self
    }

    pub fn with_status(
        mut self,
        status: crate::control::Hub,
        output: String,
        paused: Arc<AtomicBool>,
    ) -> Self {
        self.status = Some((status, output));
        self.paused = Some(paused);
        self
    }

    pub async fn adjust(&mut self, luma: u8) {
        if !self.can_adjust() {
            return;
        }
        if let Some((status, output)) = &self.status {
            status.set_luma(output, luma);
        }
        self.adjust_inner(luma).await;
        for controller in &mut self.additional {
            if controller.can_adjust() {
                controller.adjust_inner(luma).await;
            } else if !controller.is_enabled() {
                controller.discard_inputs().await;
            }
        }
    }

    fn can_adjust(&self) -> bool {
        !self
            .paused
            .as_ref()
            .is_some_and(|paused| paused.load(Ordering::Relaxed))
            && self.is_enabled()
    }

    fn is_enabled(&self) -> bool {
        self.enabled
            .as_ref()
            .is_none_or(|enabled| enabled.load(Ordering::Relaxed))
    }

    async fn adjust_inner(&mut self, luma: u8) {
        match &mut self.inner {
            Inner::Adaptive(c) => c.adjust(luma).await,
            Inner::Manual(c) => c.adjust(luma).await,
        }
    }

    async fn discard_inputs(&mut self) {
        if let Inner::Adaptive(controller) = &mut self.inner {
            controller.discard_inputs().await;
        }
    }

    pub(crate) fn discard_stale_inputs(&mut self) {
        if let Inner::Adaptive(controller) = &mut self.inner {
            controller.discard_stale_inputs();
        }
        for controller in &mut self.additional {
            controller.discard_stale_inputs();
        }
    }
}

fn distance(scale: Scale, als: u64, luma: u8, entry: &Entry) -> f64 {
    let als_distance = scale.coordinate(als) - scale.coordinate(entry.als);
    let luma_distance = (luma as f64 - entry.luma as f64) / LUMA_SCALE;
    als_distance.hypot(luma_distance)
}

fn interpolate_raw(entries: &[Entry], scale: Scale, als: u64, luma: u8) -> Option<f64> {
    let points = entries
        .iter()
        .filter_map(|entry| {
            let distance = distance(scale, als, luma, entry);
            (distance <= MAX_DISTANCE).then_some((entry.brightness as f64, distance))
        })
        .collect::<Vec<_>>();
    if let Some((brightness, _)) = points.iter().find(|(_, distance)| *distance == 0.0) {
        return Some(*brightness);
    }
    let total_weight = points
        .iter()
        .map(|(_, distance)| 1.0 / distance.powi(2))
        .sum::<f64>();
    if total_weight == 0.0 {
        return None;
    }
    let prediction = points
        .iter()
        .map(|(brightness, distance)| brightness / distance.powi(2) / total_weight)
        .sum::<f64>();
    Some(prediction)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_rounds_to_nearest_brightness() {
        let entries = vec![
            Entry::new(3, 0, 2),
            Entry::new(18, 0, 1),
            Entry::new(747, 0, 0),
        ];

        assert_eq!(
            interpolate_raw(&entries, Scale::Lux, 5, 0).map(f64::round),
            Some(2.0)
        );
    }
}
