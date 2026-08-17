use anyhow::{anyhow, Result};
use dbus::arg::{RefArg, Variant};
use dbus::blocking::Connection;
use dbus::message::MatchRule;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DESTINATION: &str = "org.gnome.Mutter.ScreenCast";
const PATH: &str = "/org/gnome/Mutter/ScreenCast";
const INTERFACE: &str = "org.gnome.Mutter.ScreenCast";
const SESSION_INTERFACE: &str = "org.gnome.Mutter.ScreenCast.Session";
const STREAM_INTERFACE: &str = "org.gnome.Mutter.ScreenCast.Stream";
const TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn node(
    output_name: &str,
    deadline: Instant,
    active: &AtomicBool,
) -> Result<(u32, Connection)> {
    if !active.load(Ordering::Relaxed) {
        return Err(anyhow!("Mutter screen capture setup was interrupted"));
    }
    let connection = Connection::new_session()?;
    let proxy = connection.with_proxy(DESTINATION, PATH, remaining(deadline)?);
    let properties = HashMap::<String, Variant<Box<dyn RefArg>>>::new();
    let (session_path,): (dbus::Path<'static>,) =
        proxy.method_call(INTERFACE, "CreateSession", (properties,))?;
    if !active.load(Ordering::Relaxed) {
        return Err(anyhow!("Mutter screen capture setup was interrupted"));
    }
    let session = connection.with_proxy(DESTINATION, session_path.clone(), remaining(deadline)?);
    let mut properties = HashMap::<String, Variant<Box<dyn RefArg>>>::new();
    properties.insert("cursor-mode".to_string(), Variant(Box::new(0_u32)));
    properties.insert("is-recording".to_string(), Variant(Box::new(true)));
    let (stream_path,): (dbus::Path<'static>,) = session.method_call(
        SESSION_INTERFACE,
        "RecordMonitor",
        (output_name, properties),
    )?;

    if !active.load(Ordering::Relaxed) {
        return Err(anyhow!("Mutter screen capture setup was interrupted"));
    }
    let node = Arc::new(Mutex::new(None));
    let signal_node = node.clone();
    let rule = MatchRule::new_signal(STREAM_INTERFACE, "PipeWireStreamAdded")
        .with_path(stream_path.clone());
    connection.add_match(rule, move |(id,): (u32,), _, _| {
        *signal_node.lock().unwrap() = Some(id);
        true
    })?;
    let session = connection.with_proxy(DESTINATION, session_path, remaining(deadline)?);
    let _: () = session.method_call(SESSION_INTERFACE, "Start", ())?;

    let deadline = deadline.min(Instant::now() + TIMEOUT);
    let node = loop {
        if let Some(node) = *node.lock().unwrap() {
            break node;
        }
        if !active.load(Ordering::Relaxed) {
            return Err(anyhow!("Mutter screen capture setup was interrupted"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!("Timed out waiting for Mutter's PipeWire stream"));
        }
        connection.process(remaining.min(Duration::from_millis(200)))?;
    };

    log::debug!("Using GNOME Mutter PipeWire stream node {node}");
    // Mutter owns the stream only for the lifetime of this D-Bus client. Return the
    // connection with the node so reconnects can cleanly drop the old session.
    Ok((node, connection))
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(anyhow!("Timed out setting up Mutter screen capture"))
    } else {
        Ok(remaining.min(TIMEOUT))
    }
}
