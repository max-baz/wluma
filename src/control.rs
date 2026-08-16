use anyhow::{anyhow, Context, Result};
use smol::channel::{Receiver, Sender};
use smol::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use smol::net::unix::{UnixListener, UnixStream};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SOCKET_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    Brightness,
    Dim,
    Temperature,
}

impl Property {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Brightness => "brightness",
            Self::Dim => "dim",
            Self::Temperature => "temperature",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Adjustment {
    pub percent: u8,
    pub relative: bool,
    pub increase: bool,
}

#[derive(Clone, Debug)]
pub struct ValueAdjustment {
    pub value: u64,
    pub relative: bool,
    pub increase: bool,
}

#[derive(Debug)]
pub enum Action {
    Get(Property, String),
    Set(Property, String, ValueAdjustment),
    Pause(Option<String>, Option<Duration>),
    Resume(Option<String>),
    Toggle(Option<String>),
}

pub struct Command {
    pub action: Action,
    pub response: Sender<std::result::Result<String, String>>,
}

pub struct InstanceLock {
    _file: File,
}

#[derive(Clone)]
pub struct Hub(Arc<Mutex<State>>);

#[derive(Clone)]
struct State {
    als: AlsStatus,
    idle: IdleStatus,
    screens: BTreeMap<String, ScreenStatus>,
    monitors: BTreeMap<String, MonitorStatus>,
    watchers: Vec<Sender<Snapshot>>,
}

#[derive(Clone, PartialEq)]
struct AlsStatus {
    kind: String,
    value: Option<u64>,
}

#[derive(Clone, PartialEq)]
struct IdleStatus {
    source: Option<String>,
    enabled: bool,
    timeout: Option<u64>,
    brightness: Option<u8>,
    idled: bool,
}

#[derive(Clone, PartialEq)]
struct ScreenStatus {
    capturer: String,
    luma: Option<u8>,
}

#[derive(Clone, PartialEq)]
struct MonitorStatus {
    kind: String,
    brightness: Option<u8>,
    dim: Option<u8>,
    temperature: Option<u32>,
    manual_pause: bool,
    idle_pause: bool,
}

#[derive(Clone, PartialEq)]
struct Snapshot {
    als: AlsStatus,
    idle: IdleStatus,
    screens: BTreeMap<String, ScreenStatus>,
    monitors: BTreeMap<String, MonitorStatus>,
}

impl Hub {
    pub fn new(als_kind: impl Into<String>) -> Self {
        Self(Arc::new(Mutex::new(State {
            als: AlsStatus {
                kind: als_kind.into(),
                value: None,
            },
            idle: IdleStatus {
                source: None,
                enabled: false,
                timeout: None,
                brightness: None,
                idled: false,
            },
            screens: BTreeMap::new(),
            monitors: BTreeMap::new(),
            watchers: Vec::new(),
        })))
    }

    pub fn set_als(&self, kind: &str, value: u64) {
        self.change(|state| {
            state.als.kind = kind.to_string();
            state.als.value = Some(value);
        });
    }

    pub fn add_output(&self, name: &str, kind: &str, capturer: Option<&str>) {
        self.change(|state| {
            state.monitors.insert(
                name.to_string(),
                MonitorStatus {
                    kind: kind.to_string(),
                    brightness: None,
                    dim: None,
                    temperature: None,
                    manual_pause: false,
                    idle_pause: false,
                },
            );
            if let Some(capturer) = capturer {
                state.screens.insert(
                    name.to_string(),
                    ScreenStatus {
                        capturer: capturer.to_string(),
                        luma: None,
                    },
                );
            }
        });
    }

    pub fn remove_output(&self, name: &str) {
        self.change(|state| {
            state.screens.remove(name);
            state.monitors.remove(name);
        });
    }

    pub fn set_capturer(&self, name: &str, capturer: &str) {
        self.change(|state| {
            if let Some(screen) = state.screens.get_mut(name) {
                screen.capturer = capturer.to_string();
            }
        });
    }

    pub fn set_luma(&self, name: &str, luma: u8) {
        self.change(|state| {
            if let Some(screen) = state.screens.get_mut(name) {
                screen.luma = Some(luma);
            }
        });
    }

    pub fn set_brightness(&self, name: &str, brightness: u8) {
        self.change(|state| {
            if let Some(monitor) = state.monitors.get_mut(name) {
                monitor.brightness = Some(brightness);
            }
        });
    }

    pub fn set_gamma(&self, name: &str, dim: u8, temperature: u32) {
        self.change(|state| {
            if let Some(monitor) = state.monitors.get_mut(name) {
                monitor.dim = Some(dim);
                monitor.temperature = Some(temperature);
            }
        });
    }

    pub fn clear_gamma(&self, name: &str) {
        self.change(|state| {
            if let Some(monitor) = state.monitors.get_mut(name) {
                monitor.dim = None;
                monitor.temperature = None;
            }
        });
    }

    pub fn set_pause(&self, name: &str, manual: bool, idle: bool) {
        self.change(|state| {
            if let Some(monitor) = state.monitors.get_mut(name) {
                monitor.manual_pause = manual;
                monitor.idle_pause = idle;
            }
        });
    }

    pub fn set_idle_profile(&self, source: &str, enabled: bool, timeout: u64, brightness: u8) {
        self.change(|state| {
            state.idle.source = Some(source.to_string());
            state.idle.enabled = enabled;
            state.idle.timeout = Some(timeout);
            state.idle.brightness = Some(brightness);
        });
    }

    pub fn set_idled(&self, idled: bool) {
        self.change(|state| state.idle.idled = idled);
    }

    fn change(&self, update: impl FnOnce(&mut State)) {
        let mut state = self.0.lock().unwrap();
        let previous = state.snapshot();
        update(&mut state);
        let snapshot = state.snapshot();
        if snapshot != previous {
            state
                .watchers
                .retain(|watcher| watcher.try_send(snapshot.clone()).is_ok());
        }
    }

    fn snapshot(&self) -> Snapshot {
        self.0.lock().unwrap().snapshot()
    }

    fn subscribe(&self) -> Receiver<Snapshot> {
        let (tx, rx) = smol::channel::bounded(16);
        let mut state = self.0.lock().unwrap();
        let _ = tx.try_send(state.snapshot());
        state.watchers.push(tx);
        rx
    }
}

impl State {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            als: self.als.clone(),
            idle: self.idle.clone(),
            screens: self.screens.clone(),
            monitors: self.monitors.clone(),
        }
    }
}

fn runtime_dir() -> Result<PathBuf> {
    let runtime =
        std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime).join("wluma"))
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("control.sock"))
}

impl InstanceLock {
    pub fn acquire() -> Result<Self> {
        let directory = runtime_dir()?;
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let path = directory.join("daemon.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::WouldBlock {
                return Err(anyhow!("another wluma daemon is already running"));
            }
            return Err(error.into());
        }
        Ok(Self { _file: file })
    }
}

pub async fn serve(hub: Hub, commands: Sender<Command>) -> Result<()> {
    let path = socket_path()?;
    let mut initial = true;
    loop {
        let listener = loop {
            match bind(&path, initial) {
                Ok(listener) => break listener,
                Err(error) if initial => return Err(error),
                Err(error) => {
                    log::warn!("Unable to restore control socket: {error:#}");
                    smol::Timer::after(SOCKET_CHECK_INTERVAL).await;
                }
            }
        };
        initial = false;
        let metadata = fs::metadata(&path)?;
        let identity = (metadata.dev(), metadata.ino());
        loop {
            enum Event {
                Connection(std::io::Result<UnixStream>),
                Check,
            }
            let event = smol::future::race(
                async { Event::Connection(listener.accept().await.map(|(stream, _)| stream)) },
                async {
                    smol::Timer::after(SOCKET_CHECK_INTERVAL).await;
                    Event::Check
                },
            )
            .await;
            match event {
                Event::Connection(Ok(stream)) => {
                    let hub = hub.clone();
                    let commands = commands.clone();
                    smol::spawn(async move {
                        if let Err(error) = handle(stream, hub, commands).await {
                            log::debug!("Control connection failed: {error:#}");
                        }
                    })
                    .detach();
                }
                Event::Connection(Err(error)) => return Err(error.into()),
                Event::Check if !socket_is_healthy(&path, identity) => {
                    log::warn!("Control socket disappeared or was damaged; restoring it");
                    break;
                }
                Event::Check => {}
            }
        }
    }
}

fn bind(path: &Path, check_existing: bool) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    if check_existing && std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(anyhow!("another wluma daemon is already running"));
    }
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn socket_is_healthy(path: &Path, identity: (u64, u64)) -> bool {
    fs::metadata(path).is_ok_and(|metadata| {
        (metadata.dev(), metadata.ino()) == identity
            && metadata.file_type().is_socket()
            && metadata.permissions().mode() & 0o777 == 0o600
    })
}

async fn handle(mut stream: UnixStream, hub: Hub, commands: Sender<Command>) -> Result<()> {
    let mut line = String::new();
    BufReader::new(&mut stream).read_line(&mut line).await?;
    let fields = line.trim_end().split('\t').collect::<Vec<_>>();
    match fields.as_slice() {
        ["status"] => write_response(&mut stream, &hub.snapshot().tables()).await?,
        ["status-json"] => write_response(&mut stream, &hub.snapshot().json()).await?,
        ["watch"] | ["watch-json"] => {
            let json = fields[0] == "watch-json";
            let updates = hub.subscribe();
            while let Ok(snapshot) = updates.recv().await {
                let line = if json {
                    snapshot.json()
                } else {
                    snapshot.line()
                };
                write_response(&mut stream, &line).await?;
            }
        }
        _ => {
            let action = parse_action(&fields)?;
            let (response_tx, response_rx) = smol::channel::bounded(1);
            commands
                .send(Command {
                    action,
                    response: response_tx,
                })
                .await?;
            match response_rx.recv().await? {
                Ok(value) => write_response(&mut stream, &value).await?,
                Err(error) => {
                    write_response(&mut stream, &format!("error\t{error}")).await?;
                }
            }
        }
    }
    Ok(())
}

fn parse_action(fields: &[&str]) -> Result<Action> {
    match fields {
        ["get", property, name] => Ok(Action::Get(parse_property(property)?, (*name).to_string())),
        ["set", property, name, value] => {
            let property = parse_property(property)?;
            Ok(Action::Set(
                property,
                (*name).to_string(),
                parse_value_adjustment(property, value)?,
            ))
        }
        ["pause", target, duration] => Ok(Action::Pause(
            parse_target(target),
            if *duration == "-" {
                None
            } else {
                Some(Duration::from_secs(duration.parse()?))
            },
        )),
        ["resume", target] => Ok(Action::Resume(parse_target(target))),
        ["toggle", target] => Ok(Action::Toggle(parse_target(target))),
        _ => Err(anyhow!("invalid control request")),
    }
}

fn parse_target(value: &str) -> Option<String> {
    (value != "*").then(|| value.to_string())
}

pub fn parse_property(value: &str) -> Result<Property> {
    match value {
        "brightness" => Ok(Property::Brightness),
        "dim" => Ok(Property::Dim),
        "temperature" => Ok(Property::Temperature),
        _ => Err(anyhow!("property must be brightness, dim, or temperature")),
    }
}

pub fn parse_value_adjustment(property: Property, value: &str) -> Result<ValueAdjustment> {
    if property == Property::Temperature && value == "neutral" {
        return Ok(ValueAdjustment {
            value: crate::gamma::ramp::NEUTRAL_TEMPERATURE,
            relative: false,
            increase: true,
        });
    }
    let suffix = match property {
        Property::Brightness | Property::Dim => '%',
        Property::Temperature => 'K',
    };
    let invalid = || match property {
        Property::Brightness | Property::Dim => {
            anyhow!("value must be a percentage from 0% to 100%")
        }
        Property::Temperature => {
            anyhow!("temperature must be neutral or between 1000K and 25000K")
        }
    };
    let (relative, increase, value) = if let Some(value) = value.strip_prefix('+') {
        (true, true, value)
    } else if let Some(value) = value.strip_prefix('-') {
        (true, false, value)
    } else if let Some(value) = value.strip_suffix('+') {
        (true, true, value)
    } else if let Some(value) = value.strip_suffix('-') {
        (true, false, value)
    } else {
        (false, true, value)
    };
    let number = value.strip_suffix(suffix).ok_or_else(invalid)?;
    let value = number.parse::<u64>().map_err(|_| invalid())?;
    let valid = match property {
        Property::Brightness | Property::Dim => value <= 100,
        Property::Temperature if relative => true,
        Property::Temperature => (1000..=25000).contains(&value),
    };
    if !valid {
        return Err(invalid());
    }
    Ok(ValueAdjustment {
        value,
        relative,
        increase,
    })
}

impl TryFrom<ValueAdjustment> for Adjustment {
    type Error = anyhow::Error;

    fn try_from(value: ValueAdjustment) -> Result<Self> {
        Ok(Self {
            percent: value
                .value
                .try_into()
                .context("brightness adjustment is too large")?,
            relative: value.relative,
            increase: value.increase,
        })
    }
}

async fn write_response(stream: &mut UnixStream, value: &str) -> Result<()> {
    stream.write_all(value.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

pub async fn send(request: &str, stream: bool) -> Result<()> {
    let path = socket_path()?;
    if !stream {
        return send_once(&path, request).await;
    }

    let mut connected = false;
    loop {
        match UnixStream::connect(&path).await {
            Ok(mut socket) => {
                connected = true;
                if write_request(&mut socket, request).await.is_ok() {
                    let mut reader = BufReader::new(socket);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => println!("{}", line.trim_end()),
                        }
                    }
                }
            }
            Err(error) if !connected => {
                return Err(error).with_context(|| {
                    format!("unable to connect to wluma daemon at {}", path.display())
                });
            }
            Err(_) => {}
        }
        smol::Timer::after(RECONNECT_INTERVAL).await;
    }
}

async fn send_once(path: &Path, request: &str) -> Result<()> {
    let mut socket = UnixStream::connect(path)
        .await
        .with_context(|| format!("unable to connect to wluma daemon at {}", path.display()))?;
    write_request(&mut socket, request).await?;
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    while reader.read_line(&mut line).await? != 0 {
        let value = line.trim_end();
        if let Some(error) = value.strip_prefix("error\t") {
            return Err(anyhow!(error.to_string()));
        }
        println!("{value}");
        line.clear();
    }
    Ok(())
}

async fn write_request(socket: &mut UnixStream, request: &str) -> Result<()> {
    socket.write_all(request.as_bytes()).await?;
    socket.write_all(b"\n").await?;
    socket.flush().await?;
    Ok(())
}

impl IdleStatus {
    fn state(&self) -> &str {
        if !self.enabled {
            "disabled"
        } else if self.idled {
            "idle"
        } else {
            "active"
        }
    }
}

impl MonitorStatus {
    fn state(&self) -> &str {
        match (self.manual_pause, self.idle_pause) {
            (false, false) => "active",
            (true, false) => "manual",
            (false, true) => "idle",
            (true, true) => "idle+manual",
        }
    }

    fn paused(&self) -> bool {
        self.manual_pause || self.idle_pause
    }
}

impl Snapshot {
    fn tables(&self) -> String {
        let als_width = "ALS TYPE".len().max(self.als.kind.len());
        let mut lines = vec![
            format!("{:<als_width$}  VALUE", "ALS TYPE"),
            format!(
                "{:<als_width$}  {}",
                self.als.kind,
                optional(self.als.value, "")
            ),
            String::new(),
        ];
        let idle_headers = [
            "POWER SOURCE",
            "IDLE STATE",
            "ENABLED",
            "TIMEOUT",
            "BRIGHTNESS",
        ];
        let idle_row = [
            self.idle.source.as_deref().unwrap_or("-").to_string(),
            self.idle.state().to_string(),
            if self.idle.enabled { "yes" } else { "no" }.to_string(),
            optional(self.idle.timeout, "s"),
            optional(self.idle.brightness, "%"),
        ];
        let idle_widths = std::array::from_fn::<_, 5, _>(|column| {
            idle_headers[column].len().max(idle_row[column].len())
        });
        lines.push(format_row(&idle_headers, &idle_widths));
        lines.push(format_row(&idle_row, &idle_widths));
        lines.push(String::new());
        let headers = [
            "OUTPUT",
            "TYPE",
            "CAPTURER",
            "LUMA",
            "BRIGHTNESS",
            "DIM",
            "TEMPERATURE",
            "STATE",
        ];
        let rows = self
            .monitors
            .iter()
            .map(|(name, monitor)| {
                let screen = self.screens.get(name);
                [
                    name.to_string(),
                    monitor.kind.clone(),
                    screen.map_or_else(|| "-".to_string(), |screen| screen.capturer.clone()),
                    optional(screen.and_then(|screen| screen.luma), "%"),
                    optional(monitor.brightness, "%"),
                    optional(monitor.dim, "%"),
                    optional(monitor.temperature, "K"),
                    monitor.state().to_string(),
                ]
            })
            .collect::<Vec<_>>();
        let widths = std::array::from_fn::<_, 8, _>(|column| {
            rows.iter()
                .map(|row| row[column].len())
                .max()
                .unwrap_or(0)
                .max(headers[column].len())
        });
        lines.push(format_row(&headers, &widths));
        lines.extend(rows.iter().map(|row| format_row(row, &widths)));
        lines.join("\n")
    }

    fn line(&self) -> String {
        let mut parts = vec![
            format!("als={}", optional(self.als.value, "")),
            format!(
                "power={},idle={},idle-timeout={},idle-brightness={}",
                self.idle.source.as_deref().unwrap_or("-"),
                self.idle.state(),
                optional(self.idle.timeout, "s"),
                optional(self.idle.brightness, "%")
            ),
        ];
        for (name, monitor) in &self.monitors {
            let luma = self.screens.get(name).and_then(|screen| screen.luma);
            parts.push(format!(
                "{name}:luma={},brightness={},dim={},temperature={},{}",
                optional(luma, "%"),
                optional(monitor.brightness, "%"),
                optional(monitor.dim, "%"),
                optional(monitor.temperature, "K"),
                monitor.state()
            ));
        }
        parts.join(" ")
    }

    fn json(&self) -> String {
        let outputs = self
            .monitors
            .iter()
            .map(|(name, monitor)| {
                let screen = self.screens.get(name);
                format!(
                    "{{\"name\":{},\"type\":{},\"capturer\":{},\"luma\":{},\"brightness\":{},\"dim\":{},\"temperature\":{},\"state\":{},\"paused\":{},\"idle\":{}}}",
                    json_string(name),
                    json_string(&monitor.kind),
                    screen.map_or_else(|| "null".to_string(), |screen| json_string(&screen.capturer)),
                    json_option(screen.and_then(|screen| screen.luma)),
                    json_option(monitor.brightness),
                    json_option(monitor.dim),
                    json_option(monitor.temperature),
                    json_string(monitor.state()),
                    monitor.paused(),
                    monitor.idle_pause
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"als\":{{\"type\":{},\"value\":{}}},\"idle\":{{\"power_source\":{},\"state\":{},\"enabled\":{},\"timeout\":{},\"brightness\":{}}},\"outputs\":[{}]}}",
            json_string(&self.als.kind),
            json_option(self.als.value),
            self.idle.source.as_ref().map_or_else(|| "null".to_string(), |source| json_string(source)),
            json_string(self.idle.state()),
            self.idle.enabled,
            json_option(self.idle.timeout),
            json_option(self.idle.brightness),
            outputs
        )
    }
}

fn format_row<T: AsRef<str>, const N: usize>(values: &[T; N], widths: &[usize; N]) -> String {
    values
        .iter()
        .zip(widths)
        .map(|(value, width)| format!("{:<width$}", value.as_ref()))
        .collect::<Vec<_>>()
        .join("  ")
        .trim_end()
        .to_string()
}

fn optional<T: std::fmt::Display>(value: Option<T>, suffix: &str) -> String {
    value.map_or_else(|| "-".to_string(), |value| format!("{value}{suffix}"))
}

fn json_option<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "null".to_string(), |value| value.to_string())
}

fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '\"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                output.push_str(&format!("\\u{:04x}", character as u32))
            }
            character => output.push(character),
        }
    }
    output.push('\"');
    output
}

#[cfg(test)]
mod tests {
    use super::{parse_value_adjustment, Hub, Property};

    #[test]
    fn parses_brightness_adjustments() {
        let absolute = parse_value_adjustment(Property::Brightness, "60%").unwrap();
        assert_eq!(absolute.value, 60);
        assert!(!absolute.relative);

        for value in ["+5%", "5%+"] {
            let adjustment = parse_value_adjustment(Property::Brightness, value).unwrap();
            assert_eq!(adjustment.value, 5);
            assert!(adjustment.relative);
            assert!(adjustment.increase);
        }

        for value in ["-5%", "5%-"] {
            let adjustment = parse_value_adjustment(Property::Brightness, value).unwrap();
            assert_eq!(adjustment.value, 5);
            assert!(adjustment.relative);
            assert!(!adjustment.increase);
        }

        assert!(parse_value_adjustment(Property::Brightness, "60").is_err());
        assert!(parse_value_adjustment(Property::Brightness, "+5").is_err());
        assert!(parse_value_adjustment(Property::Brightness, "101%").is_err());
        assert!(parse_value_adjustment(Property::Brightness, "+101%").is_err());
    }

    #[test]
    fn parses_gamma_adjustments() {
        let dim = parse_value_adjustment(Property::Dim, "+5%").unwrap();
        assert_eq!(dim.value, 5);
        assert!(dim.relative);
        assert!(dim.increase);

        let temperature = parse_value_adjustment(Property::Temperature, "4500K").unwrap();
        assert_eq!(temperature.value, 4500);
        assert!(!temperature.relative);

        let neutral = parse_value_adjustment(Property::Temperature, "neutral").unwrap();
        assert_eq!(neutral.value, crate::gamma::ramp::NEUTRAL_TEMPERATURE);
        assert!(!neutral.relative);

        assert!(parse_value_adjustment(Property::Dim, "101%").is_err());
        assert!(parse_value_adjustment(Property::Temperature, "999K").is_err());
        assert!(parse_value_adjustment(Property::Temperature, "25001K").is_err());
    }

    #[test]
    fn reports_idle_status_and_pause_reasons_as_json() {
        let hub = Hub::new("none");
        hub.set_idle_profile("battery", true, 120, 20);
        hub.set_idled(true);
        hub.add_output("eDP-1", "backlight", Some("wayland"));
        hub.set_pause("eDP-1", true, true);

        let json = hub.snapshot().json();
        assert!(json.contains(
            "\"idle\":{\"power_source\":\"battery\",\"state\":\"idle\",\"enabled\":true,\"timeout\":120,\"brightness\":20}"
        ));
        assert!(json.contains("\"state\":\"idle+manual\",\"paused\":true,\"idle\":true"));
    }

    #[test]
    fn aligns_status_columns_for_long_output_names() {
        let hub = Hub::new("iio-sensor-proxy");
        hub.set_als("iio-sensor-proxy", 925);
        hub.set_idle_profile("battery", true, 120, 30);
        hub.set_idled(true);
        hub.add_output("DP-1", "ddc", Some("wayland"));
        hub.set_luma("DP-1", 16);
        hub.set_brightness("DP-1", 99);
        hub.set_pause("DP-1", false, true);
        hub.add_output("dell::kbd_backlight", "backlight", None);
        hub.set_brightness("dell::kbd_backlight", 0);

        assert_eq!(
            hub.snapshot().tables(),
            "ALS TYPE          VALUE\n\
             iio-sensor-proxy  925\n\
             \n\
             POWER SOURCE  IDLE STATE  ENABLED  TIMEOUT  BRIGHTNESS\n\
             battery       idle        yes      120s     30%\n\
             \n\
             OUTPUT               TYPE       CAPTURER  LUMA  BRIGHTNESS  DIM  TEMPERATURE  STATE\n\
             DP-1                 ddc        wayland   16%   99%         -    -            idle\n\
             dell::kbd_backlight  backlight  -         -     0%          -    -            active"
        );
    }
}
