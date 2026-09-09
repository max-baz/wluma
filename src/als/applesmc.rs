use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use smol::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PLATFORM_DEVICES: &str = "/sys/devices/platform";
const DEVICE_PREFIX: &str = "applesmc.";
const SENSOR_FILE: &str = "light";
const POLL_INTERVAL: Duration = Duration::from_millis(800);

/// Ambient light from the Apple SMC on Intel MacBooks.
///
/// The `applesmc` platform driver exposes the sensor as
/// `/sys/devices/platform/applesmc.<id>/light`, formatted as `(left,right)`.
/// The brighter slot is used as a raw, unitless illuminance reading.
pub struct Als {
    path: PathBuf,
}

impl Als {
    pub async fn new(path: Option<&str>) -> Result<Self> {
        let path = match path {
            Some(path) => PathBuf::from(path),
            None => discover(Path::new(PLATFORM_DEVICES)).await?,
        };
        let als = Self { path };
        als.get_raw().await?;
        log::info!(
            "Using Apple SMC ambient light sensor '{}'",
            als.path.display()
        );
        Ok(als)
    }

    pub async fn get(&self) -> Result<u64> {
        let value = self.get_raw().await?;
        log::trace!("ALS (applesmc): {value}");
        Ok(value)
    }

    pub fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }

    async fn get_raw(&self) -> Result<u64> {
        let content = fs::read_to_string(&self.path).await.with_context(|| {
            format!(
                "Unable to read Apple SMC ambient light sensor '{}'",
                self.path.display()
            )
        })?;
        parse(&content)
    }
}

async fn discover(platform_devices: &Path) -> Result<PathBuf> {
    let mut entries = fs::read_dir(platform_devices)
        .await
        .with_context(|| format!("Unable to enumerate '{}'", platform_devices.display()))?;
    while let Some(entry) = entries.next().await {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(DEVICE_PREFIX)
        {
            continue;
        }
        let path = entry.path().join(SENSOR_FILE);
        if fs::metadata(&path).await.is_ok() {
            return Ok(path);
        }
    }
    Err(anyhow!(
        "No Apple SMC ambient light sensor found under '{}'",
        platform_devices.display()
    ))
}

fn parse(content: &str) -> Result<u64> {
    let trimmed = content.trim();
    let inner = trimmed
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or_else(|| anyhow!("Unexpected Apple SMC light reading '{trimmed}'"))?;
    let mut values = inner.split(',').map(|value| {
        value
            .trim()
            .parse::<u64>()
            .map_err(|error| anyhow!("Unexpected Apple SMC light value '{value}': {error}"))
    });
    let left = values
        .next()
        .ok_or_else(|| anyhow!("Unexpected Apple SMC light reading '{trimmed}'"))??;
    let right = values
        .next()
        .ok_or_else(|| anyhow!("Unexpected Apple SMC light reading '{trimmed}'"))??;
    if values.next().is_some() {
        return Err(anyhow!("Unexpected Apple SMC light reading '{trimmed}'"));
    }
    Ok(left.max(right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("wluma-{name}-{unique}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn parses_both_sensor_formats() {
        assert_eq!(11, parse("(11,0)\n").unwrap());
        assert_eq!(17, parse("(4,17)").unwrap());
        assert_eq!(0, parse("(0,0)").unwrap());
        assert_eq!(255, parse(" ( 255 , 0 ) ").unwrap());
    }

    #[test]
    fn rejects_unexpected_readings() {
        assert!(parse("").is_err());
        assert!(parse("11").is_err());
        assert!(parse("(1)").is_err());
        assert!(parse("(-3,-1)").is_err());
        assert!(parse("(a,b)").is_err());
        assert!(parse("(1,2,3)").is_err());
    }

    #[test]
    fn reads_a_configured_sensor_file() {
        smol::block_on(async {
            let dir = temp_dir("applesmc-file");
            let path = dir.join("light");
            std::fs::write(&path, "(42,0)\n").unwrap();

            let als = Als::new(Some(path.to_str().unwrap())).await.unwrap();
            assert_eq!(42, als.get().await.unwrap());

            std::fs::write(&path, "(7,9)\n").unwrap();
            assert_eq!(9, als.get().await.unwrap());
            std::fs::remove_dir_all(dir).unwrap();
        });
    }

    #[test]
    fn rejects_a_missing_or_malformed_sensor_at_startup() {
        smol::block_on(async {
            let dir = temp_dir("applesmc-invalid");
            assert!(Als::new(Some(dir.join("missing").to_str().unwrap()))
                .await
                .is_err());
            let path = dir.join("light");
            std::fs::write(&path, "garbage\n").unwrap();
            assert!(Als::new(Some(path.to_str().unwrap())).await.is_err());
            std::fs::remove_dir_all(dir).unwrap();
        });
    }

    #[test]
    fn discovers_the_platform_device() {
        smol::block_on(async {
            let dir = temp_dir("applesmc-discover");
            std::fs::create_dir_all(dir.join("other.0")).unwrap();
            std::fs::create_dir_all(dir.join("applesmc.768")).unwrap();
            assert!(discover(&dir).await.is_err());

            std::fs::write(dir.join("applesmc.768/light"), "(11,0)\n").unwrap();
            assert_eq!(
                dir.join("applesmc.768/light"),
                discover(&dir).await.unwrap()
            );
            std::fs::remove_dir_all(dir).unwrap();
        });
    }
}
