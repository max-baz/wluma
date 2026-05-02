use crate::device_file::read;
use anyhow::{anyhow, Error, Result};
use futures_util::{FutureExt, StreamExt, TryFutureExt};
use smol::fs::File;
use smol::lock::Mutex;
use std::collections::HashMap;
use std::ops::DerefMut;
use std::path::{Path, PathBuf};
use SensorType::*;

#[allow(clippy::large_enum_variant)]
enum SensorType {
    Illuminance {
        value: Mutex<File>,
        scale: f64,
        offset: f64,
    },
    Intensity {
        r: Mutex<File>,
        g: Mutex<File>,
        b: Mutex<File>,
    },
}

pub struct Als {
    sensor: SensorType,
    thresholds: HashMap<u64, String>,
}

impl Als {
    pub async fn new(base_path: &str, thresholds: HashMap<u64, String>) -> Result<Self> {
        smol::fs::read_dir(base_path)
            .await
            .map_err(|e| anyhow!("Can't enumerate iio devices: {e}"))?
            .filter_map(|r| async { r.ok() })
            .then(|entry| {
                smol::fs::read_to_string(entry.path().join("name")).map(|name| (name, entry))
            })
            .filter_map(|(name, entry)| async {
                ["als", "acpi-als", "apds9960", "cros-ec-light", "aop-sensors-als"]
                    .contains(&name.unwrap_or_default().trim())
                    .then_some(entry)
            })
            .filter_map(|entry| async move {
                // TODO should probably start from the `parse_illuminance_input` in the next major version
                parse_illuminance_raw(entry.path())
                    .or_else(|_| parse_illuminance_input(entry.path()))
                    .or_else(|_| parse_intensity_raw(entry.path()))
                    .or_else(|_| parse_intensity_rgb(entry.path()))
                    .await
                    .map(Some)
                    .unwrap_or_else(|_| {
                        log::error!("Failed to read sensor '{}'", entry.path().display());
                        None
                    })
            })
            .boxed()
            .next()
            .await
            .map(|sensor| Self { sensor, thresholds })
            .ok_or_else(|| anyhow!("No iio device found"))
    }

    pub async fn get(&self) -> Result<String> {
        let raw = self.get_raw().await?;
        let profile = super::find_profile(raw, &self.thresholds);

        log::trace!("ALS (iio): {} ({})", profile, raw);
        Ok(profile)
    }

    async fn get_raw(&self) -> Result<u64> {
        Ok(match self.sensor {
            Illuminance {
                ref value,
                scale,
                offset,
            } => (read(value.lock().await.deref_mut()).await? + offset) * scale,

            Intensity {
                ref r,
                ref g,
                ref b,
            } => {
                -0.32466 * read(r.lock().await.deref_mut()).await?
                    + 1.57837 * read(g.lock().await.deref_mut()).await?
                    + -0.73191 * read(b.lock().await.deref_mut()).await?
            }
        } as u64)
    }
}

async fn parse_illuminance_raw(path: PathBuf) -> Result<SensorType> {
    Ok(Illuminance {
        value: Mutex::new(
            open_file(&path, "in_illuminance_raw")
                .or_else(|_| open_file(&path, "in_illuminance0_raw"))
                .await?,
        ),
        scale: {
            open_file(&path, "in_illuminance_scale")
                .or_else(|_| open_file(&path, "in_illuminance0_scale"))
                .and_then(move |mut f| async move { read(&mut f).await })
                .await
                .unwrap_or(1_f64)
        },
        offset: {
            open_file(&path, "in_illuminance_offset")
                .or_else(|_| open_file(&path, "in_illuminance0_offset"))
                .and_then(move |mut f| async move { read(&mut f).await })
                .await
                .unwrap_or(0_f64)
        },
    })
}

async fn parse_intensity_raw(path: PathBuf) -> Result<SensorType> {
    async fn try_open_and_read(path: &Path, name: &str) -> Result<f64> {
        let mut f = open_file(path, name).await?;
        read(&mut f).await
    }

    Ok(Illuminance {
        value: Mutex::new(open_file(&path, "in_intensity_both_raw").await?),
        scale: try_open_and_read(&path, "in_intensity_scale")
            .await
            .unwrap_or(1_f64),
        offset: try_open_and_read(&path, "in_intensity_offset")
            .await
            .unwrap_or(0_f64),
    })
}

async fn parse_illuminance_input(path: PathBuf) -> Result<SensorType> {
    Ok(Illuminance {
        value: Mutex::new(
            open_file(&path, "in_illuminance_input")
                .or_else(|_| open_file(&path, "in_illuminance0_input"))
                .await?,
        ),
        scale: 1_f64,
        offset: 0_f64,
    })
}

async fn parse_intensity_rgb(path: PathBuf) -> Result<SensorType> {
    Ok(Intensity {
        r: Mutex::new(open_file(&path, "in_intensity_red_raw").await?),
        g: Mutex::new(open_file(&path, "in_intensity_green_raw").await?),
        b: Mutex::new(open_file(&path, "in_intensity_blue_raw").await?),
    })
}

async fn open_file(path: &Path, name: &str) -> Result<File> {
    File::open(path.join(name)).await.map_err(Error::msg)
}
