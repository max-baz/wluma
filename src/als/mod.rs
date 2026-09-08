use anyhow::Result;
use std::time::Duration;

pub mod applesmc;
pub mod auto;
pub mod controller;
pub mod external;
pub mod iio;
pub mod none;
mod sensor_proxy;
pub mod time;
pub mod webcam;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const LINEAR_COORDINATE_SCALE: f64 = 20.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    Lux,
    Linear,
}

impl Scale {
    pub fn coordinate(self, value: u64) -> f64 {
        match self {
            Self::Lux => (value as f64 + 1.0).log10(),
            Self::Linear => value as f64 / LINEAR_COORDINATE_SCALE,
        }
    }

    pub fn value(self, coordinate: f64) -> u64 {
        match self {
            Self::Lux => (10.0_f64.powf(coordinate) - 1.0).round() as u64,
            Self::Linear => (coordinate * LINEAR_COORDINATE_SCALE)
                .round()
                .clamp(0.0, 100.0) as u64,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub value: u64,
    pub stable: bool,
}

#[allow(clippy::large_enum_variant)]
pub enum Als {
    Auto(auto::Als),
    Webcam(webcam::Als),
    External(external::Als),
    Iio(iio::Als),
    Applesmc(applesmc::Als),
    Time(time::Als),
    None(none::Als),
}

impl Als {
    pub async fn get(&self) -> Result<Option<u64>> {
        match self {
            Self::Auto(als) => als.get().await,
            Self::Webcam(als) => als.get().await,
            Self::External(als) => als.get().await.map(Some),
            Self::Iio(als) => als.get().await.map(Some),
            Self::Applesmc(als) => als.get().await.map(Some),
            Self::Time(als) => als.get().await.map(Some),
            Self::None(als) => als.get().await.map(Some),
        }
    }

    pub async fn kind(&self) -> &'static str {
        match self {
            Self::Auto(als) => als.kind().await,
            Self::External(_) => "external",
            Self::Iio(als) => als.backend_name(),
            Self::Applesmc(_) => "applesmc",
            Self::Webcam(_) => "webcam",
            Self::Time(_) => "time",
            Self::None(_) => "none",
        }
    }

    pub fn poll_interval(&self) -> Duration {
        match self {
            Self::Auto(als) => als.poll_interval(),
            Self::External(als) => als.poll_interval(),
            Self::Iio(als) => als.poll_interval(),
            Self::Applesmc(als) => als.poll_interval(),
            Self::Webcam(_) | Self::Time(_) | Self::None(_) => DEFAULT_POLL_INTERVAL,
        }
    }

    pub fn scale(&self) -> Scale {
        match self {
            Self::Auto(_) => Scale::Lux,
            Self::External(als) => als.scale(),
            Self::Iio(_) | Self::Applesmc(_) => Scale::Lux,
            Self::Webcam(_) | Self::Time(_) | Self::None(_) => Scale::Linear,
        }
    }

    pub fn generation(&self) -> u64 {
        match self {
            Self::Auto(als) => als.generation(),
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lux_scale_spans_orders_of_magnitude() {
        assert_eq!(0.0, Scale::Lux.coordinate(0));
        assert!((Scale::Lux.coordinate(100_000) - 5.0).abs() < 0.001);
    }

    #[test]
    fn linear_scale_preserves_value() {
        assert_eq!(2.1, Scale::Linear.coordinate(42));
        assert_eq!(42, Scale::Linear.value(2.1));
        assert_eq!(100, Scale::Linear.value(7.5));
    }
}
