use super::super::wayland::{match_action, output_match, MatchAction, OutputMatch};
use anyhow::{anyhow, Context, Result};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_callback::WlCallback;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::ZxdgOutputV1;
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::ZkdeScreencastStreamUnstableV1;
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_unstable_v1::{
    Pointer, ZkdeScreencastUnstableV1,
};

#[derive(Clone)]
struct OutputContext {
    desired_output: String,
    name: Arc<Mutex<Option<String>>>,
}

#[derive(Clone)]
struct XdgOutputContext {
    output: WlOutput,
    output_context: OutputContext,
}

#[derive(Default)]
struct State {
    manager: Option<ZkdeScreencastUnstableV1>,
    xdg_output_manager: Option<ZxdgOutputManagerV1>,
    outputs: Vec<(WlOutput, OutputContext, bool)>,
    output: Option<WlOutput>,
    output_name: Option<String>,
    output_match: Option<OutputMatch>,
    output_match_ambiguous: bool,
    node: Option<u32>,
    failure: Option<String>,
}

pub(super) struct Session {
    active: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

type Found = (Option<(u32, Session)>, Option<String>);

impl Drop for Session {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) fn node(output_name: &str, deadline: Instant, active: &AtomicBool) -> Result<Found> {
    find(output_name, true, deadline, active)
}

pub(super) fn connector(
    output_name: &str,
    deadline: Instant,
    active: &AtomicBool,
) -> Result<String> {
    let (_, connector) = find(output_name, false, deadline, active)?;
    connector.ok_or_else(|| anyhow!("Unable to determine the connector for '{output_name}'"))
}

fn find(
    output_name: &str,
    create_stream: bool,
    deadline: Instant,
    active: &AtomicBool,
) -> Result<Found> {
    let connection = Connection::connect_to_env().context("Unable to connect to Wayland")?;
    let display = connection.display();
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    display.get_registry(
        &qh,
        OutputContext {
            desired_output: output_name.to_string(),
            name: Arc::new(Mutex::new(None)),
        },
    );

    let mut state = State::default();
    timed_roundtrip(&connection, &mut queue, &mut state, deadline, active)?;
    if let Some(manager) = state.xdg_output_manager.as_ref() {
        for (output, output_context, needs_xdg_output) in &state.outputs {
            if *needs_xdg_output {
                manager.get_xdg_output(
                    output,
                    &qh,
                    XdgOutputContext {
                        output: output.clone(),
                        output_context: output_context.clone(),
                    },
                );
            }
        }
    }
    timed_roundtrip(&connection, &mut queue, &mut state, deadline, active)?;

    let output = state
        .output
        .as_ref()
        .ok_or_else(|| anyhow!("Unable to match '{output_name}' to a Wayland output"))?;
    if state.output_match_ambiguous {
        return Err(anyhow!("Multiple Wayland outputs match '{output_name}'"));
    }
    if !create_stream {
        return Ok((None, state.output_name));
    }
    let Some(manager) = state.manager.as_ref() else {
        return Ok((None, state.output_name));
    };
    manager.stream_output(output, Pointer::Hidden.into(), &qh, ());
    connection.flush()?;

    while state.node.is_none() && state.failure.is_none() {
        dispatch_until_readable(&connection, &mut queue, &mut state, deadline, active)?;
    }
    if let Some(error) = state.failure.take() {
        return Err(anyhow!(error));
    }

    let node = state.node.unwrap();
    let active = Arc::new(AtomicBool::new(true));
    let thread_active = active.clone();
    let thread = std::thread::spawn(move || {
        while thread_active.load(Ordering::Relaxed) {
            if queue.dispatch_pending(&mut state).is_err() || connection.flush().is_err() {
                break;
            }
            let Some(guard) = queue.prepare_read() else {
                continue;
            };
            let mut fd = libc::pollfd {
                fd: connection.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut fd, 1, 200) };
            if ready > 0 {
                if guard.read().is_err() {
                    break;
                }
            } else {
                drop(guard);
                if ready < 0 {
                    break;
                }
            }
        }
    });
    log::info!("Using KWin PipeWire stream node {node}");
    Ok((
        Some((
            node,
            Session {
                active,
                thread: Some(thread),
            },
        )),
        None,
    ))
}

fn timed_roundtrip(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    deadline: Instant,
    active: &AtomicBool,
) -> Result<()> {
    let done = Arc::new(AtomicBool::new(false));
    connection.display().sync(&queue.handle(), done.clone());
    while !done.load(Ordering::Relaxed) {
        dispatch_until_readable(connection, queue, state, deadline, active)?;
    }
    queue.dispatch_pending(state)?;
    Ok(())
}

fn dispatch_until_readable(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    deadline: Instant,
    active: &AtomicBool,
) -> Result<()> {
    if !active.load(Ordering::Relaxed) {
        return Err(anyhow!("Wayland output lookup was interrupted"));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(anyhow!("Timed out waiting for Wayland output lookup"));
    }
    queue.dispatch_pending(state)?;
    connection.flush()?;
    let Some(guard) = queue.prepare_read() else {
        return Ok(());
    };
    let timeout = remaining.min(Duration::from_millis(200));
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let mut fd = libc::pollfd {
        fd: connection.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
    if ready > 0 {
        guard.read()?;
    } else {
        drop(guard);
        if ready < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

impl Dispatch<WlCallback, Arc<AtomicBool>> for State {
    fn event(
        _state: &mut Self,
        _callback: &WlCallback,
        event: <WlCallback as Proxy>::Event,
        done: &Arc<AtomicBool>,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_callback::Event::Done { .. } = event {
            done.store(true, Ordering::Relaxed);
        }
    }
}

impl Dispatch<WlRegistry, OutputContext> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        context: &OutputContext,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_registry::Event;

        if let Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == WlOutput::interface().name {
                let output_context = OutputContext {
                    desired_output: context.desired_output.clone(),
                    name: Arc::new(Mutex::new(None)),
                };
                let version = version.min(4);
                let output =
                    registry.bind::<WlOutput, _, _>(name, version, qh, output_context.clone());
                state.outputs.push((output, output_context, version < 4));
            } else if interface == ZxdgOutputManagerV1::interface().name {
                state.xdg_output_manager =
                    Some(registry.bind::<ZxdgOutputManagerV1, _, _>(name, version.min(3), qh, ()));
            } else if interface == ZkdeScreencastUnstableV1::interface().name {
                state.manager = Some(registry.bind::<ZkdeScreencastUnstableV1, _, _>(
                    name,
                    version.min(4),
                    qh,
                    (),
                ));
            }
        }
    }
}

impl Dispatch<WlOutput, OutputContext> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        context: &OutputContext,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_output::Event;

        match event {
            Event::Name { name } => match_output(state, output, context, name, true),
            Event::Description { description } => {
                match_output(state, output, context, description, false)
            }
            _ => {}
        }
    }
}

fn match_output(
    state: &mut State,
    output: &WlOutput,
    context: &OutputContext,
    value: String,
    exact: bool,
) {
    if exact {
        *context.name.lock().unwrap() = Some(value.clone());
        if state.output.as_ref() == Some(output) {
            state.output_name = Some(value.clone());
        }
    }
    if let Some(candidate) = output_match(&value, &context.desired_output, exact) {
        let same_output = state.output.as_ref() == Some(output);
        match match_action(state.output_match, candidate, same_output) {
            MatchAction::Select => {
                state.output = Some(output.clone());
                state.output_name = context.name.lock().unwrap().clone();
                state.output_match = Some(candidate);
            }
            MatchAction::Replace => {
                state.output = Some(output.clone());
                state.output_name = context.name.lock().unwrap().clone();
                state.output_match = Some(candidate);
                state.output_match_ambiguous = false;
            }
            MatchAction::Ambiguous => state.output_match_ambiguous = true,
            MatchAction::Ignore => {}
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, XdgOutputContext> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as Proxy>::Event,
        context: &XdgOutputContext,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::Event;

        match event {
            Event::Name { name } => {
                match_output(state, &context.output, &context.output_context, name, true)
            }
            Event::Description { description } => match_output(
                state,
                &context.output,
                &context.output_context,
                description,
                false,
            ),
            _ => {}
        }
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZkdeScreencastUnstableV1,
        _: <ZkdeScreencastUnstableV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        event: <ZkdeScreencastStreamUnstableV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::Event;

        match event {
            Event::Created { node } => state.node = Some(node),
            Event::Failed { error } => state.failure = Some(error),
            Event::Closed => state.failure = Some("KDE closed the screen stream".to_string()),
            _ => {}
        }
    }
}
