use super::app;
use ddc_hi::Display;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

struct Connector {
    name: String,
    path: PathBuf,
    edid: Vec<u8>,
}

struct Backlight {
    path: PathBuf,
    connector: Option<String>,
    edid: Vec<u8>,
    rank: u8,
}

pub fn outputs() -> Vec<app::Output> {
    let connectors = connectors();
    let mut backlights = backlights(&connectors);
    let mut used_backlights = HashSet::new();
    let mut displays = Display::enumerate();
    let display_identifier_counts = displays.iter().filter_map(display_identifier).fold(
        HashMap::new(),
        |mut counts, identifier| {
            *counts.entry(identifier).or_insert(0) += 1;
            counts
        },
    );
    let mut outputs = Vec::new();

    for connector in connectors {
        let backlight = backlights
            .iter_mut()
            .enumerate()
            .filter(|(index, backlight)| {
                !used_backlights.contains(index)
                    && (backlight.connector.as_deref() == Some(&connector.name)
                        || same_edid(&backlight.edid, &connector.edid))
            })
            .max_by_key(|(_, backlight)| backlight.rank);

        if let Some((index, backlight)) = backlight {
            used_backlights.insert(index);
            log::debug!(
                "Discovered output '{}' using backlight {}",
                connector.name,
                backlight.path.display()
            );
            outputs.push(app::Output::Backlight(app::BacklightOutput {
                name: connector.name,
                path: backlight.path.to_string_lossy().into_owned(),
                capturer: app::Capturer::Auto,
                min_brightness: 1,
                predictor: app::Predictor::Adaptive,
            }));
            continue;
        }

        let display = displays.iter().position(|display| {
            display
                .info
                .edid_data
                .as_deref()
                .is_some_and(|edid| same_edid(edid, &connector.edid))
        });
        if let Some(index) = display {
            let display = displays.swap_remove(index);
            if let Some(identifier) = display_identifier(&display) {
                if display_identifier_counts.get(&identifier) == Some(&1) {
                    log::debug!(
                        "Discovered output '{}' using DDC identifier '{}'",
                        connector.name,
                        identifier
                    );
                    outputs.push(app::Output::DdcUtil(app::DdcUtilOutput {
                        name: connector.name,
                        identifier,
                        identifier_overridden: false,
                        capturer: app::Capturer::Auto,
                        min_brightness: 1,
                        predictor: app::Predictor::Adaptive,
                    }));
                    continue;
                }
                log::warn!(
                    "Cannot uniquely identify DDC output '{}' because identifier '{}' is shared by multiple displays",
                    connector.name,
                    identifier
                );
            }
        }

        log::warn!(
            "Skipping connected output '{}' because no brightness control was detected",
            connector.name
        );
    }

    outputs.extend(keyboards());
    outputs
}

fn keyboards() -> Vec<app::Output> {
    read_dir("/sys/class/leds")
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_string();
            if !name.to_ascii_lowercase().contains("kbd_backlight")
                || !path.join("brightness").exists()
                || !path.join("max_brightness").exists()
            {
                return None;
            }
            log::debug!(
                "Discovered keyboard '{}' using backlight {}",
                name,
                path.display()
            );
            Some(app::Output::Backlight(app::BacklightOutput {
                name,
                path: path.to_string_lossy().into_owned(),
                capturer: app::Capturer::None,
                min_brightness: 0,
                predictor: app::Predictor::Adaptive,
            }))
        })
        .collect()
}

fn connectors() -> Vec<Connector> {
    read_dir("/sys/class/drm")
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("card") && name.contains('-'))
                && fs::read_to_string(path.join("status"))
                    .is_ok_and(|status| status.trim() == "connected")
        })
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.split_once('-')?.1.to_string();
            let edid = fs::read(path.join("edid")).unwrap_or_default();
            Some(Connector { name, path, edid })
        })
        .collect()
}

fn backlights(connectors: &[Connector]) -> Vec<Backlight> {
    read_dir("/sys/class/backlight")
        .into_iter()
        .map(|path| {
            let canonical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let connector = connectors.iter().find_map(|connector| {
                let connector_path =
                    fs::canonicalize(&connector.path).unwrap_or_else(|_| connector.path.clone());
                canonical
                    .starts_with(&connector_path)
                    .then(|| connector.name.clone())
            });
            let edid = fs::read(path.join("device/edid")).unwrap_or_default();
            let rank = fs::read_to_string(path.join("type"))
                .map(|kind| match kind.trim() {
                    "raw" => 3,
                    "platform" => 2,
                    "firmware" => 1,
                    _ => 0,
                })
                .unwrap_or(0);
            Backlight {
                path,
                connector,
                edid,
                rank,
            }
        })
        .collect()
}

fn same_edid(left: &[u8], right: &[u8]) -> bool {
    left.len() >= 128 && right.len() >= 128 && left[..128] == right[..128]
}

fn display_identifier(display: &Display) -> Option<String> {
    display
        .info
        .serial_number
        .clone()
        .filter(|value| !value.is_empty())
        .or_else(|| display.info.serial.map(|value| format!("{:#010x}", value)))
        .or_else(|| {
            display
                .info
                .model_name
                .clone()
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            display
                .info
                .manufacturer_id
                .clone()
                .filter(|value| !value.is_empty())
        })
}

fn read_dir(path: impl AsRef<Path>) -> Vec<PathBuf> {
    fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn merge(configured: Vec<app::Output>, mut detected: Vec<app::Output>) -> Vec<app::Output> {
    let mut outputs = Vec::new();

    for configured_output in configured {
        let output_name = name(&configured_output).to_string();
        let detected_position = detected.iter().position(|detected_output| {
            name(detected_output) == output_name
                || same_backlight_path(detected_output, &configured_output)
        });
        if let Some(position) = detected_position {
            let mut detected_output = detected.remove(position);
            apply_overrides(&mut detected_output, configured_output);
            outputs.push(detected_output);
        } else if has_brightness_path(&configured_output) {
            outputs.push(configured_output);
        } else {
            log::debug!(
                "Skipping configured output '{}' because it is disconnected or could not be detected",
                output_name
            );
        }
    }

    outputs.extend(detected);
    outputs.sort_by(|left, right| name(left).cmp(name(right)));
    outputs
}

fn same_backlight_path(left: &app::Output, right: &app::Output) -> bool {
    match (left, right) {
        (app::Output::Backlight(left), app::Output::Backlight(right))
            if !left.path.is_empty() && !right.path.is_empty() =>
        {
            fs::canonicalize(&left.path).unwrap_or_else(|_| PathBuf::from(&left.path))
                == fs::canonicalize(&right.path).unwrap_or_else(|_| PathBuf::from(&right.path))
        }
        _ => false,
    }
}

fn apply_overrides(detected: &mut app::Output, configured: app::Output) {
    match (detected, configured) {
        (app::Output::Backlight(detected), app::Output::Backlight(configured)) => {
            detected.name = configured.name;
            if !configured.path.is_empty() {
                detected.path = configured.path;
            }
            detected.capturer = configured.capturer;
            detected.predictor = configured.predictor;
        }
        (app::Output::DdcUtil(detected), app::Output::DdcUtil(configured)) => {
            detected.name = configured.name;
            if configured.identifier_overridden {
                detected.identifier = configured.identifier;
                detected.identifier_overridden = true;
            }
            detected.capturer = configured.capturer;
            detected.predictor = configured.predictor;
        }
        (app::Output::Backlight(detected), app::Output::DdcUtil(configured)) => {
            detected.capturer = configured.capturer;
            detected.predictor = configured.predictor;
        }
        (app::Output::DdcUtil(detected), app::Output::Backlight(configured)) => {
            detected.capturer = configured.capturer;
            detected.predictor = configured.predictor;
        }
    }
}

fn has_brightness_path(output: &app::Output) -> bool {
    match output {
        app::Output::Backlight(output) => !output.path.is_empty(),
        app::Output::DdcUtil(_) => true,
    }
}

fn name(output: &app::Output) -> &str {
    match output {
        app::Output::Backlight(output) => &output.name,
        app::Output::DdcUtil(output) => &output.name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backlight(name: &str, path: &str, capturer: app::Capturer) -> app::Output {
        app::Output::Backlight(app::BacklightOutput {
            name: name.to_string(),
            path: path.to_string(),
            capturer,
            min_brightness: 1,
            predictor: app::Predictor::Adaptive,
        })
    }

    #[test]
    fn adds_detected_outputs_and_applies_overrides() {
        let configured = vec![backlight("eDP-1", "", app::Capturer::None)];
        let detected = vec![
            backlight("eDP-1", "/sys/backlight/panel", app::Capturer::Auto),
            backlight("DP-1", "/sys/backlight/external", app::Capturer::Auto),
        ];
        let merged = merge(configured, detected);

        assert_eq!(merged.len(), 2);
        match &merged[1] {
            app::Output::Backlight(output) => {
                assert_eq!(output.name, "eDP-1");
                assert_eq!(output.path, "/sys/backlight/panel");
                assert!(matches!(output.capturer, app::Capturer::None));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn configured_keyboard_replaces_detected_keyboard_by_path() {
        let configured = backlight(
            "keyboard-dell",
            "/sys/class/leds/dell::kbd_backlight",
            app::Capturer::None,
        );
        let detected = backlight(
            "dell::kbd_backlight",
            "/sys/class/leds/dell::kbd_backlight",
            app::Capturer::None,
        );
        let merged = merge(vec![configured], vec![detected]);

        assert_eq!(merged.len(), 1);
        match &merged[0] {
            app::Output::Backlight(output) => assert_eq!(output.name, "keyboard-dell"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn preserves_detected_ddc_identifier_when_it_is_not_overridden() {
        let configured = app::Output::DdcUtil(app::DdcUtilOutput {
            name: "HDMI-A-1".to_string(),
            identifier: "HDMI-A-1".to_string(),
            identifier_overridden: false,
            capturer: app::Capturer::None,
            min_brightness: 1,
            predictor: app::Predictor::Adaptive,
        });
        let detected = app::Output::DdcUtil(app::DdcUtilOutput {
            name: "HDMI-A-1".to_string(),
            identifier: "serial-123".to_string(),
            identifier_overridden: false,
            capturer: app::Capturer::Auto,
            min_brightness: 1,
            predictor: app::Predictor::Adaptive,
        });

        match &merge(vec![configured], vec![detected])[0] {
            app::Output::DdcUtil(output) => {
                assert_eq!(output.identifier, "serial-123");
                assert!(matches!(output.capturer, app::Capturer::None));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn skips_unresolved_pathless_overrides() {
        assert!(merge(
            vec![backlight("disconnected", "", app::Capturer::Auto)],
            vec![]
        )
        .is_empty());
    }
}
