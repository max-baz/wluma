use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Debug, Default)]
pub enum Capturer {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "wlroots")]
    Wlroots,
    #[serde(rename = "wayland")]
    Wayland,
    #[serde(rename = "wlr-export-dmabuf-unstable-v1")]
    WlrExportDmabufUnstableV1,
    #[serde(rename = "wlr-screencopy-unstable-v1")]
    WlrScreencopyUnstableV1,
    #[serde(rename = "ext-image-copy-capture-v1")]
    ExtImageCopyCaptureV1,
    #[serde(rename = "pipewire")]
    Pipewire,
    #[serde(rename = "xdg-desktop-portal-screencast")]
    XdgDesktopPortalScreencast,
    #[serde(rename = "zkde-screencast-unstable-v1")]
    ZkdeScreencastUnstableV1,
    #[serde(rename = "gnome-mutter-screencast")]
    GnomeMutterScreencast,
    #[serde(rename = "none")]
    None,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum AlsScale {
    #[default]
    Lux,
    Linear,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Als {
    External {
        path: Option<String>,
        #[serde(default)]
        scale: AlsScale,
    },
    Iio {
        path: Option<String>,
        thresholds: Option<HashMap<String, String>>,
    },
    Time {
        levels: Option<HashMap<String, u64>>,
        thresholds: Option<HashMap<String, String>>,
    },
    Webcam {
        video: usize,
        thresholds: Option<HashMap<String, String>>,
    },
    None,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
pub struct OutputByType {
    pub backlight: Vec<BacklightOutput>,
    pub ddcutil: Vec<DdcUtilOutput>,
}

#[derive(Deserialize, Debug)]
pub struct ManualPoint {
    pub als: u64,
    pub luma: u8,
    pub reduction: u64,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Predictor {
    #[default]
    Adaptive,
    Manual {
        #[serde(default)]
        points: Vec<ManualPoint>,
        thresholds: Option<HashMap<String, HashMap<String, u64>>>,
    },
}

#[derive(Deserialize, Debug)]
pub struct BacklightOutput {
    pub name: String,
    pub path: Option<String>,
    pub capturer: Option<Capturer>,
    pub vulkan_device: Option<String>,
    pub predictor: Option<Predictor>,
    pub gamma: Option<bool>,
}

#[derive(Deserialize, Debug)]
pub struct DdcUtilOutput {
    pub name: String,
    pub identifier: Option<String>,
    pub capturer: Option<Capturer>,
    pub vulkan_device: Option<String>,
    pub predictor: Option<Predictor>,
    pub gamma: Option<bool>,
}

#[derive(Deserialize, Debug)]
pub struct Keyboard {
    pub name: String,
    pub path: String,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
pub struct IdleProfile {
    pub enabled: Option<bool>,
    pub timeout: Option<u64>,
    pub brightness: Option<u8>,
}

#[derive(Deserialize, Debug)]
#[serde(default)]
pub struct Idle {
    pub enabled: bool,
    pub timeout: u64,
    pub brightness: u8,
    pub ac: IdleProfile,
    pub battery: IdleProfile,
}

impl Default for Idle {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout: 120,
            brightness: 30,
            ac: IdleProfile::default(),
            battery: IdleProfile::default(),
        }
    }
}

#[derive(Deserialize, Debug, Default)]
pub struct Config {
    pub als: Option<Als>,
    pub idle: Option<Idle>,
    #[serde(default)]
    pub output: OutputByType,
    #[serde(default)]
    pub keyboard: Vec<Keyboard>,
}
