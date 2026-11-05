mod backlight;
mod controller;
mod ddcutil;
#[cfg(test)]
pub(crate) mod test_support;

use anyhow::Result;
pub use backlight::Backlight;
pub use controller::{Command, CommandAction, Controller, PauseState};
pub use ddcutil::DdcUtil;

#[derive(Clone)]
pub struct Lifetime {
    signal: smol::channel::Sender<()>,
    changed: smol::channel::Receiver<()>,
}

impl Default for Lifetime {
    fn default() -> Self {
        let (signal, changed) = smol::channel::unbounded();
        Self { signal, changed }
    }
}

impl Lifetime {
    pub fn invalidate(&self) {
        self.signal.close();
    }

    pub fn is_valid(&self) -> bool {
        !self.signal.is_closed()
    }

    pub async fn changed(&self) {
        let _ = self.changed.recv().await;
    }
}

#[derive(Debug)]
pub struct BackendChanged(pub String);

impl std::fmt::Display for BackendChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BackendChanged {}

pub fn validate_limits(minimum: u64, maximum: u64) -> Result<()> {
    anyhow::ensure!(maximum > 0, "Hardware brightness maximum must be nonzero");
    anyhow::ensure!(
        minimum <= maximum,
        "Configured brightness minimum {minimum} exceeds hardware maximum {maximum}"
    );
    Ok(())
}

#[allow(clippy::large_enum_variant)]
pub enum Brightness {
    DdcUtil(ddcutil::DdcUtil),
    Backlight(backlight::Backlight),

    #[cfg(test)]
    Mock {
        get: Vec<u64>,
        set: Vec<u64>,
        change_threshold: u64,
        min: u64,
        max: u64,
        get_error: bool,
        set_error: bool,
        get_gate: Option<smol::channel::Receiver<()>>,
        set_gate: Option<smol::channel::Receiver<()>>,
    },
}

impl Brightness {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Backlight(backlight) => backlight.validate(),
            _ => Ok(()),
        }
    }

    pub async fn set_after_read(&mut self, value: u64) -> Result<u64> {
        match self {
            Self::DdcUtil(ddc) => ddc.set_after_read(value).await,
            _ => self.set(value).await,
        }
    }

    pub async fn get(&mut self) -> Result<u64> {
        match self {
            Brightness::DdcUtil(b) => b.get().await,
            Brightness::Backlight(b) => b.get().await,

            #[cfg(test)]
            Brightness::Mock {
                get,
                get_error,
                get_gate,
                ..
            } => {
                if let Some(gate) = get_gate.take() {
                    gate.recv().await?;
                }
                anyhow::ensure!(!*get_error, "Read failed");
                Ok(get.remove(0))
            }
        }
    }

    pub async fn set(&mut self, value: u64) -> Result<u64> {
        match self {
            Brightness::DdcUtil(b) => b.set(value).await,
            Brightness::Backlight(b) => b.set(value).await,

            #[cfg(test)]
            Brightness::Mock {
                set,
                set_gate,
                set_error,
                ..
            } => {
                if let Some(gate) = set_gate.take() {
                    gate.recv().await?;
                }
                assert_eq!(set.remove(0), value);
                anyhow::ensure!(!*set_error, "Write failed");
                Ok(value)
            }
        }
    }

    pub fn min(&self) -> u64 {
        match self {
            Brightness::DdcUtil(b) => b.min(),
            Brightness::Backlight(b) => b.min(),

            #[cfg(test)]
            Brightness::Mock { min, .. } => *min,
        }
    }

    pub fn max(&self) -> u64 {
        match self {
            Brightness::DdcUtil(b) => b.max(),
            Brightness::Backlight(b) => b.max(),

            #[cfg(test)]
            Brightness::Mock { max, .. } => *max,
        }
    }

    pub fn percent(&self, value: u64) -> u8 {
        value
            .saturating_mul(100)
            .saturating_add(self.max() / 2)
            .checked_div(self.max())
            .unwrap_or(0)
            .min(100) as u8
    }

    pub fn value_at_percent(&self, percent: u8) -> u64 {
        self.max()
            .saturating_mul(percent as u64)
            .saturating_add(50)
            .checked_div(100)
            .unwrap_or(0)
            .clamp(self.min(), self.max())
    }

    pub fn change_threshold(&self) -> u64 {
        match self {
            Brightness::DdcUtil(_) => 1,
            Brightness::Backlight(b) => b.change_threshold(),

            #[cfg(test)]
            Brightness::Mock {
                change_threshold, ..
            } => *change_threshold,
        }
    }

    pub fn waiting_sleep_ms(&self) -> u64 {
        match self {
            Brightness::DdcUtil(b) => b.waiting_sleep_ms(),
            Brightness::Backlight(_) => 100,

            #[cfg(test)]
            Brightness::Mock { .. } => 100,
        }
    }
}
