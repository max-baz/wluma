use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
mod app;
mod discovery;
mod file;
use anyhow::{anyhow, Result};
pub use app::*;

pub fn load() -> Result<app::Config> {
    validate(parse()?)
}

pub fn detected_outputs(configured: Vec<app::Output>) -> Vec<app::Output> {
    discovery::merge(configured, discovery::outputs())
}

pub fn topology() -> Vec<String> {
    discovery::topology()
}

fn external_socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join("wluma/als.sock"))
}

fn default_external_path() -> Result<String> {
    external_socket_path()
        .map(|path| path.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("External ALS requires 'path' when XDG_RUNTIME_DIR is not set"))
}

fn match_predictor(predictor: file::Predictor) -> Result<app::Predictor> {
    match predictor {
        file::Predictor::Adaptive => Ok(app::Predictor::Adaptive),
        file::Predictor::Manual { points, thresholds } => {
            if thresholds.is_some() {
                return Err(anyhow!(
                    "Manual predictor 'thresholds' are no longer supported; configure 'points' with als, luma and reduction values"
                ));
            }
            if points.is_empty() {
                return Err(anyhow!("Manual predictor requires at least one point"));
            }
            if points
                .iter()
                .any(|point| point.luma > 100 || point.reduction > 100)
            {
                return Err(anyhow!(
                    "Manual predictor luma and reduction values must be between 0 and 100"
                ));
            }
            Ok(app::Predictor::Manual {
                points: points
                    .into_iter()
                    .map(|point| app::ManualPoint {
                        als: point.als,
                        luma: point.luma,
                        reduction: point.reduction,
                    })
                    .collect(),
            })
        }
    }
}

fn validate_manual_points(als: &app::Als, output: &[app::Output]) -> Result<()> {
    let limit = match als {
        app::Als::External {
            scale: crate::als::Scale::Linear,
            ..
        }
        | app::Als::Webcam { .. }
        | app::Als::Time { .. } => Some(100),
        app::Als::None => Some(0),
        app::Als::Auto { .. }
        | app::Als::External {
            scale: crate::als::Scale::Lux,
            ..
        }
        | app::Als::Iio { .. } => None,
    };
    let Some(limit) = limit else {
        return Ok(());
    };
    let invalid = output.iter().any(|output| {
        let predictor = match output {
            app::Output::Backlight(output) => &output.predictor,
            app::Output::DdcUtil(output) => &output.predictor,
        };
        matches!(predictor, app::Predictor::Manual { points } if points.iter().any(|point| point.als > limit))
    });
    if invalid {
        return Err(anyhow!(
            "Manual predictor ALS values must be between 0 and {limit} for the configured ALS source"
        ));
    }
    Ok(())
}

fn match_capturer(capturer: file::Capturer) -> app::Capturer {
    match capturer {
        file::Capturer::Auto => app::Capturer::Auto,
        file::Capturer::None => app::Capturer::None,
        file::Capturer::Wlroots => {
            log::warn!(
                "Config value capturer=\"wlroots\" is deprecated, use capturer=\"wayland\" instead"
            );
            app::Capturer::Wayland(app::WaylandProtocol::Any)
        }
        file::Capturer::Wayland => app::Capturer::Wayland(app::WaylandProtocol::Any),
        file::Capturer::Pipewire => app::Capturer::Pipewire(app::PipewireProtocol::Any),
        file::Capturer::XdgDesktopPortalScreencast => {
            app::Capturer::Pipewire(app::PipewireProtocol::Portal)
        }
        file::Capturer::ZkdeScreencastUnstableV1 => {
            app::Capturer::Pipewire(app::PipewireProtocol::Kwin)
        }
        file::Capturer::GnomeMutterScreencast => {
            app::Capturer::Pipewire(app::PipewireProtocol::Mutter)
        }
        file::Capturer::ExtImageCopyCaptureV1 => {
            app::Capturer::Wayland(app::WaylandProtocol::ExtImageCopyCaptureV1)
        }
        file::Capturer::WlrScreencopyUnstableV1 => {
            app::Capturer::Wayland(app::WaylandProtocol::WlrScreencopyUnstableV1)
        }
        file::Capturer::WlrExportDmabufUnstableV1 => {
            app::Capturer::Wayland(app::WaylandProtocol::WlrExportDmabufUnstableV1)
        }
    }
}

fn default_iio_thresholds() -> HashMap<u64, String> {
    [
        (0, "night"),
        (20, "dark"),
        (80, "dim"),
        (250, "normal"),
        (500, "bright"),
        (800, "outdoors"),
    ]
    .into_iter()
    .map(|(value, name)| (value, name.to_string()))
    .collect()
}

fn time_level_at(levels: &HashMap<u64, u64>, hour: u64) -> u64 {
    let mut points = levels
        .iter()
        .map(|(hour, level)| (*hour, *level))
        .collect::<Vec<_>>();
    points.sort_unstable_by_key(|(hour, _)| *hour);
    if points.len() == 1 {
        return points[0].1;
    }
    let next_index = points
        .iter()
        .position(|(candidate, _)| *candidate > hour)
        .unwrap_or(0);
    let previous_index = if next_index == 0 {
        points.len() - 1
    } else {
        next_index - 1
    };
    let (previous_hour, previous_level) = points[previous_index];
    let (mut next_hour, next_level) = points[next_index];
    let mut hour = hour;
    if next_index == 0 {
        next_hour += 24;
        if hour < previous_hour {
            hour += 24;
        }
    }
    let progress = (hour - previous_hour) as f64 / (next_hour - previous_hour) as f64;
    (previous_level as f64 + (next_level as f64 - previous_level as f64) * progress).round() as u64
}

fn parse() -> Result<app::Config> {
    let config_path = xdg::BaseDirectories::with_prefix("wluma").find_config_file("config.toml");
    let file_config = config_path
        .map(fs::read_to_string)
        .transpose()?
        .map(|config| toml::from_str(&config))
        .transpose()?
        .unwrap_or_default();

    parse_file_config(file_config)
}

#[cfg(test)]
fn parse_config_str(file_config: &str) -> Result<app::Config> {
    parse_file_config(toml::from_str(file_config)?)
}

fn parse_file_config(file_config: file::Config) -> Result<app::Config> {
    let parse_map = |values: HashMap<String, String>| -> Result<HashMap<u64, String>> {
        values
            .into_iter()
            .map(|(key, value)| Ok((key.parse::<u64>()?, value)))
            .collect()
    };
    let parse_levels = |values: HashMap<String, u64>| -> Result<HashMap<u64, u64>> {
        values
            .into_iter()
            .map(|(key, value)| Ok((key.parse::<u64>()?, value)))
            .collect()
    };

    let mut output = file_config
        .output
        .backlight
        .into_iter()
        .map(|o| {
            Ok(app::Output::Backlight(app::BacklightOutput {
                name: o.name,
                path: o.path.unwrap_or_default(),
                min_brightness: 1,
                kind: app::BacklightKind::Display,
                capturer: match_capturer(o.capturer.unwrap_or_default()),
                vulkan_device: o.vulkan_device.into(),
                predictor: match_predictor(o.predictor.unwrap_or_default())?,
                als_direction: crate::predictor::AlsDirection::Increasing,
                gamma: o.gamma.unwrap_or(true),
                enabled: o.enabled.unwrap_or(true),
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    output.extend(
        file_config
            .output
            .ddcutil
            .into_iter()
            .map(|o| {
                let identifier_overridden = o.identifier.is_some();
                let identifier = o.identifier.unwrap_or_else(|| o.name.clone());
                Ok(app::Output::DdcUtil(app::DdcUtilOutput {
                    name: o.name,
                    identifier,
                    identifier_overridden,
                    min_brightness: 1,
                    capturer: match_capturer(o.capturer.unwrap_or_default()),
                    vulkan_device: o.vulkan_device.into(),
                    predictor: match_predictor(o.predictor.unwrap_or_default())?,
                    gamma: o.gamma.unwrap_or(true),
                    enabled: o.enabled.unwrap_or(true),
                }))
            })
            .collect::<Result<Vec<_>>>()?,
    );
    output.extend(file_config.keyboard.into_iter().map(|k| {
        app::Output::Backlight(app::BacklightOutput {
            name: k.name,
            path: k.path,
            min_brightness: 0,
            kind: app::BacklightKind::Keyboard,
            capturer: Capturer::None,
            vulkan_device: app::VulkanDevice::Auto,
            predictor: app::Predictor::Adaptive,
            als_direction: crate::predictor::AlsDirection::Decreasing,
            gamma: false,
            enabled: true,
        })
    }));

    let als = match file_config.als {
        None => app::Als::Auto {
            thresholds: default_iio_thresholds(),
        },
        Some(file::Als::External { path, scale }) => {
            let scale = match scale {
                file::AlsScale::Lux => crate::als::Scale::Lux,
                file::AlsScale::Linear => crate::als::Scale::Linear,
            };
            app::Als::External {
                path: path.map_or_else(default_external_path, Ok)?,
                scale,
                thresholds: if scale == crate::als::Scale::Lux {
                    default_iio_thresholds()
                } else {
                    HashMap::new()
                },
            }
        }
        Some(file::Als::Iio { path, thresholds }) => {
            if thresholds.is_some() {
                log::warn!("ALS thresholds are obsolete and are only used to migrate learned data");
            }
            app::Als::Iio {
                path,
                thresholds: thresholds
                    .map(parse_map)
                    .transpose()?
                    .unwrap_or_else(default_iio_thresholds),
            }
        }
        Some(file::Als::Webcam { video, thresholds }) => {
            if thresholds.is_some() {
                log::warn!("ALS thresholds are obsolete and are only used to migrate learned data");
            }
            app::Als::Webcam {
                video,
                thresholds: thresholds.map(parse_map).transpose()?.unwrap_or_default(),
            }
        }
        Some(file::Als::Time { levels, thresholds }) => {
            let levels = levels.ok_or_else(|| {
                anyhow!("Time ALS 'thresholds' are no longer supported; configure numeric 'levels' by hour")
            })?;
            let levels = parse_levels(levels)?;
            if levels.is_empty()
                || levels.keys().any(|hour| *hour >= 24)
                || levels.values().any(|level| *level > 100)
            {
                return Err(anyhow!(
                    "Time ALS requires hours from 0 to 23 and levels from 0 to 100"
                ));
            }
            let thresholds = thresholds.map(parse_map).transpose()?.unwrap_or_default();
            if thresholds.keys().any(|hour| *hour >= 24) {
                return Err(anyhow!(
                    "Legacy time ALS threshold hours must be between 0 and 23"
                ));
            }
            if !thresholds.is_empty() {
                log::warn!(
                    "Time ALS thresholds are obsolete and are only used to migrate learned data"
                );
            }
            let mut thresholds = thresholds.into_iter().collect::<Vec<_>>();
            thresholds.sort_unstable_by_key(|(hour, _)| *hour);
            let mut migration_thresholds = HashMap::new();
            for (hour, profile) in thresholds {
                let level = time_level_at(&levels, hour);
                if migration_thresholds
                    .insert(level, profile.clone())
                    .is_some_and(|existing| existing != profile)
                {
                    return Err(anyhow!(
                        "Legacy time ALS profiles map to the same numeric level {level}"
                    ));
                }
            }
            app::Als::Time {
                levels,
                thresholds: migration_thresholds,
            }
        }
        Some(file::Als::None) => app::Als::None,
    };

    let idle = file_config.idle.unwrap_or_default();
    let defaults = (idle.enabled, idle.timeout, idle.brightness);
    let profile = |override_: file::IdleProfile| app::IdleProfile {
        enabled: override_.enabled.unwrap_or(defaults.0),
        timeout: override_.timeout.unwrap_or(defaults.1),
        brightness: override_.brightness.unwrap_or(defaults.2),
    };
    let ac = profile(idle.ac);
    let battery = profile(idle.battery);
    for profile in [ac, battery].into_iter().filter(|profile| profile.enabled) {
        if profile.timeout == 0 {
            return Err(anyhow!("Idle timeouts must be greater than zero"));
        }
        if profile.brightness > 100 {
            return Err(anyhow!("Idle brightness must be between 0 and 100"));
        }
        profile
            .timeout
            .checked_mul(1000)
            .filter(|timeout| *timeout <= u32::MAX as u64)
            .ok_or_else(|| anyhow!("Idle timeout is too large"))?;
    }
    let idle = (ac.enabled || battery.enabled).then_some(app::Idle { ac, battery });

    validate_manual_points(&als, &output)?;
    Ok(app::Config { als, idle, output })
}

fn validate(config: app::Config) -> Result<app::Config> {
    let names = config
        .output
        .iter()
        .map(|output| match output {
            app::Output::Backlight(app::BacklightOutput { name, .. }) => name,
            app::Output::DdcUtil(DdcUtilOutput { name, .. }) => name,
        })
        .collect::<HashSet<_>>();

    if names.len() != config.output.len() {
        Err(anyhow!("Names of all outputs and keyboards are not unique"))
    } else {
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_config_uses_auto_als() {
        let config = validate(parse_config_str("").unwrap()).unwrap();
        let debug = format!("{config:#?}");
        assert!(!debug.contains("thresholds"));
        assert!(!debug.contains("\"night\""));
        assert!(matches!(config.als, app::Als::Auto { .. }));
        assert_eq!(
            config.idle,
            Some(app::Idle {
                ac: app::IdleProfile {
                    enabled: true,
                    timeout: 120,
                    brightness: 30
                },
                battery: app::IdleProfile {
                    enabled: true,
                    timeout: 120,
                    brightness: 30
                }
            })
        );
    }

    #[test]
    fn test_keyboard_uses_decreasing_als_direction() {
        let config = parse_config_str(
            r#"
[[keyboard]]
name = "keyboard"
path = "/sys/class/leds/kbd_backlight"
"#,
        )
        .unwrap();

        match &config.output[0] {
            app::Output::Backlight(output) => {
                assert_eq!(output.kind, app::BacklightKind::Keyboard);
                assert_eq!(
                    output.als_direction,
                    crate::predictor::AlsDirection::Decreasing
                );
            }
            _ => unreachable!(),
        }
        assert!(!format!("{config:#?}").contains("als_direction"));
    }

    #[test]
    fn test_external_defaults_to_lux() {
        let config = parse_config_str(
            r#"
[als.external]
path = "/tmp/als.sock"
"#,
        )
        .unwrap();

        match config.als {
            app::Als::External { path, scale, .. } => {
                assert_eq!(path, "/tmp/als.sock");
                assert_eq!(scale, crate::als::Scale::Lux);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_external_scale_can_be_configured() {
        let config = parse_config_str(
            r#"
[als.external]
path = "/tmp/als.sock"
scale = "linear"
"#,
        )
        .unwrap();

        match config.als {
            app::Als::External { path, scale, .. } => {
                assert_eq!(path, "/tmp/als.sock");
                assert_eq!(scale, crate::als::Scale::Linear);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_iio_path_is_optional() {
        let config = parse_config_str(
            r#"
[als.iio]
thresholds = { 0 = "night" }
"#,
        )
        .unwrap();

        match config.als {
            app::Als::Iio { path, .. } => assert_eq!(path, None),
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_iio_path_can_configure_fallback() {
        let config = parse_config_str(
            r#"
[als.iio]
path = "/sys/bus/iio/devices"
thresholds = { 0 = "night" }
"#,
        )
        .unwrap();

        match config.als {
            app::Als::Iio { path, .. } => {
                assert_eq!(path.as_deref(), Some("/sys/bus/iio/devices"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_time_requires_new_levels() {
        let error = parse_config_str(
            r#"
[als.time]
thresholds = { 0 = "night", 8 = "day" }
"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("levels"));
    }

    #[test]
    fn test_time_keeps_thresholds_for_state_migration() {
        let config = parse_config_str(
            r#"
[als.time]
levels = { 0 = 0, 8 = 100, 20 = 0 }
thresholds = { 0 = "night", 8 = "day", 20 = "night" }
"#,
        )
        .unwrap();
        match config.als {
            app::Als::Time { thresholds, .. } => {
                assert_eq!(thresholds.get(&0).map(String::as_str), Some("night"));
                assert_eq!(thresholds.get(&100).map(String::as_str), Some("day"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_time_rejects_ambiguous_migration_thresholds() {
        let error = parse_config_str(
            r#"
[als.time]
levels = { 0 = 0, 12 = 100 }
thresholds = { 0 = "night", 24 = "day" }
"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("between 0 and 23"));

        let error = parse_config_str(
            r#"
[als.time]
levels = { 0 = 0, 12 = 100 }
thresholds = { 0 = "night", 6 = "day", 18 = "evening" }
"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("same numeric level"));
    }

    #[test]
    fn test_manual_predictor_uses_points() {
        let config = parse_config_str(
            r#"
[als.none]

[[output.backlight]]
name = "panel"
[output.backlight.predictor.manual]
[[output.backlight.predictor.manual.points]]
als = 0
luma = 100
reduction = 60
"#,
        )
        .unwrap();
        match &config.output[0] {
            app::Output::Backlight(app::BacklightOutput {
                predictor: app::Predictor::Manual { points },
                ..
            }) => assert_eq!(
                points,
                &vec![app::ManualPoint {
                    als: 0,
                    luma: 100,
                    reduction: 60
                }]
            ),
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_legacy_manual_predictor_is_rejected() {
        let error = parse_config_str(
            r#"
[als.none]

[[output.backlight]]
name = "panel"
[output.backlight.predictor.manual]
thresholds.none = { 0 = 0, 100 = 60 }
"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("points"));
    }

    #[test]
    fn test_manual_predictor_validates_als_domain() {
        let error = parse_config_str(
            r#"
[als.webcam]
video = 0

[[output.backlight]]
name = "panel"
[output.backlight.predictor.manual]
[[output.backlight.predictor.manual.points]]
als = 101
luma = 50
reduction = 20
"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("between 0 and 100"));
    }

    #[test]
    fn test_idle_configuration() {
        let config = parse_config_str(
            r#"
[idle.ac]
timeout = 600
brightness = 40

[idle.battery]
timeout = 180
brightness = 20
"#,
        )
        .unwrap();
        assert_eq!(
            config.idle,
            Some(app::Idle {
                ac: app::IdleProfile {
                    enabled: true,
                    timeout: 600,
                    brightness: 40
                },
                battery: app::IdleProfile {
                    enabled: true,
                    timeout: 180,
                    brightness: 20
                }
            })
        );
    }

    #[test]
    fn test_idle_profiles_inherit_global_configuration() {
        let config = parse_config_str(
            r#"
[idle]
timeout = 180
brightness = 40
"#,
        )
        .unwrap();
        let idle = config.idle.unwrap();
        for profile in [idle.ac, idle.battery] {
            assert!(profile.enabled);
            assert_eq!(profile.timeout, 180);
            assert_eq!(profile.brightness, 40);
        }
    }

    #[test]
    fn test_idle_profile_uses_source_defaults() {
        let config = parse_config_str(
            r#"
[idle.battery]
brightness = 20
"#,
        )
        .unwrap();
        assert_eq!(
            config.idle,
            Some(app::Idle {
                ac: app::IdleProfile {
                    enabled: true,
                    timeout: 120,
                    brightness: 30
                },
                battery: app::IdleProfile {
                    enabled: true,
                    timeout: 120,
                    brightness: 20
                }
            })
        );
    }

    #[test]
    fn test_idle_source_can_be_disabled() {
        let config = parse_config_str(
            r#"
[idle.ac]
enabled = false
"#,
        )
        .unwrap();
        assert_eq!(
            config.idle,
            Some(app::Idle {
                ac: app::IdleProfile {
                    enabled: false,
                    timeout: 120,
                    brightness: 30
                },
                battery: app::IdleProfile {
                    enabled: true,
                    timeout: 120,
                    brightness: 30
                }
            })
        );
    }

    #[test]
    fn test_idle_source_can_override_global_disable() {
        let config = parse_config_str(
            r#"
[idle]
enabled = false

[idle.battery]
enabled = true
"#,
        )
        .unwrap();
        let idle = config.idle.unwrap();
        assert!(!idle.ac.enabled);
        assert!(idle.battery.enabled);
    }

    #[test]
    fn test_idle_can_be_disabled() {
        let config = parse_config_str(
            r#"
[idle]
enabled = false
"#,
        )
        .unwrap();
        assert_eq!(config.idle, None);
    }

    #[test]
    fn test_idle_configuration_is_validated() {
        for value in [
            "[idle.ac]\ntimeout = 0",
            "[idle.battery]\ntimeout = 0",
            "[idle.ac]\nbrightness = 101",
            "[idle.battery]\nbrightness = 101",
            "[idle.ac]\ntimeout = 4294968",
        ] {
            assert!(parse_config_str(value).is_err());
        }
    }

    #[test]
    fn test_backlight_override_needs_only_a_name() {
        let config = parse_config_str(
            r#"
[als.none]

[[output.backlight]]
name = "eDP-1"
capturer = "none"
vulkan_device = "/dev/dri/renderD128"
"#,
        )
        .unwrap();

        match &config.output[0] {
            app::Output::Backlight(output) => {
                assert_eq!(output.name, "eDP-1");
                assert!(output.path.is_empty());
                assert!(matches!(output.capturer, app::Capturer::None));
                assert_eq!(output.vulkan_device.as_deref(), Some("/dev/dri/renderD128"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_output_can_be_disabled() {
        let config = parse_config_str(
            r#"
[[output.ddcutil]]
name = "DP-1"
enabled = false
"#,
        )
        .unwrap();

        match &config.output[0] {
            app::Output::DdcUtil(output) => assert!(!output.enabled),
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_gamma_can_be_disabled_per_output() {
        let config = parse_config_str(
            r#"
[als.none]

[[output.backlight]]
name = "panel"
gamma = false

[[output.ddcutil]]
name = "DP-1"
"#,
        )
        .unwrap();

        match &config.output[0] {
            app::Output::Backlight(output) => assert!(!output.gamma),
            _ => unreachable!(),
        }
        match &config.output[1] {
            app::Output::DdcUtil(output) => assert!(output.gamma),
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_pipewire_capturers() {
        for (value, expected) in [
            ("pipewire", app::PipewireProtocol::Any),
            (
                "xdg-desktop-portal-screencast",
                app::PipewireProtocol::Portal,
            ),
            ("zkde-screencast-unstable-v1", app::PipewireProtocol::Kwin),
            ("gnome-mutter-screencast", app::PipewireProtocol::Mutter),
        ] {
            let config = parse_config_str(&format!(
                r#"
[als.none]

[[output.backlight]]
name = "panel"
path = "/sys/class/backlight/panel"
capturer = "{value}"
"#,
            ))
            .unwrap();

            match &config.output[0] {
                app::Output::Backlight(output) => match &output.capturer {
                    app::Capturer::Pipewire(protocol) => assert_eq!(*protocol, expected),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn test_ddc_identifier_defaults_to_output_name() {
        let config = parse_config_str(
            r#"
[als.none]

[[output.backlight]]
name = "panel"
path = "/sys/class/backlight/panel"
capturer = "none"

[[output.ddcutil]]
name = "HDMI-A-3"
capturer = "none"
"#,
        )
        .unwrap();

        match &config.output[0] {
            app::Output::Backlight(output) => {
                assert_eq!("panel", output.name);
            }
            _ => unreachable!(),
        }

        match &config.output[1] {
            app::Output::DdcUtil(output) => {
                assert_eq!("HDMI-A-3", output.name);
                assert_eq!("HDMI-A-3", output.identifier);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_ddc_identifier_can_be_configured_separately() {
        let config = parse_config_str(
            r#"
[als.none]

[[output.backlight]]
name = "panel"
path = "/sys/class/backlight/panel"
capturer = "none"

[[output.ddcutil]]
name = "HDMI-A-3"
identifier = "serial-123"
capturer = "none"
"#,
        )
        .unwrap();

        match &config.output[0] {
            app::Output::Backlight(output) => {
                assert_eq!("panel", output.name);
            }
            _ => unreachable!(),
        }

        match &config.output[1] {
            app::Output::DdcUtil(output) => {
                assert_eq!("HDMI-A-3", output.name);
                assert_eq!("serial-123", output.identifier);
            }
            _ => unreachable!(),
        }
    }
}
