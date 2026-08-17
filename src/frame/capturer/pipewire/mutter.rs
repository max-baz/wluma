use anyhow::{anyhow, Result};
use dbus::arg::{RefArg, Variant};
use dbus::blocking::Connection;
use dbus::message::MatchRule;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DESTINATION: &str = "org.gnome.Mutter.ScreenCast";
const PATH: &str = "/org/gnome/Mutter/ScreenCast";
const INTERFACE: &str = "org.gnome.Mutter.ScreenCast";
const SESSION_INTERFACE: &str = "org.gnome.Mutter.ScreenCast.Session";
const STREAM_INTERFACE: &str = "org.gnome.Mutter.ScreenCast.Stream";
const TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn node(output_name: &str) -> Result<u32> {
    let connection = Connection::new_session()?;
    let proxy = connection.with_proxy(DESTINATION, PATH, TIMEOUT);
    let properties = HashMap::<String, Variant<Box<dyn RefArg>>>::new();
    let (session_path,): (dbus::Path<'static>,) =
        proxy.method_call(INTERFACE, "CreateSession", (properties,))?;
    let session = connection.with_proxy(DESTINATION, session_path, TIMEOUT);
    let mut properties = HashMap::<String, Variant<Box<dyn RefArg>>>::new();
    properties.insert("cursor-mode".to_string(), Variant(Box::new(0_u32)));
    properties.insert("is-recording".to_string(), Variant(Box::new(true)));
    let (stream_path,): (dbus::Path<'static>,) = session.method_call(
        SESSION_INTERFACE,
        "RecordMonitor",
        (output_name, properties),
    )?;

    let node = Arc::new(Mutex::new(None));
    let signal_node = node.clone();
    let rule = MatchRule::new_signal(STREAM_INTERFACE, "PipeWireStreamAdded")
        .with_path(stream_path.clone());
    connection.add_match(rule, move |(id,): (u32,), _, _| {
        *signal_node.lock().unwrap() = Some(id);
        true
    })?;
    let _: () = session.method_call(SESSION_INTERFACE, "Start", ())?;

    let deadline = Instant::now() + TIMEOUT;
    let node = loop {
        if let Some(node) = *node.lock().unwrap() {
            break node;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!("Timed out waiting for Mutter's PipeWire stream"));
        }
        connection.process(remaining)?;
    };

    std::thread::spawn(move || while connection.process(TIMEOUT).is_ok() {});
    log::debug!("Using GNOME Mutter PipeWire stream node {node}");
    Ok(node)
}
