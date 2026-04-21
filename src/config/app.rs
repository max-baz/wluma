use std::{collections::HashMap, fmt};

#[derive(Debug, Clone, PartialEq)]
pub enum WaylandProtocol {
    Any,
    ExtImageCopyCaptureV1,
    WlrScreencopyUnstableV1,
    WlrExportDmabufUnstableV1,
}

impl fmt::Display for WaylandProtocol {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let output = match self {
            Self::Any => "any",
            Self::ExtImageCopyCaptureV1 => "ext-image-copy-capture-v1",
            Self::WlrScreencopyUnstableV1 => "wlr-screencopy-unstable-v1",
            Self::WlrExportDmabufUnstableV1 => "wlr-export-dmabuf-unstable-v1",
        };
        write!(f, "{}", output)
    }
}

#[derive(Debug, Clone)]
pub enum Capturer {
    Wayland(WaylandProtocol),
    None,
}

#[derive(Debug)]
pub enum Als {
    Iio {
        path: String,
        thresholds: HashMap<u64, String>,
    },
    Time {
        thresholds: HashMap<u64, String>,
    },
    Webcam {
        video: usize,
        thresholds: HashMap<u64, String>,
    },
    None,
}

#[derive(Debug, Clone)]
pub enum Predictor {
    Adaptive,
    Manual {
        thresholds: HashMap<String, HashMap<u8, u64>>,
    },
}

#[derive(Debug, Clone)]
pub struct BacklightOutput {
    pub name: String,
    pub path: String,
    pub capturer: Capturer,
    pub min_brightness: u64,
    pub predictor: Predictor,
}

#[derive(Debug, Clone)]
pub struct DdcUtilOutput {
    pub name: String,
    pub identifier: String,
    pub capturer: Capturer,
    pub min_brightness: u64,
    pub predictor: Predictor,
}

#[derive(Debug, Clone)]
pub enum Output {
    Backlight(BacklightOutput),
    DdcUtil(DdcUtilOutput),
}

#[derive(Debug)]
pub struct Config {
    pub als: Als,
    pub output: Vec<Output>,
}
