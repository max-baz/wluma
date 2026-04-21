mod backlight;
mod controller;
mod ddcutil;

use anyhow::Result;
pub use backlight::Backlight;
pub use controller::Controller;
pub use ddcutil::DdcUtil;

#[allow(clippy::large_enum_variant)]
pub enum Brightness {
    DdcUtil(ddcutil::DdcUtil),
    Backlight(backlight::Backlight),

    #[cfg(test)]
    Mock {
        get: Vec<u64>,
        set: Vec<u64>,
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
            Brightness::Mock { set, .. } => {
                assert_eq!(set.remove(0), value);
                Ok(value)
            }
        }
    }

    pub fn transition_step_ms(&self) -> u64 {
        match self {
            Brightness::DdcUtil(b) => b.transition_step_ms(),
            Brightness::Backlight(_) => 1,

            #[cfg(test)]
            Brightness::Mock { .. } => 1,
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
