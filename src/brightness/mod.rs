mod backlight;
mod controller;
mod ddcutil;

use anyhow::Result;
pub use backlight::Backlight;
pub use controller::{Command, CommandAction, Controller};
pub use ddcutil::DdcUtil;

#[allow(clippy::large_enum_variant)]
pub enum Brightness {
    DdcUtil(ddcutil::DdcUtil),
    Backlight(backlight::Backlight),

    #[cfg(test)]
    Mock {
        get: Vec<u64>,
        set: Vec<u64>,
        change_threshold: u64,
        transition_step_ms: u64,
        set_gate: Option<smol::channel::Receiver<()>>,
    },
}

impl Brightness {
    pub async fn get(&mut self) -> Result<u64> {
        match self {
            Brightness::DdcUtil(b) => b.get().await,
            Brightness::Backlight(b) => b.get().await,

            #[cfg(test)]
            Brightness::Mock { get, .. } => Ok(get.remove(0)),
        }
    }

    pub async fn set(&mut self, value: u64) -> Result<u64> {
        match self {
            Brightness::DdcUtil(b) => b.set(value).await,
            Brightness::Backlight(b) => b.set(value).await,

            #[cfg(test)]
            Brightness::Mock { set, set_gate, .. } => {
                if let Some(gate) = set_gate.take() {
                    gate.recv().await?;
                }
                assert_eq!(set.remove(0), value);
                Ok(value)
            }
        }
    }

    pub fn min(&self) -> u64 {
        match self {
            Brightness::DdcUtil(b) => b.min(),
            Brightness::Backlight(b) => b.min(),

            #[cfg(test)]
            Brightness::Mock { .. } => 0,
        }
    }

    pub fn max(&self) -> u64 {
        match self {
            Brightness::DdcUtil(b) => b.max(),
            Brightness::Backlight(b) => b.max(),

            #[cfg(test)]
            Brightness::Mock { .. } => 100,
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

    pub fn transition_step_ms(&self) -> u64 {
        match self {
            Brightness::DdcUtil(b) => b.transition_step_ms(),
            Brightness::Backlight(b) => b.transition_step_ms(),

            #[cfg(test)]
            Brightness::Mock {
                transition_step_ms, ..
            } => *transition_step_ms,
        }
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
