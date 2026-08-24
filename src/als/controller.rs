use smol::channel::{Receiver, Sender};
use smol::Timer;
use std::time::{Duration, Instant};

use super::{Als, Reading, Scale};

const FILTER_TIME_CONSTANT_SECONDS: f64 = 0.5;
const STABILIZATION_DURATION: Duration = Duration::from_secs(5);
const LUX_STABILIZATION_COORDINATE_TOLERANCE: f64 = 0.1;
const LUX_STABILIZATION_ABSOLUTE_TOLERANCE: u64 = 3;
const LINEAR_STABILIZATION_TOLERANCE: u64 = 10;

struct Candidate {
    since: Instant,
    min: u64,
    max: u64,
}

impl Candidate {
    fn new(value: u64, now: Instant) -> Self {
        Self {
            since: now,
            min: value,
            max: value,
        }
    }

    fn update(&mut self, scale: Scale, value: u64, now: Instant) -> bool {
        let min = self.min.min(value);
        let max = self.max.max(value);
        if !near(scale, min, max) {
            *self = Self::new(value, now);
            false
        } else {
            self.min = min;
            self.max = max;
            now.duration_since(self.since) >= STABILIZATION_DURATION
        }
    }
}

struct Stabilizer {
    scale: Scale,
    filtered: Option<f64>,
    candidate: Option<Candidate>,
}

impl Stabilizer {
    fn new(scale: Scale) -> Self {
        Self {
            scale,
            filtered: None,
            candidate: None,
        }
    }

    fn observe(&mut self, value: u64, interval: Duration, now: Instant) -> Option<Reading> {
        let Some(filtered) = self.filtered else {
            let stable = match self.candidate.as_mut() {
                Some(candidate) => candidate.update(self.scale, value, now),
                None => {
                    self.candidate = Some(Candidate::new(value, now));
                    false
                }
            };
            if !stable {
                return None;
            }
            self.filtered = Some(value as f64);
            self.candidate = None;
            return Some(self.reading(true));
        };

        if near(self.scale, filtered.round() as u64, value) {
            self.candidate = None;
            let weight = 1.0 - (-interval.as_secs_f64() / FILTER_TIME_CONSTANT_SECONDS).exp();
            self.filtered = Some(filtered + weight * (value as f64 - filtered));
            return Some(self.reading(true));
        }

        let stable = match self.candidate.as_mut() {
            Some(candidate) => candidate.update(self.scale, value, now),
            None => {
                self.candidate = Some(Candidate::new(value, now));
                false
            }
        };
        if stable {
            self.filtered = Some(value as f64);
            self.candidate = None;
            Some(self.reading(true))
        } else {
            Some(self.reading(false))
        }
    }

    fn reading(&self, stable: bool) -> Reading {
        Reading {
            value: self
                .filtered
                .expect("Stable ALS value must be known")
                .round() as u64,
            stable,
        }
    }
}

fn near(scale: Scale, left: u64, right: u64) -> bool {
    match scale {
        Scale::Lux => {
            left.abs_diff(right) <= LUX_STABILIZATION_ABSOLUTE_TOLERANCE
                || (scale.coordinate(left) - scale.coordinate(right)).abs()
                    <= LUX_STABILIZATION_COORDINATE_TOLERANCE
        }
        Scale::Linear => left.abs_diff(right) <= LINEAR_STABILIZATION_TOLERANCE,
    }
}

pub struct Controller {
    als: Als,
    value_txs: Vec<Sender<Reading>>,
    registrations: Receiver<Sender<Reading>>,
    stabilizer: Stabilizer,
    last_observation: Option<Instant>,
    last_reading: Option<Reading>,
    generation: u64,
    status: Option<crate::control::Hub>,
}

impl Controller {
    pub fn new(als: Als, registrations: Receiver<Sender<Reading>>) -> Self {
        let stabilizer = Stabilizer::new(als.scale());
        let generation = als.generation();
        Self {
            als,
            value_txs: Vec::new(),
            registrations,
            stabilizer,
            last_observation: None,
            last_reading: None,
            generation,
            status: None,
        }
    }

    pub fn with_status(mut self, status: crate::control::Hub) -> Self {
        self.status = Some(status);
        self
    }

    pub async fn run(&mut self) {
        loop {
            self.step().await;
        }
    }

    async fn step(&mut self) {
        self.register().await;
        let started = Instant::now();
        match self.als.get().await {
            Ok(Some(value)) => {
                if let Some(status) = &self.status {
                    status.set_als(self.als.kind().await, value);
                }
                let generation = self.als.generation();
                if generation != self.generation {
                    self.generation = generation;
                    self.stabilizer = Stabilizer::new(self.als.scale());
                    self.last_observation = None;
                    let reading = Reading {
                        value,
                        stable: false,
                    };
                    self.publish(reading);
                }
                let now = Instant::now();
                let interval = self
                    .last_observation
                    .replace(now)
                    .map_or(self.als.poll_interval(), |previous| {
                        now.duration_since(previous)
                    });
                if let Some(reading) = self.stabilizer.observe(value, interval, now) {
                    self.publish(reading);
                }
            }
            Ok(None) => {}
            Err(error) => log::error!("Unable to get ALS value: {error:?}"),
        }

        if let Some(remaining) = self.als.poll_interval().checked_sub(started.elapsed()) {
            Timer::after(remaining).await;
        }
    }

    fn publish(&mut self, reading: Reading) {
        self.last_reading = Some(reading);
        self.value_txs.retain(|channel| !channel.is_closed());
        for channel in &self.value_txs {
            let _ = channel.force_send(reading);
        }
    }

    async fn register(&mut self) {
        while let Ok(channel) = self.registrations.try_recv() {
            if let Some(reading) = self.last_reading {
                if channel.send(reading).await.is_err() {
                    continue;
                }
            }
            self.value_txs.push(channel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stability_uses_source_scale() {
        assert!(near(Scale::Lux, 100, 125));
        assert!(!near(Scale::Lux, 100, 130));
        assert!(near(Scale::Lux, 0, 3));
        assert!(near(Scale::Linear, 40, 50));
        assert!(!near(Scale::Linear, 40, 51));
    }

    #[test]
    fn waits_for_initial_stability() {
        let start = Instant::now();
        let mut stabilizer = Stabilizer::new(Scale::Linear);
        assert_eq!(None, stabilizer.observe(100, Duration::ZERO, start));
        assert_eq!(
            Some(Reading {
                value: 105,
                stable: true,
            }),
            stabilizer.observe(105, Duration::ZERO, start + STABILIZATION_DURATION)
        );
    }

    #[test]
    fn holds_the_accepted_value_while_confirming_a_jump() {
        let start = Instant::now();
        let mut stabilizer = Stabilizer::new(Scale::Linear);
        stabilizer.filtered = Some(10.0);
        assert_eq!(
            Some(Reading {
                value: 10,
                stable: false,
            }),
            stabilizer.observe(100, Duration::ZERO, start)
        );
        assert_eq!(
            Some(Reading {
                value: 100,
                stable: true,
            }),
            stabilizer.observe(100, Duration::ZERO, start + STABILIZATION_DURATION)
        );
    }

    #[test]
    fn returning_to_the_accepted_value_ends_confirmation() {
        let start = Instant::now();
        let mut stabilizer = Stabilizer::new(Scale::Linear);
        stabilizer.filtered = Some(10.0);
        stabilizer.observe(100, Duration::ZERO, start);
        assert_eq!(
            Some(Reading {
                value: 10,
                stable: true,
            }),
            stabilizer.observe(10, Duration::ZERO, start + Duration::from_secs(1))
        );
    }

    #[test]
    fn pending_reading_is_replaced_with_the_latest() {
        let (_, registration_rx) = smol::channel::unbounded();
        let mut controller = Controller::new(Als::None(Default::default()), registration_rx);
        let (value_tx, value_rx) = smol::channel::bounded(1);
        controller.value_txs.push(value_tx);
        controller.publish(Reading {
            value: 1,
            stable: true,
        });
        let latest = Reading {
            value: 2,
            stable: false,
        };
        controller.publish(latest);
        assert_eq!(value_rx.try_recv().unwrap(), latest);
    }

    #[test]
    fn new_subscriber_receives_latest_reading() {
        smol::block_on(async {
            let (registration_tx, registration_rx) = smol::channel::unbounded();
            let mut controller = Controller::new(Als::None(Default::default()), registration_rx);
            let reading = Reading {
                value: 42,
                stable: true,
            };
            controller.last_reading = Some(reading);
            let (value_tx, value_rx) = smol::channel::bounded(1);
            registration_tx.send(value_tx).await.unwrap();
            controller.register().await;
            assert_eq!(value_rx.recv().await.unwrap(), reading);
        });
    }
}
