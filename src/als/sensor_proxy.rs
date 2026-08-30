use anyhow::{anyhow, Result};
use dbus::arg::RefArg;
use dbus::blocking::stdintf::org_freedesktop_dbus::{Properties, PropertiesPropertiesChanged};
use dbus::blocking::Connection;
use dbus::message::{MatchRule, SignalArgs};
use smol::channel::{self, Receiver, TryRecvError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const DESTINATION: &str = "net.hadess.SensorProxy";
const PATH: &str = "/net/hadess/SensorProxy";
const INTERFACE: &str = "net.hadess.SensorProxy";
const TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
const INITIALIZATION_ATTEMPTS: usize = 5;
const INITIALIZATION_INTERVAL: Duration = Duration::from_millis(200);

pub struct Sensor {
    value_rx: Receiver<u64>,
    value: u64,
    active: Arc<AtomicBool>,
    reconnect_at: Option<Instant>,
}

impl Sensor {
    pub fn new() -> Result<Self> {
        if !service_available()? {
            return Err(anyhow!("iio-sensor-proxy is unavailable"));
        }

        let mut error = None;
        for attempt in 1..=INITIALIZATION_ATTEMPTS {
            match Self::connect() {
                Ok(sensor) => return Ok(sensor),
                Err(current) => error = Some(current),
            }
            if attempt < INITIALIZATION_ATTEMPTS {
                thread::sleep(INITIALIZATION_INTERVAL);
            }
        }
        Err(error.expect("initialization loop always records an error"))
    }

    fn connect() -> Result<Self> {
        let connection = Connection::new_system()?;
        let proxy = connection.with_proxy(DESTINATION, PATH, TIMEOUT);
        let has_ambient_light: bool = proxy.get(INTERFACE, "HasAmbientLight")?;
        if !has_ambient_light {
            return Err(anyhow!("iio-sensor-proxy has no ambient light sensor"));
        }

        let unit: String = proxy.get(INTERFACE, "LightLevelUnit")?;
        if unit != "lux" {
            return Err(anyhow!(
                "iio-sensor-proxy reports unsupported light level unit '{unit}'"
            ));
        }

        let _: () = proxy.method_call(INTERFACE, "ClaimLight", ())?;
        let value = light_level(proxy.get(INTERFACE, "LightLevel")?)?;
        let (value_tx, value_rx) = channel::bounded(128);
        let signal_tx = value_tx.clone();
        let rule = PropertiesPropertiesChanged::match_rule(None, None).with_path(PATH);

        connection.add_match(rule, move |changed: PropertiesPropertiesChanged, _, _| {
            if changed.interface_name == INTERFACE {
                if let Some(value) = changed
                    .changed_properties
                    .get("LightLevel")
                    .and_then(|value| value.0.as_f64())
                    .and_then(|value| light_level(value).ok())
                {
                    let _ = signal_tx.try_send(value);
                }
            }
            true
        })?;

        let active = Arc::new(AtomicBool::new(true));
        let signal_active = active.clone();
        let owner_rule = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged")
            .with_path("/org/freedesktop/DBus");
        connection.add_match(
            owner_rule,
            move |(name, old_owner, new_owner): (String, String, String), _, _| {
                if name == DESTINATION && !old_owner.is_empty() && old_owner != new_owner {
                    signal_active.store(false, Ordering::Relaxed);
                }
                true
            },
        )?;

        let thread_active = active.clone();
        thread::spawn(move || {
            while thread_active.load(Ordering::Relaxed)
                && connection.process(Duration::from_secs(1)).is_ok()
            {}
        });

        Ok(Self {
            value_rx,
            value,
            active,
            reconnect_at: None,
        })
    }

    pub async fn get_raw(&mut self) -> u64 {
        loop {
            match self.value_rx.try_recv() {
                Ok(value) => self.value = value,
                Err(TryRecvError::Empty) => return self.value,
                Err(TryRecvError::Closed) => break,
            }
        }

        if self
            .reconnect_at
            .is_some_and(|reconnect_at| reconnect_at > Instant::now())
        {
            return self.value;
        }

        match smol::unblock(Self::new).await {
            Ok(sensor) => {
                log::info!("Reconnected to iio-sensor-proxy");
                *self = sensor;
            }
            Err(error) => {
                if self.reconnect_at.is_none() {
                    log::warn!("Lost connection to iio-sensor-proxy; attempting to reconnect");
                }
                log::debug!("Unable to reconnect to iio-sensor-proxy: {error}");
                self.reconnect_at = Some(Instant::now() + RECONNECT_INTERVAL);
            }
        }

        self.value
    }
}

fn service_available() -> Result<bool> {
    let connection = Connection::new_system()?;
    let proxy = connection.with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", TIMEOUT);
    let (has_owner,): (bool,) =
        proxy.method_call("org.freedesktop.DBus", "NameHasOwner", (DESTINATION,))?;
    if has_owner {
        return Ok(true);
    }
    let (activatable,): (Vec<String>,) =
        proxy.method_call("org.freedesktop.DBus", "ListActivatableNames", ())?;
    Ok(activatable.iter().any(|name| name == DESTINATION))
}

impl Drop for Sensor {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
    }
}

fn light_level(value: f64) -> Result<u64> {
    if value.is_finite() && value >= 0.0 {
        Ok(value.round() as u64)
    } else {
        Err(anyhow!("Invalid iio-sensor-proxy light level '{value}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_light_levels() {
        assert_eq!(light_level(42.9).unwrap(), 43);
        assert_eq!(light_level(0.0).unwrap(), 0);
        assert!(light_level(-1.0).is_err());
        assert!(light_level(f64::NAN).is_err());
    }
}
