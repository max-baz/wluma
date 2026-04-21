use anyhow::{anyhow, Result};
use ddc_hi::{Ddc, Display, FeatureCode};
use itertools::Itertools;
use lazy_static::lazy_static;
use smol::lock::Mutex;
use std::thread;
use std::time::Duration;

lazy_static! {
    static ref DDC_MUTEX: Mutex<()> = Mutex::new(());
}

const DDC_BRIGHTNESS_FEATURE: FeatureCode = 0x10;
const DDC_WAITING_SLEEP_MS: u64 = 500;
const DDC_TRANSITION_STEP_MS: u64 = 50;
const DDC_RETRIES: usize = 3;
const DDC_RETRY_SLEEP_MS: u64 = 40;

pub struct DdcUtil {
    display: Mutex<Display>,
    min_brightness: u64,
    max_brightness: u64,
}

impl DdcUtil {
    pub fn new(name: &str, min_brightness: u64) -> Result<Self> {
        let mut display = find_display_by_name(name, true)
            .or_else(|| find_display_by_name(name, false))
            .ok_or(anyhow!("Unable to find display"))?;
        let max_brightness = get_max_brightness_with_retry(&mut display)?;

        Ok(Self {
            display: Mutex::new(display),
            min_brightness,
            max_brightness,
        })
    }

    pub async fn get(&mut self) -> Result<u64> {
        let _lock = DDC_MUTEX.lock().await;
        get_brightness_with_retry(self.display.get_mut())
    }

    pub async fn set(&mut self, value: u64) -> Result<u64> {
        let _lock = DDC_MUTEX.lock().await;
        let value = value.clamp(self.min_brightness, self.max_brightness);
        set_brightness_with_retry(self.display.get_mut(), value)
    }

    pub fn waiting_sleep_ms(&self) -> u64 {
        DDC_WAITING_SLEEP_MS
    }

    pub fn transition_step_ms(&self) -> u64 {
        DDC_TRANSITION_STEP_MS
    }
}

fn get_max_brightness(display: &mut Display) -> Result<u64> {
    Ok(display
        .handle
        .get_vcp_feature(DDC_BRIGHTNESS_FEATURE)?
        .maximum() as u64)
}

fn get_max_brightness_with_retry(display: &mut Display) -> Result<u64> {
    retry_ddc("read max brightness", || get_max_brightness(display))
}

fn get_brightness_with_retry(display: &mut Display) -> Result<u64> {
    retry_ddc("read brightness", || {
        Ok(display
            .handle
            .get_vcp_feature(DDC_BRIGHTNESS_FEATURE)?
            .value() as u64)
    })
}

fn set_brightness_with_retry(display: &mut Display, value: u64) -> Result<u64> {
    retry_ddc("set brightness", || {
        display
            .handle
            .set_vcp_feature(DDC_BRIGHTNESS_FEATURE, value as u16)?;
        Ok(value)
    })
}

fn retry_ddc<T, F>(operation: &str, mut action: F) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    for attempt in 1..=DDC_RETRIES {
        match action() {
            Ok(result) => return Ok(result),
            Err(err) if attempt < DDC_RETRIES => {
                log::debug!(
                    "Failed to {} over direct DDC (attempt {}/{}): {:?}",
                    operation,
                    attempt,
                    DDC_RETRIES,
                    err
                );
                thread::sleep(Duration::from_millis(DDC_RETRY_SLEEP_MS));
            }
            Err(err) => return Err(err),
        }
    }

    unreachable!("retry loop always returns before falling through")
}

fn find_display_by_name(name: &str, check_caps: bool) -> Option<Display> {
    let displays = ddc_hi::Display::enumerate()
        .into_iter()
        .filter_map(|mut display| {
            let caps = if check_caps {
                display.update_capabilities()
            } else {
                Ok(())
            };
            caps.ok().map(|_| {
                let empty = "".to_string();
                let merged = format!(
                    "{} {} {}",
                    display.info.model_name.as_ref().unwrap_or(&empty),
                    display.info.serial_number.as_ref().unwrap_or(&empty),
                    display.info.manufacturer_id.as_ref().unwrap_or(&empty)
                );
                (merged, display)
            })
        })
        .collect_vec();

    log::debug!(
        "Discovered displays (check_caps={}): {:?}",
        check_caps,
        displays.iter().map(|(name, _)| name).collect_vec()
    );

    displays.into_iter().find_map(|(merged, display)| {
        merged
            .contains(name)
            .then(|| {
                log::debug!(
                    "Using display '{}' for config '{}' (check_caps={})",
                    merged,
                    name,
                    check_caps
                );
            })
            .map(|_| display)
    })
}
