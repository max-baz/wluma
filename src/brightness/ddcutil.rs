use anyhow::{anyhow, Context, Result};
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::blocking::Connection;
use ddc_hi::{Ddc, Display, FeatureCode};
use itertools::Itertools;

const DESTINATION: &str = "com.ddcutil.DdcutilService";
const PATH: &str = "/com/ddcutil/DdcutilObject";
const INTERFACE: &str = "com.ddcutil.DdcutilInterface";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const DDC_BRIGHTNESS_FEATURE: FeatureCode = 0x10;
const DDC_WAITING_SLEEP_MS: u64 = 500;
const DDC_TRANSITION_STEP_MS: u64 = 50;

type DetectedDisplay = (i32, i32, i32, String, String, String, u16, String, u32);

enum Backend {
    Service(Service),
    Raw(Display),
}

struct Service {
    connection: Connection,
    edid: String,
}

pub struct DdcUtil {
    backend: Backend,
    min_brightness: u64,
    max_brightness: u64,
}

impl DdcUtil {
    pub fn new(identifier: &str, min_brightness: u64) -> Result<Self> {
        match Service::new(identifier) {
            Ok((service, max_brightness)) => {
                log::info!("Using ddcutil-service over D-Bus for DDC display '{identifier}'");
                Ok(Self {
                    backend: Backend::Service(service),
                    min_brightness,
                    max_brightness: (max_brightness as u64).max(min_brightness),
                })
            }
            Err(service_error) => {
                log::debug!(
                    "Unable to use ddcutil-service for DDC display '{identifier}': {service_error:#}"
                );
                let mut display = find_raw_display(identifier)
                    .ok_or_else(|| anyhow!("Unable to find DDC display '{identifier}'"))?;
                let max_brightness = display
                    .handle
                    .get_vcp_feature(DDC_BRIGHTNESS_FEATURE)
                    .context("Unable to read brightness over raw DDC")?
                    .maximum() as u64;
                log::info!(
                    "Using raw DDC for display '{identifier}' (using ddcutil-service is recommended)"
                );
                Ok(Self {
                    backend: Backend::Raw(display),
                    min_brightness,
                    max_brightness: max_brightness.max(min_brightness),
                })
            }
        }
    }

    pub async fn get(&mut self) -> Result<u64> {
        let value = match &mut self.backend {
            Backend::Service(service) => {
                let (current, maximum) = service.get_brightness()?;
                self.max_brightness = (maximum as u64).max(self.min_brightness);
                current as u64
            }
            Backend::Raw(display) => display
                .handle
                .get_vcp_feature(DDC_BRIGHTNESS_FEATURE)
                .context("Unable to read brightness over raw DDC")?
                .value() as u64,
        };
        Ok(value.clamp(self.min_brightness, self.max_brightness))
    }

    pub async fn set(&mut self, value: u64) -> Result<u64> {
        let value = value.clamp(self.min_brightness, self.max_brightness);
        match &mut self.backend {
            Backend::Service(service) => service.set_brightness(value as u16)?,
            Backend::Raw(display) => display
                .handle
                .set_vcp_feature(DDC_BRIGHTNESS_FEATURE, value as u16)
                .context("Unable to set brightness over raw DDC")?,
        }
        Ok(value)
    }

    pub fn min(&self) -> u64 {
        self.min_brightness
    }

    pub fn max(&self) -> u64 {
        self.max_brightness
    }

    pub fn waiting_sleep_ms(&self) -> u64 {
        DDC_WAITING_SLEEP_MS
    }

    pub fn transition_step_ms(&self) -> u64 {
        DDC_TRANSITION_STEP_MS
    }
}

impl Service {
    fn new(identifier: &str) -> Result<(Self, u16)> {
        let connection =
            Connection::new_session().context("Unable to connect to the session bus")?;
        let proxy = connection.with_proxy(DESTINATION, PATH, TIMEOUT);
        let version: String = proxy
            .get(INTERFACE, "ServiceInterfaceVersion")
            .context("ddcutil-service is unavailable")?;
        if version.split('.').next() != Some("1") {
            return Err(anyhow!(
                "Unsupported ddcutil-service interface version '{version}'"
            ));
        }

        let mut display = match list_displays(&connection, "ListDetected") {
            Ok(displays) => displays
                .into_iter()
                .find(|display| display_matches(display, identifier)),
            Err(error) => {
                log::debug!("Unable to list displays through ddcutil-service: {error:#}");
                None
            }
        };
        if display.is_none() {
            display = list_displays(&connection, "Detect")?
                .into_iter()
                .find(|display| display_matches(display, identifier));
        }
        let display = display.ok_or_else(|| {
            anyhow!("ddcutil-service could not find a display matching '{identifier}'")
        })?;
        let mut service = Self {
            connection,
            edid: display.7,
        };
        let (_, maximum) = service.get_brightness()?;
        Ok((service, maximum))
    }

    fn get_brightness(&mut self) -> Result<(u16, u16)> {
        let proxy = self.connection.with_proxy(DESTINATION, PATH, TIMEOUT);
        let (current, maximum, _, status, message): (u16, u16, String, i32, String) = proxy
            .method_call(
                INTERFACE,
                "GetVcp",
                (-1i32, self.edid.as_str(), DDC_BRIGHTNESS_FEATURE, 0u32),
            )
            .context("Unable to read brightness through ddcutil-service")?;
        service_result(status, &message, "read brightness")?;
        Ok((current, maximum))
    }

    fn set_brightness(&mut self, value: u16) -> Result<()> {
        let proxy = self.connection.with_proxy(DESTINATION, PATH, TIMEOUT);
        let (status, message): (i32, String) = proxy
            .method_call(
                INTERFACE,
                "SetVcp",
                (
                    -1i32,
                    self.edid.as_str(),
                    DDC_BRIGHTNESS_FEATURE,
                    value,
                    0u32,
                ),
            )
            .context("Unable to set brightness through ddcutil-service")?;
        service_result(status, &message, "set brightness")
    }
}

fn list_displays(connection: &Connection, method: &str) -> Result<Vec<DetectedDisplay>> {
    let proxy = connection.with_proxy(DESTINATION, PATH, TIMEOUT);
    let (_, displays, status, message): (i32, Vec<DetectedDisplay>, i32, String) = proxy
        .method_call(INTERFACE, method, (0u32,))
        .with_context(|| format!("Unable to call ddcutil-service {method}"))?;
    service_result(status, &message, "detect displays")?;
    Ok(displays)
}

fn service_result(status: i32, message: &str, operation: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(anyhow!(
            "ddcutil-service failed to {operation}: {message} (status {status})"
        ))
    }
}

fn display_matches(display: &DetectedDisplay, identifier: &str) -> bool {
    format!(
        "{} {} {} {}",
        display.3,
        display.4,
        display.5,
        binary_serial_identifiers(display.8)
    )
    .contains(identifier)
}

fn find_raw_display(identifier: &str) -> Option<Display> {
    let displays = Display::enumerate()
        .into_iter()
        .map(|display| (raw_display_identifiers(&display), display))
        .collect_vec();
    log::debug!(
        "Discovered raw DDC displays: {:?}",
        displays
            .iter()
            .map(|(identifiers, _)| identifiers)
            .collect_vec()
    );
    displays
        .into_iter()
        .find_map(|(identifiers, display)| identifiers.contains(identifier).then_some(display))
}

fn raw_display_identifiers(display: &Display) -> String {
    format!(
        "{} {} {} {}",
        display.info.model_name.as_deref().unwrap_or_default(),
        display.info.serial_number.as_deref().unwrap_or_default(),
        display.info.manufacturer_id.as_deref().unwrap_or_default(),
        display
            .info
            .serial
            .map(binary_serial_identifiers)
            .unwrap_or_default()
    )
}

fn binary_serial_identifiers(value: u32) -> String {
    format!("{} {:#010x}", value, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_service_display_identifiers() {
        let display = (
            1,
            0,
            0,
            "GSM".to_string(),
            "LG ULTRAWIDE".to_string(),
            "504AZER5F964".to_string(),
            0,
            "edid".to_string(),
            0x1234abcd,
        );
        assert!(display_matches(&display, "LG ULTRAWIDE"));
        assert!(display_matches(&display, "504AZER5F964"));
        assert!(display_matches(&display, "305441741"));
        assert!(display_matches(&display, "0x1234abcd"));
        assert!(!display_matches(&display, "missing"));
    }

    #[test]
    fn formats_binary_serial_identifiers() {
        assert_eq!(
            binary_serial_identifiers(0x1234abcd),
            "305441741 0x1234abcd"
        );
    }
}
