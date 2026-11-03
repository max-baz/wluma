use std::time::{Duration, Instant};

const MIN_TAU: Duration = Duration::from_millis(200);
const MAX_TAU: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scalar {
    value: f64,
    destination: u64,
    updated: Instant,
    tau: Duration,
}

impl Scalar {
    pub fn new(current: u64, destination: u64, tau: Duration, now: Instant) -> Self {
        Self {
            value: current as f64,
            destination,
            updated: now,
            tau,
        }
    }

    pub fn destination(&self) -> u64 {
        self.destination
    }

    pub fn output(&self) -> u64 {
        self.value.round() as u64
    }

    pub fn settled(&self) -> bool {
        self.output() == self.destination
    }

    pub fn advance(&mut self, now: Instant) -> u64 {
        let elapsed = now.saturating_duration_since(self.updated);
        self.updated = self.updated.max(now);
        let alpha = -(-elapsed.as_secs_f64() / self.tau.as_secs_f64()).exp_m1();
        self.value += (self.destination as f64 - self.value) * alpha;
        if self.settled() {
            self.value = self.destination as f64;
        }
        self.output()
    }

    pub fn retarget(&mut self, destination: u64, now: Instant) {
        if destination != self.destination {
            self.advance(now);
            self.destination = destination;
        }
    }

    pub fn set_tau(&mut self, tau: Duration, now: Instant) {
        self.advance(now);
        self.tau = tau;
    }

    pub fn rebase(&mut self, current: u64, now: Instant) {
        self.value = current as f64;
        self.updated = now;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoTiming {
    estimate: Option<Duration>,
}

impl IoTiming {
    pub fn record(&mut self, cost: Duration) {
        self.estimate = Some(match self.estimate {
            Some(previous) => previous - previous / 4 + cost / 4,
            None => cost,
        });
    }

    pub fn tau(&self, cadence: Duration) -> Duration {
        self.estimate
            .unwrap_or_default()
            .max(cadence)
            .saturating_mul(8)
            .clamp(MIN_TAU, MAX_TAU)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
    }

    #[test]
    fn stable_target_uses_elapsed_time_and_finishes_exactly() {
        let now = Instant::now();
        let tau = Duration::from_millis(200);
        let mut scalar = Scalar::new(20, 80, tau, now);
        scalar.advance(now + tau);
        close(scalar.value, 80.0 - 60.0 / std::f64::consts::E);
        assert_eq!(80, scalar.advance(now + Duration::from_secs(1)));
        assert_eq!(80.0, scalar.value);
        assert!(scalar.settled());
        assert_eq!(80, scalar.advance(now + Duration::from_secs(10)));
    }

    #[test]
    fn retarget_accounts_for_old_target_without_a_value_jump() {
        let now = Instant::now();
        let tau = Duration::from_millis(200);
        let mut scalar = Scalar::new(0, 100, tau, now);
        let changed = now + Duration::from_millis(180);
        let expected = 100.0 * (1.0 - (-0.9_f64).exp());
        scalar.retarget(10, changed);
        close(scalar.value, expected);
        assert_eq!(expected.round() as u64, scalar.advance(changed));
        scalar.advance(changed + Duration::from_millis(20));
        close(scalar.value, 10.0 + (expected - 10.0) * (-0.1_f64).exp());
        assert!(scalar.output() > 50);
    }

    #[test]
    fn repeated_predictions_and_frequent_retargets_keep_progress() {
        let now = Instant::now();
        let mut scalar = Scalar::new(0, 99, MIN_TAU, now);
        let original = scalar;
        scalar.retarget(99, now + Duration::from_millis(10));
        assert_eq!(original, scalar);
        for ms in (10..=200).step_by(10) {
            let at = now + Duration::from_millis(ms);
            let mut previous = scalar;
            previous.advance(at);
            scalar.retarget(if ms % 20 == 0 { 99 } else { 100 }, at);
            close(scalar.value, previous.value);
            scalar.advance(at);
        }
        assert!(scalar.output() >= 62);
    }

    #[test]
    fn irregular_subdivisions_match_one_elapsed_interval() {
        let now = Instant::now();
        let mut single = Scalar::new(100, 0, MIN_TAU, now);
        let mut divided = single;
        for ms in [1, 7, 19, 53, 54, 121, 199, 311] {
            divided.advance(now + Duration::from_millis(ms));
        }
        single.advance(now + Duration::from_millis(311));
        close(single.value, divided.value);
    }

    #[test]
    fn fractional_one_unit_changes_finish_in_both_directions() {
        let now = Instant::now();
        for (from, to) in [(0, 1), (1, 0), (50, 51), (51, 50)] {
            let mut scalar = Scalar::new(from, to, MIN_TAU, now);
            for ms in (10..=130).step_by(10) {
                assert_eq!(from, scalar.advance(now + Duration::from_millis(ms)));
            }
            assert_eq!(to, scalar.advance(now + Duration::from_millis(140)));
            assert_eq!(to as f64, scalar.value);
        }
    }

    #[test]
    fn idle_time_is_not_spent_on_a_new_destination() {
        let now = Instant::now();
        let mut scalar = Scalar::new(20, 20, MIN_TAU, now);
        let later = now + Duration::from_secs(60);
        scalar.retarget(80, later);
        assert_eq!(20, scalar.advance(later));
        assert!(scalar.advance(later + Duration::from_millis(20)) > 20);
    }

    #[test]
    fn tau_updates_apply_only_to_future_elapsed_time() {
        let now = Instant::now();
        let mut scalar = Scalar::new(0, 100, MIN_TAU, now);
        scalar.set_tau(Duration::from_secs(1), now + MIN_TAU);
        close(scalar.value, 100.0 * (1.0 - (-1.0_f64).exp()));
        scalar.advance(now + Duration::from_millis(400));
        close(scalar.value, 100.0 * (1.0 - (-1.2_f64).exp()));
    }

    #[test]
    fn rebase_discards_failed_progress_but_preserves_destination() {
        let now = Instant::now();
        let mut scalar = Scalar::new(20, 80, MIN_TAU, now);
        scalar.advance(now + Duration::from_secs(10));
        assert!(scalar.settled());
        scalar.rebase(20, now + Duration::from_secs(60));
        assert_eq!(80, scalar.destination());
        assert!(!scalar.settled());
        assert_eq!(20, scalar.advance(now + Duration::from_secs(60)));
    }

    #[test]
    fn timing_uses_successful_cost_ewma_cadence_and_bounds() {
        let mut timing = IoTiming::default();
        assert_eq!(MIN_TAU, timing.tau(Duration::from_millis(16)));
        assert_eq!(
            Duration::from_millis(400),
            timing.tau(Duration::from_millis(50))
        );
        timing.record(Duration::from_millis(20));
        assert_eq!(Some(Duration::from_millis(20)), timing.estimate);
        assert_eq!(MIN_TAU, timing.tau(Duration::from_millis(16)));
        timing.record(Duration::from_millis(180));
        assert_eq!(Some(Duration::from_millis(60)), timing.estimate);
        assert_eq!(
            Duration::from_millis(480),
            timing.tau(Duration::from_millis(16))
        );
        timing.record(Duration::MAX);
        assert_eq!(MAX_TAU, timing.tau(Duration::from_millis(16)));
    }
}
