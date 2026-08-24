use smol::Timer;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Capturer {}

impl Capturer {
    pub async fn run(
        &mut self,
        _output_name: &str,
        mut controller: crate::predictor::Controller,
        active: Arc<AtomicBool>,
    ) {
        while active.load(Ordering::Relaxed) {
            controller.adjust(0).await;
            let deadline = Instant::now() + super::PREDICTION_INTERVAL;
            while active.load(Ordering::Relaxed) && Instant::now() < deadline {
                Timer::after(
                    Duration::from_millis(100)
                        .min(deadline.saturating_duration_since(Instant::now())),
                )
                .await;
            }
        }
    }
}
