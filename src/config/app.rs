use std::{collections::HashMap, fmt};

#[derive(Debug, Clone, PartialEq)]
pub enum WaylandProtocol {
    Any,
    ExtImageCopyCaptureV1,
    WlrScreencopyUnstableV1,
    WlrExportDmabufUnstableV1,
}

impl WaylandProtocol {
    pub const fn actual_name(&self) -> Option<&'static str> {
        match self {
            Self::Any => None,
            Self::ExtImageCopyCaptureV1 => Some("ext-image-copy-capture-v1"),
            Self::WlrScreencopyUnstableV1 => Some("wlr-screencopy-unstable-v1"),
            Self::WlrExportDmabufUnstableV1 => Some("wlr-export-dmabuf-unstable-v1"),
        }
    }
}

impl fmt::Display for WaylandProtocol {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.actual_name().unwrap_or("any"))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PipewireProtocol {
    Any,
    Portal,
    Kwin,
    Mutter,
}

impl PipewireProtocol {
    pub const fn actual_name(&self) -> Option<&'static str> {
        match self {
            Self::Any => None,
            Self::Portal => Some("xdg-desktop-portal-screencast"),
            Self::Kwin => Some("zkde-screencast-unstable-v1"),
            Self::Mutter => Some("gnome-mutter-screencast"),
        }
    }
}

impl fmt::Display for PipewireProtocol {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.actual_name().unwrap_or("any"))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Capturer {
    Auto,
    Wayland(WaylandProtocol),
    Pipewire(PipewireProtocol),
    None,
}

#[derive(Clone)]
pub enum Als {
    Auto {
        thresholds: HashMap<u64, String>,
    },
    External {
        path: String,
        scale: crate::als::Scale,
        thresholds: HashMap<u64, String>,
    },
    Iio {
        path: Option<String>,
        thresholds: HashMap<u64, String>,
    },
    Time {
        levels: HashMap<u64, u64>,
        thresholds: HashMap<u64, String>,
    },
    Webcam {
        video: usize,
        thresholds: HashMap<u64, String>,
    },
    None,
}

impl fmt::Debug for Als {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Auto { .. } => f.debug_struct("Auto").finish(),
            Self::External { path, scale, .. } => f
                .debug_struct("External")
                .field("path", path)
                .field("scale", scale)
                .finish(),
            Self::Iio { path, .. } => f.debug_struct("Iio").field("path", path).finish(),
            Self::Time { levels, .. } => f.debug_struct("Time").field("levels", levels).finish(),
            Self::Webcam { video, .. } => f.debug_struct("Webcam").field("video", video).finish(),
            Self::None => write!(f, "None"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualPoint {
    pub als: u64,
    pub luma: u8,
    pub reduction: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Predictor {
    Adaptive,
    Manual { points: Vec<ManualPoint> },
}

#[derive(Clone, Default, PartialEq)]
pub enum VulkanDevice {
    #[default]
    Auto,
    Path(String),
}

impl VulkanDevice {
    pub fn as_deref(&self) -> Option<&str> {
        match self {
            Self::Auto => None,
            Self::Path(path) => Some(path),
        }
    }
}

impl From<Option<String>> for VulkanDevice {
    fn from(path: Option<String>) -> Self {
        path.map_or(Self::Auto, Self::Path)
    }
}

impl fmt::Debug for VulkanDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("Auto"),
            Self::Path(path) => path.fmt(f),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BacklightKind {
    Display,
    Keyboard,
}

#[derive(Clone, PartialEq)]
pub struct BacklightOutput {
    pub name: String,
    pub path: String,
    pub capturer: Capturer,
    pub vulkan_device: VulkanDevice,
    pub min_brightness: u64,
    pub kind: BacklightKind,
    pub predictor: Predictor,
    pub als_direction: crate::predictor::AlsDirection,
    pub gamma: bool,
    pub enabled: bool,
}

impl fmt::Debug for BacklightOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BacklightOutput")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("capturer", &self.capturer)
            .field("vulkan_device", &self.vulkan_device)
            .field("min_brightness", &self.min_brightness)
            .field("kind", &self.kind)
            .field("predictor", &self.predictor)
            .field("gamma", &self.gamma)
            .field("enabled", &self.enabled)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DdcUtilOutput {
    pub name: String,
    pub identifier: String,
    pub identifier_overridden: bool,
    pub capturer: Capturer,
    pub vulkan_device: VulkanDevice,
    pub min_brightness: u64,
    pub predictor: Predictor,
    pub gamma: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Backlight(BacklightOutput),
    DdcUtil(DdcUtilOutput),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleProfile {
    pub enabled: bool,
    pub timeout: u64,
    pub brightness: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Idle {
    pub ac: IdleProfile,
    pub battery: IdleProfile,
}

#[derive(Debug)]
pub struct Config {
    pub als: Als,
    pub idle: Option<Idle>,
    pub output: Vec<Output>,
}

#[cfg(test)]
mod tests {
    use super::{PipewireProtocol, VulkanDevice, WaylandProtocol};

    #[test]
    fn names_actual_capture_protocols() {
        let protocols = [
            (
                WaylandProtocol::ExtImageCopyCaptureV1.actual_name(),
                "ext-image-copy-capture-v1",
            ),
            (
                WaylandProtocol::WlrScreencopyUnstableV1.actual_name(),
                "wlr-screencopy-unstable-v1",
            ),
            (
                WaylandProtocol::WlrExportDmabufUnstableV1.actual_name(),
                "wlr-export-dmabuf-unstable-v1",
            ),
            (
                PipewireProtocol::Portal.actual_name(),
                "xdg-desktop-portal-screencast",
            ),
            (
                PipewireProtocol::Kwin.actual_name(),
                "zkde-screencast-unstable-v1",
            ),
            (
                PipewireProtocol::Mutter.actual_name(),
                "gnome-mutter-screencast",
            ),
        ];

        assert_eq!(WaylandProtocol::Any.actual_name(), None);
        assert_eq!(PipewireProtocol::Any.actual_name(), None);
        for (actual_name, expected) in protocols {
            assert_eq!(actual_name, Some(expected));
        }
    }

    #[test]
    fn formats_automatic_vulkan_device_for_config_dump() {
        assert_eq!(format!("{:?}", VulkanDevice::Auto), "Auto");
        assert_eq!(
            format!(
                "{:?}",
                VulkanDevice::Path("/dev/dri/renderD128".to_string())
            ),
            "\"/dev/dri/renderD128\""
        );
    }
}
