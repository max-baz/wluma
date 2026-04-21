use anyhow::{anyhow, Context, Result};
use ddc_hi::{Ddc, Display, FeatureCode};
use itertools::Itertools;
use lazy_static::lazy_static;
use smol::lock::Mutex;
use std::process::Command;
use std::thread;
use std::time::Duration;

lazy_static! {
    static ref DDC_MUTEX: Mutex<()> = Mutex::new(());
}

const DDC_BRIGHTNESS_FEATURE: FeatureCode = 0x10;
const DDC_DIRECT_WAITING_SLEEP_MS: u64 = 500;
const DDC_DIRECT_TRANSITION_STEP_MS: u64 = 50;
const DDC_CLI_WAITING_SLEEP_MS: u64 = 800;
const DDC_CLI_TRANSITION_STEP_MS: u64 = 100;
const DDC_DIRECT_RETRIES: usize = 3;
const DDC_DIRECT_RETRY_SLEEP_MS: u64 = 40;
const DDC_CLI_RETRIES: usize = 3;
const DDC_CLI_RETRY_SLEEP_MS: u64 = 150;

enum Backend {
    Direct(Box<Mutex<Display>>),
    Cli { bus: u32 },
}

pub struct DdcUtil {
    identifier: String,
    backend: Backend,
    min_brightness: u64,
    max_brightness: u64,
}

impl DdcUtil {
    pub fn new(identifier: &str, min_brightness: u64) -> Result<Self> {
        let direct_error = match find_display_by_identifier(identifier, true)
            .or_else(|| find_display_by_identifier(identifier, false))
        {
            Some(mut display) => match get_max_brightness_with_retry(&mut display) {
                Ok(max_brightness) => {
                    return Ok(Self {
                        identifier: identifier.to_string(),
                        backend: Backend::Direct(Box::new(Mutex::new(display))),
                        min_brightness,
                        max_brightness,
                    })
                }
                Err(err) => {
                    log::warn!(
                        "Unable to initialize '{}' with direct DDC access: {:?}. Trying ddcutil CLI fallback.",
                        identifier,
                        err
                    );
                    err.to_string()
                }
            },
            None => "Unable to find display".to_string(),
        };

        let mut cli_backend =
            Self::new_cli_backend(identifier, min_brightness).map_err(|cli_err| {
                anyhow!(
                    "Unable to initialize DDC display '{}' directly ({}) or via ddcutil CLI ({})",
                    identifier,
                    direct_error,
                    cli_err
                )
            })?;
        log::warn!(
            "Using ddcutil CLI fallback for '{}' because direct DDC access is unreliable on this monitor.",
            identifier
        );
        cli_backend.max_brightness = cli_backend.max_brightness.max(min_brightness);
        Ok(cli_backend)
    }

    pub async fn get(&mut self) -> Result<u64> {
        let _lock = DDC_MUTEX.lock().await;
        match self.get_locked() {
            Ok(value) => Ok(value),
            Err(err) if self.switch_to_cli_locked(&err)? => self.get_locked(),
            Err(err) => Err(err),
        }
    }

    pub async fn set(&mut self, value: u64) -> Result<u64> {
        let _lock = DDC_MUTEX.lock().await;
        let value = value.clamp(self.min_brightness, self.max_brightness);
        match self.set_locked(value) {
            Ok(set_value) => Ok(set_value),
            Err(err) if self.switch_to_cli_locked(&err)? => self.set_locked(value),
            Err(err) => Err(err),
        }
    }

    pub fn waiting_sleep_ms(&self) -> u64 {
        match &self.backend {
            Backend::Direct(_) => DDC_DIRECT_WAITING_SLEEP_MS,
            Backend::Cli { .. } => DDC_CLI_WAITING_SLEEP_MS,
        }
    }

    pub fn transition_step_ms(&self) -> u64 {
        match &self.backend {
            Backend::Direct(_) => DDC_DIRECT_TRANSITION_STEP_MS,
            Backend::Cli { .. } => DDC_CLI_TRANSITION_STEP_MS,
        }
    }

    fn new_cli_backend(identifier: &str, min_brightness: u64) -> Result<Self> {
        let bus = find_ddcutil_bus_by_identifier(identifier)?.ok_or_else(|| {
            anyhow!(
                "Unable to find a ddcutil display that matches '{}'",
                identifier
            )
        })?;
        let (_, max_brightness) = cli_get_brightness(bus)?;

        Ok(Self {
            identifier: identifier.to_string(),
            backend: Backend::Cli { bus },
            min_brightness,
            max_brightness,
        })
    }

    fn get_locked(&mut self) -> Result<u64> {
        match &mut self.backend {
            Backend::Direct(display) => get_brightness_with_retry(display.get_mut()),
            Backend::Cli { bus } => {
                let (current, max_brightness) = cli_get_brightness(*bus)?;
                self.max_brightness = max_brightness;
                Ok(current)
            }
        }
    }

    fn set_locked(&mut self, value: u64) -> Result<u64> {
        match &mut self.backend {
            Backend::Direct(display) => set_brightness_with_retry(display.get_mut(), value),
            Backend::Cli { bus } => {
                cli_set_brightness(*bus, value)?;
                Ok(value)
            }
        }
    }

    fn switch_to_cli_locked(&mut self, err: &anyhow::Error) -> Result<bool> {
        if matches!(self.backend, Backend::Cli { .. }) {
            return Ok(false);
        }

        let Some(bus) = find_ddcutil_bus_by_identifier(&self.identifier)? else {
            return Ok(false);
        };

        let (_, max_brightness) = cli_get_brightness(bus)?;
        log::warn!(
            "Direct DDC access failed for '{}': {:?}. Switching to ddcutil CLI on bus {}.",
            self.identifier,
            err,
            bus
        );
        self.backend = Backend::Cli { bus };
        self.max_brightness = max_brightness;
        Ok(true)
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
    for attempt in 1..=DDC_DIRECT_RETRIES {
        match action() {
            Ok(result) => return Ok(result),
            Err(err) if attempt < DDC_DIRECT_RETRIES => {
                log::debug!(
                    "Failed to {} over direct DDC (attempt {}/{}): {:?}",
                    operation,
                    attempt,
                    DDC_DIRECT_RETRIES,
                    err
                );
                thread::sleep(Duration::from_millis(DDC_DIRECT_RETRY_SLEEP_MS));
            }
            Err(err) => return Err(err),
        }
    }

    unreachable!("retry loop always returns before falling through")
}

fn cli_get_brightness(bus: u32) -> Result<(u64, u64)> {
    let bus = bus.to_string();
    let stdout = retry_cli("read brightness via ddcutil CLI", || {
        run_ddcutil(&["--bus", bus.as_str(), "getvcp", "10", "--terse"])
    })?;

    parse_ddcutil_getvcp_output(&stdout)
}

fn cli_set_brightness(bus: u32, value: u64) -> Result<()> {
    let bus_arg = bus.to_string();
    let value_arg = value.to_string();

    if let Err(err) = retry_cli("set brightness via ddcutil CLI", || {
        run_ddcutil(&[
            "--bus",
            bus_arg.as_str(),
            "setvcp",
            "10",
            value_arg.as_str(),
            "--noverify",
        ])
        .map(|_| ())
    }) {
        // Some monitors apply the value but still make ddcutil exit non-zero.
        if cli_get_brightness(bus)
            .map(|(current, _)| current == value)
            .unwrap_or(false)
        {
            log::debug!(
                "ddcutil setvcp reported an error on bus {}, but brightness is already {}.",
                bus,
                value
            );
            return Ok(());
        }

        return Err(err.context(format!(
            "ddcutil setvcp failed on bus {} while setting brightness to {}",
            bus, value
        )));
    }

    Ok(())
}

fn retry_cli<T, F>(operation: &str, mut action: F) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    for attempt in 1..=DDC_CLI_RETRIES {
        match action() {
            Ok(result) => return Ok(result),
            Err(err) if attempt < DDC_CLI_RETRIES => {
                log::debug!(
                    "Failed to {} (attempt {}/{}): {:?}",
                    operation,
                    attempt,
                    DDC_CLI_RETRIES,
                    err
                );
                thread::sleep(Duration::from_millis(DDC_CLI_RETRY_SLEEP_MS));
            }
            Err(err) => return Err(err),
        }
    }

    unreachable!("retry loop always returns before falling through")
}

fn run_ddcutil(args: &[&str]) -> Result<String> {
    let output = Command::new("ddcutil")
        .args(args)
        .output()
        .with_context(|| format!("Unable to execute ddcutil {}", args.iter().join(" ")))?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let details = match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!(" stdout: {} stderr: {}", stdout, stderr),
        (false, true) => format!(" stdout: {}", stdout),
        (true, false) => format!(" stderr: {}", stderr),
        (true, true) => " no output".to_string(),
    };
    let exit_status = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".to_string());

    Err(anyhow!(
        "ddcutil {} failed with status {}.{}",
        args.iter().join(" "),
        exit_status,
        details
    ))
}

fn find_ddcutil_bus_by_identifier(identifier: &str) -> Result<Option<u32>> {
    let output = Command::new("ddcutil")
        .args(["detect", "--terse"])
        .output()
        .context("Unable to execute ddcutil detect")?;

    if !output.status.success() {
        return Err(anyhow!(
            "ddcutil detect failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(
        parse_ddcutil_detect_output(&String::from_utf8_lossy(&output.stdout))?
            .into_iter()
            .find_map(|display| display.monitor.contains(identifier).then_some(display.bus)),
    )
}

#[derive(Debug, PartialEq, Eq)]
struct CliDisplay {
    bus: u32,
    monitor: String,
}

fn parse_ddcutil_detect_output(output: &str) -> Result<Vec<CliDisplay>> {
    let mut displays = Vec::new();
    let mut current_bus = None;
    let mut current_monitor = None;
    let mut current_valid = true;

    for line in output.lines() {
        let line = line.trim();

        if line.is_empty() || line.starts_with("Display ") {
            push_cli_display(
                &mut displays,
                &mut current_bus,
                &mut current_monitor,
                current_valid,
            );
            current_valid = true;
            continue;
        }

        if line == "Invalid display" {
            push_cli_display(
                &mut displays,
                &mut current_bus,
                &mut current_monitor,
                current_valid,
            );
            current_valid = false;
            continue;
        }

        if let Some(bus) = line.strip_prefix("I2C bus:") {
            current_bus = Some(parse_bus_number(bus.trim())?);
            continue;
        }

        if let Some(monitor) = line.strip_prefix("Monitor:") {
            current_monitor = Some(monitor.trim().to_string());
        }
    }

    push_cli_display(
        &mut displays,
        &mut current_bus,
        &mut current_monitor,
        current_valid,
    );
    Ok(displays)
}

fn push_cli_display(
    displays: &mut Vec<CliDisplay>,
    current_bus: &mut Option<u32>,
    current_monitor: &mut Option<String>,
    current_valid: bool,
) {
    let display = current_bus.take().zip(current_monitor.take());

    if let (true, Some((bus, monitor))) = (current_valid, display) {
        displays.push(CliDisplay { bus, monitor });
    }
}

fn parse_bus_number(bus: &str) -> Result<u32> {
    bus.rsplit('-')
        .next()
        .ok_or_else(|| anyhow!("Unable to parse ddcutil bus '{}': missing bus suffix", bus))?
        .parse()
        .context("Unable to parse ddcutil bus number")
}

fn parse_ddcutil_getvcp_output(output: &str) -> Result<(u64, u64)> {
    let line = output
        .lines()
        .find(|line| line.trim_start().starts_with("VCP "))
        .ok_or_else(|| {
            anyhow!(
                "ddcutil getvcp returned unexpected output: {}",
                output.trim()
            )
        })?;
    let parts = line.split_whitespace().collect_vec();

    if parts.len() < 5 {
        return Err(anyhow!(
            "ddcutil getvcp returned unexpected terse output: {}",
            line
        ));
    }

    let current = parts[3]
        .parse::<u64>()
        .context("Unable to parse current brightness from ddcutil getvcp output")?;
    let maximum = parts[4]
        .parse::<u64>()
        .context("Unable to parse max brightness from ddcutil getvcp output")?;

    Ok((current, maximum))
}

fn find_display_by_identifier(identifier: &str, check_caps: bool) -> Option<Display> {
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
            .contains(identifier)
            .then(|| {
                log::debug!(
                    "Using display '{}' for config '{}' (check_caps={})",
                    merged,
                    identifier,
                    check_caps
                );
            })
            .map(|_| display)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ddcutil_detect_output() -> Result<()> {
        let output = r#"
Invalid display
   I2C bus:          /dev/i2c-4
   DRM connector:    card1-eDP-1
   Monitor:          AUO::

Display 1
   I2C bus:          /dev/i2c-12
   DRM connector:    card0-HDMI-A-3
   Monitor:          GSM:LG ULTRAWIDE:504AZER5F964
"#;

        assert_eq!(
            parse_ddcutil_detect_output(output)?,
            vec![CliDisplay {
                bus: 12,
                monitor: "GSM:LG ULTRAWIDE:504AZER5F964".to_string(),
            }]
        );

        Ok(())
    }

    #[test]
    fn test_parse_ddcutil_getvcp_output() -> Result<()> {
        assert_eq!(parse_ddcutil_getvcp_output("VCP 10 C 27 100")?, (27, 100));
        Ok(())
    }
}
