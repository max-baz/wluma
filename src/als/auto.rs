use super::{external, iio};
use anyhow::Result;
use smol::lock::Mutex;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PROBE_INTERVAL: Duration = Duration::from_secs(2);

pub struct Als {
    path: Option<PathBuf>,
    state: Mutex<State>,
    generation: AtomicU64,
    poll_interval_ms: AtomicU64,
}

struct State {
    source: Source,
    last_external_probe: Option<Instant>,
    last_iio_probe: Option<Instant>,
}

enum Source {
    External(external::Als),
    Iio(iio::Als),
    None,
}

impl Als {
    pub fn new() -> Self {
        Self {
            path: std::env::var_os("XDG_RUNTIME_DIR")
                .map(|dir| PathBuf::from(dir).join("wluma/als.sock")),
            state: Mutex::new(State {
                source: Source::None,
                last_external_probe: None,
                last_iio_probe: None,
            }),
            generation: AtomicU64::new(0),
            poll_interval_ms: AtomicU64::new(super::DEFAULT_POLL_INTERVAL.as_millis() as u64),
        }
    }

    pub async fn get(&self) -> Result<Option<u64>> {
        let mut state = self.state.lock().await;
        let external_available = self.path.as_ref().is_some_and(|path| {
            fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
        });

        if external_available {
            if let Source::External(source) = &state.source {
                match source.get().await {
                    Ok(value) => return Ok(Some(value)),
                    Err(error) => {
                        log::info!("External ALS became unavailable: {error:#}");
                        self.switch(&mut state, Source::None);
                    }
                }
            } else if let Some(path) = &self.path {
                let now = Instant::now();
                let probe = state
                    .last_external_probe
                    .is_none_or(|last| now.duration_since(last) >= PROBE_INTERVAL);
                if probe {
                    state.last_external_probe = Some(now);
                    let source = external::Als::new(path, super::Scale::Lux);
                    match source.get().await {
                        Ok(value) => {
                            log::info!("Using external ALS at '{}'", path.display());
                            self.switch(&mut state, Source::External(source));
                            return Ok(Some(value));
                        }
                        Err(error) => log::debug!("Unable to use external ALS: {error:#}"),
                    }
                }
            }
        } else if matches!(state.source, Source::External(_)) {
            log::info!("External ALS disappeared");
            self.switch(&mut state, Source::None);
        }

        if let Source::Iio(source) = &state.source {
            match source.get().await {
                Ok(value) => return Ok(Some(value)),
                Err(error) => {
                    log::info!("IIO ambient light sensor disappeared: {error:#}");
                    self.switch(&mut state, Source::None);
                }
            }
        }

        let now = Instant::now();
        let probe = state
            .last_iio_probe
            .is_none_or(|last| now.duration_since(last) >= PROBE_INTERVAL);
        if probe {
            state.last_iio_probe = Some(now);
            if let Ok(source) = iio::Als::new(Some("/sys/bus/iio/devices")).await {
                match source.get().await {
                    Ok(value) => {
                        self.switch(&mut state, Source::Iio(source));
                        return Ok(Some(value));
                    }
                    Err(error) => log::debug!("Unable to read detected IIO sensor: {error:#}"),
                }
            }
        }

        Ok(Some(0))
    }

    pub async fn kind(&self) -> &'static str {
        match &self.state.lock().await.source {
            Source::External(_) => "external",
            Source::Iio(source) => source.backend_name(),
            Source::None => "none",
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms.load(Ordering::Relaxed))
    }

    fn switch(&self, state: &mut State, source: Source) {
        let poll_interval = match &source {
            Source::External(source) => source.poll_interval(),
            Source::Iio(source) => source.poll_interval(),
            Source::None => super::DEFAULT_POLL_INTERVAL,
        };
        state.source = source;
        self.poll_interval_ms.store(
            poll_interval.as_millis().try_into().unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for Als {
    fn default() -> Self {
        Self::new()
    }
}
