use crate::device_file::read;
use anyhow::{anyhow, Error, Result};
use futures_util::{StreamExt, TryFutureExt};
use smol::fs::{self, File};
use smol::lock::Mutex;
use std::ops::DerefMut;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SENSOR_PROXY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DEFAULT_SYSFS_POLL_INTERVAL: Duration = Duration::from_millis(800);
const MIN_SAMPLING_FREQUENCY: f64 = 10.0;
const CHANNELS: [&str; 5] = [
    "in_illuminance",
    "in_illuminance0",
    "in_illuminance_clear",
    "in_intensity_clear",
    "in_intensity_both",
];

struct Channel {
    value: Mutex<File>,
    scale: f64,
    offset: f64,
}

#[allow(clippy::large_enum_variant)]
enum SensorType {
    Illuminance {
        channel: Channel,
        poll_interval: Duration,
    },
    Rgb {
        red: Mutex<File>,
        green: Mutex<File>,
        blue: Mutex<File>,
        poll_interval: Duration,
    },
}

enum Source {
    SensorProxy(Mutex<super::sensor_proxy::Sensor>),
    Sysfs(Box<SensorType>),
}

pub struct Als {
    source: Source,
}

impl Als {
    pub async fn new(base_path: Option<&str>) -> Result<Self> {
        let source = match smol::unblock(super::sensor_proxy::Sensor::new).await {
            Ok(sensor) => {
                log::info!("Using iio-sensor-proxy for ambient light sensing");
                Source::SensorProxy(Mutex::new(sensor))
            }
            Err(proxy_error) => {
                log::debug!("Unable to use iio-sensor-proxy: {proxy_error}");
                let base_path = base_path.ok_or_else(|| {
                    anyhow!(
                        "Unable to use iio-sensor-proxy and no IIO path is configured: {proxy_error}"
                    )
                })?;
                let sensor = find_sensor(base_path).await?;
                log::info!(
                    "Using IIO sysfs for ambient light sensing (using iio-sensor-proxy is recommended)"
                );
                Source::Sysfs(Box::new(sensor))
            }
        };

        Ok(Self { source })
    }

    pub async fn get(&self) -> Result<u64> {
        let value = self.get_raw().await?;
        log::trace!("ALS (iio): {value}");
        Ok(value)
    }

    pub fn poll_interval(&self) -> Duration {
        match &self.source {
            Source::SensorProxy(_) => SENSOR_PROXY_POLL_INTERVAL,
            Source::Sysfs(sensor) => match sensor.as_ref() {
                SensorType::Illuminance { poll_interval, .. }
                | SensorType::Rgb { poll_interval, .. } => *poll_interval,
            },
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match &self.source {
            Source::SensorProxy(_) => "iio-sensor-proxy",
            Source::Sysfs(_) => "iio-sysfs",
        }
    }

    async fn get_raw(&self) -> Result<u64> {
        match &self.source {
            Source::SensorProxy(sensor) => Ok(sensor.lock().await.get_raw().await),
            Source::Sysfs(sensor) => Ok((match sensor.as_ref() {
                SensorType::Illuminance { channel, .. } => read_channel(channel).await?,
                SensorType::Rgb {
                    red, green, blue, ..
                } => {
                    -0.32466 * read(red.lock().await.deref_mut()).await?
                        + 1.57837 * read(green.lock().await.deref_mut()).await?
                        + -0.73191 * read(blue.lock().await.deref_mut()).await?
                }
            })
            .round() as u64),
        }
    }
}

async fn find_sensor(base_path: &str) -> Result<SensorType> {
    smol::fs::read_dir(base_path)
        .await
        .map_err(|e| anyhow!("Can't enumerate iio devices: {e}"))?
        .filter_map(|r| async { r.ok() })
        .filter_map(|entry| async move {
            let path = entry.path();
            parse_polling_sensor(path.clone())
                .await
                .ok()
                .map(|sensor| (path, sensor))
        })
        .boxed()
        .next()
        .await
        .map(|(path, sensor)| {
            log::debug!("Using IIO ambient light sensor '{}'", path.display());
            sensor
        })
        .ok_or_else(|| anyhow!("No supported IIO ambient light sensor found"))
}

async fn parse_polling_sensor(path: PathBuf) -> Result<SensorType> {
    let sensor = parse_processed(&path)
        .or_else(|_| parse_raw(&path))
        .or_else(|_| parse_rgb(&path))
        .await?;
    fix_sampling_frequencies(&path).await;
    Ok(sensor)
}

async fn parse_processed(path: &Path) -> Result<SensorType> {
    for name in CHANNELS {
        if let Ok(value) = open_file(path, &format!("{name}_input")).await {
            return Ok(SensorType::Illuminance {
                channel: Channel {
                    value: Mutex::new(value),
                    scale: 1.0,
                    offset: 0.0,
                },
                poll_interval: integration_time(path).await,
            });
        }
    }
    Err(anyhow!("No processed illuminance channel"))
}

async fn parse_raw(path: &Path) -> Result<SensorType> {
    for name in CHANNELS {
        if let Ok(channel) = raw_channel(path, name).await {
            return Ok(SensorType::Illuminance {
                channel,
                poll_interval: integration_time(path).await,
            });
        }
    }
    Err(anyhow!("No raw illuminance channel"))
}

async fn parse_rgb(path: &Path) -> Result<SensorType> {
    Ok(SensorType::Rgb {
        red: Mutex::new(open_file(path, "in_intensity_red_raw").await?),
        green: Mutex::new(open_file(path, "in_intensity_green_raw").await?),
        blue: Mutex::new(open_file(path, "in_intensity_blue_raw").await?),
        poll_interval: integration_time(path).await,
    })
}

async fn raw_channel(path: &Path, name: &str) -> Result<Channel> {
    let value = open_file(path, &format!("{name}_raw")).await?;
    let generic = if name.starts_with("in_illuminance") {
        "in_illuminance"
    } else {
        "in_intensity"
    };
    let mut scale = channel_attribute(path, name, generic, "scale")
        .await
        .unwrap_or(1.0);
    if scale == 0.0 {
        scale = 1.0;
    }
    let offset = channel_attribute(path, name, generic, "offset")
        .await
        .unwrap_or(0.0);
    Ok(Channel {
        value: Mutex::new(value),
        scale,
        offset,
    })
}

async fn channel_attribute(path: &Path, name: &str, generic: &str, attribute: &str) -> Result<f64> {
    if let Ok(value) = read_attribute(path, &format!("{name}_{attribute}")).await {
        return Ok(value);
    }
    read_attribute(path, &format!("{generic}_{attribute}")).await
}

async fn read_channel(channel: &Channel) -> Result<f64> {
    Ok((read(channel.value.lock().await.deref_mut()).await? + channel.offset) * channel.scale)
}

async fn integration_time(path: &Path) -> Duration {
    for channel in CHANNELS {
        if let Ok(value) = read_attribute(path, &format!("{channel}_integration_time")).await {
            return Duration::try_from_secs_f64(value).unwrap_or(DEFAULT_SYSFS_POLL_INTERVAL);
        }
    }
    DEFAULT_SYSFS_POLL_INTERVAL
}

async fn fix_sampling_frequencies(path: &Path) {
    let Ok(mut entries) = fs::read_dir(path).await else {
        return;
    };
    while let Some(Ok(entry)) = entries.next().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with("sampling_frequency") {
            continue;
        }
        let Ok(current) = read_attribute(path, &name).await else {
            continue;
        };
        if current >= MIN_SAMPLING_FREQUENCY {
            continue;
        }
        let desired = available_sampling_frequency(path, &name).await;
        if let Err(error) = fs::write(entry.path(), desired.to_string()).await {
            log::warn!(
                "Unable to set IIO sampling frequency '{}': {error}",
                entry.path().display()
            );
        }
    }
}

async fn available_sampling_frequency(path: &Path, name: &str) -> f64 {
    let available = fs::read_to_string(path.join(format!("{name}_available")))
        .await
        .unwrap_or_default();
    select_sampling_frequency(&available)
}

fn select_sampling_frequency(available: &str) -> f64 {
    let mut higher = f64::INFINITY;
    let mut lower: f64 = 0.0;
    for value in available.split_whitespace() {
        let Ok(value) = value.parse::<f64>() else {
            break;
        };
        if value == 0.0 {
            continue;
        }
        if value >= MIN_SAMPLING_FREQUENCY {
            higher = higher.min(value);
        } else {
            lower = lower.max(value);
        }
    }
    if higher.is_finite() {
        higher
    } else if lower > 0.0 {
        lower
    } else {
        MIN_SAMPLING_FREQUENCY
    }
}

async fn read_attribute(path: &Path, name: &str) -> Result<f64> {
    let mut file = open_file(path, name).await?;
    read(&mut file).await
}

async fn open_file(path: &Path, name: &str) -> Result<File> {
    File::open(path.join(name)).await.map_err(Error::msg)
}
