use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
mod app;
mod file;
use anyhow::{anyhow, Result};
pub use app::*;

pub fn load() -> Result<app::Config> {
    validate(parse()?)
}

fn match_predictor(predictor: file::Predictor) -> app::Predictor {
    match predictor {
        file::Predictor::Adaptive => app::Predictor::Adaptive,
        file::Predictor::Manual { thresholds } => app::Predictor::Manual {
            thresholds: thresholds
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        v.into_iter()
                            .map(|(k, v)| (k.parse::<u8>().unwrap(), v))
                            .collect(),
                    )
                })
                .collect(),
        },
    }
}

fn match_capturer(capturer: file::Capturer) -> app::Capturer {
    match capturer {
        file::Capturer::None => app::Capturer::None,
        file::Capturer::Wlroots => {
            log::warn!(
                "Config value capturer=\"wlroots\" is deprecated, use capturer=\"wayland\" instead"
            );
            app::Capturer::Wayland(app::WaylandProtocol::Any)
        }
        file::Capturer::Wayland => app::Capturer::Wayland(app::WaylandProtocol::Any),
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

fn parse() -> Result<app::Config, toml::de::Error> {
    let file_config = xdg::BaseDirectories::with_prefix("wluma")
        .ok()
        .and_then(|xdg| xdg.find_config_file("config.toml"))
        .and_then(|cfg_path| fs::read_to_string(cfg_path).ok())
        .unwrap_or_else(|| include_str!("../../config.toml").to_string());

    parse_config_str(&file_config)
}

fn parse_config_str(file_config: &str) -> Result<app::Config, toml::de::Error> {
    let parse_als_thresholds = |t: HashMap<String, String>| -> HashMap<u64, String> {
        t.into_iter()
            .map(|(k, v)| (k.parse().unwrap(), v))
            .collect()
    };

    toml::from_str(file_config).map(|file_config: file::Config| app::Config {
        output: file_config
            .output
            .backlight
            .into_iter()
            .map(|o| {
                app::Output::Backlight(app::BacklightOutput {
                    name: o.name,
                    path: o.path,
                    min_brightness: 1,
                    capturer: match_capturer(o.capturer.unwrap_or_default()),
                    predictor: match_predictor(o.predictor.unwrap_or_default()),
                })
            })
            .chain(file_config.output.ddcutil.into_iter().map(|o| {
                let identifier = o.identifier.clone().unwrap_or_else(|| o.name.clone());
                app::Output::DdcUtil(app::DdcUtilOutput {
                    name: o.name,
                    identifier,
                    min_brightness: 1,
                    capturer: match_capturer(o.capturer.unwrap_or_default()),
                    predictor: match_predictor(o.predictor.unwrap_or_default()),
                })
            }))
            .chain(file_config.keyboard.into_iter().map(|k| {
                app::Output::Backlight(app::BacklightOutput {
                    name: k.name,
                    path: k.path,
                    min_brightness: 0,
                    capturer: Capturer::None,
                    predictor: app::Predictor::Adaptive,
                })
            }))
            .collect(),

        als: match file_config.als {
            file::Als::Iio { path, thresholds } => app::Als::Iio {
                path,
                thresholds: parse_als_thresholds(thresholds),
            },
            file::Als::Webcam { video, thresholds } => app::Als::Webcam {
                video,
                thresholds: parse_als_thresholds(thresholds),
            },
            file::Als::Time { thresholds } => app::Als::Time {
                thresholds: parse_als_thresholds(thresholds),
            },
            file::Als::None => app::Als::None,
        },
    })
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

    match (names.len(), names.len() == config.output.len()) {
        (0, _) => Err(anyhow!("No output or keyboard configured")),
        (_, false) => Err(anyhow!("Names of all outputs and keyboards are not unique")),
        _ => Ok(config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
