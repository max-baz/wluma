use super::ramp;
use anyhow::{anyhow, Context, Result};
use std::ffi::CString;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::{AsFd, FromRawFd};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1;
use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_v1::ZwlrGammaControlV1;

pub enum Failure {
    Unavailable(anyhow::Error),
    Rejected(anyhow::Error),
}

pub struct Backend {
    queue: EventQueue<State>,
    state: State,
}

struct State {
    desired_output: String,
    manager: Option<ZwlrGammaControlManagerV1>,
    output: Option<WlOutput>,
    output_exact: bool,
    output_ambiguous: bool,
    control: Option<ZwlrGammaControlV1>,
    size: Option<u32>,
    failed: bool,
}

#[derive(Clone)]
struct OutputData {
    desired_output: String,
}

impl Backend {
    pub fn new(output_name: &str) -> std::result::Result<Self, Failure> {
        Self::connect(output_name).map_err(|error| match error.downcast_ref::<Rejected>() {
            Some(_) => Failure::Rejected(error),
            None => Failure::Unavailable(error),
        })
    }

    fn connect(output_name: &str) -> Result<Self> {
        let connection = Connection::connect_to_env().context("Unable to connect to Wayland")?;
        let display = connection.display();
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        let mut state = State {
            desired_output: output_name.to_string(),
            manager: None,
            output: None,
            output_exact: false,
            output_ambiguous: false,
            control: None,
            size: None,
            failed: false,
        };
        display.get_registry(&qh, ());
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        if state.output_ambiguous {
            return Err(anyhow!("Multiple Wayland outputs match '{output_name}'"));
        }
        let manager = state
            .manager
            .as_ref()
            .ok_or_else(|| anyhow!("wlr-gamma-control-unstable-v1 is unavailable"))?;
        let output = state
            .output
            .as_ref()
            .ok_or_else(|| anyhow!("Unable to match '{output_name}' to a Wayland output"))?;
        state.control = Some(manager.get_gamma_control(output, &qh, ()));
        queue.roundtrip(&mut state)?;
        if state.failed {
            return Err(Rejected(anyhow!("Gamma control was rejected for '{output_name}'")).into());
        }
        if state.size.unwrap_or(0) == 0 {
            return Err(anyhow!("Gamma control reported no LUT for '{output_name}'"));
        }
        log::debug!("Using wlr-gamma-control-unstable-v1 for '{output_name}'");
        Ok(Self { queue, state })
    }

    pub fn set(&mut self, dim: u64, temperature: u64) -> Result<()> {
        let ramps = ramp::linear(self.state.size.unwrap() as usize, dim, temperature);
        let mut file = memfd()?;
        for channel in ramps {
            for value in channel {
                file.write_all(&value.to_ne_bytes())?;
            }
        }
        file.seek(SeekFrom::Start(0))?;
        self.state
            .control
            .as_ref()
            .ok_or_else(|| anyhow!("Gamma control is unavailable"))?
            .set_gamma(file.as_fd());
        self.queue.roundtrip(&mut self.state)?;
        if self.state.failed {
            return Err(anyhow!("Gamma control failed"));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Rejected(anyhow::Error);

impl std::fmt::Display for Rejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for Rejected {}

fn memfd() -> Result<File> {
    let name = CString::new("wluma-gamma")?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _: &(),
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
                registry.bind::<WlOutput, _, _>(
                    name,
                    version.min(WlOutput::interface().version),
                    qh,
                    OutputData {
                        desired_output: state.desired_output.clone(),
                    },
                );
            } else if interface == ZwlrGammaControlManagerV1::interface().name {
                state.manager = Some(registry.bind::<ZwlrGammaControlManagerV1, _, _>(
                    name,
                    version.min(1),
                    qh,
                    (),
                ));
            }
        }
    }
}

impl Dispatch<WlOutput, OutputData> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        data: &OutputData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_output::Event;
        let value = match event {
            Event::Name { name } => Some((name, true)),
            Event::Description { description } => Some((description, false)),
            _ => None,
        };
        if let Some((value, name)) = value {
            let exact = name && value == data.desired_output;
            let matches = exact || value.contains(&data.desired_output);
            if matches {
                let same_output = state.output.as_ref() == Some(output);
                if state.output.is_none() || exact && !state.output_exact {
                    state.output = Some(output.clone());
                    state.output_exact = exact;
                    state.output_ambiguous = false;
                } else if !same_output && exact == state.output_exact {
                    state.output_ambiguous = true;
                }
            }
        }
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrGammaControlManagerV1,
        _: <ZwlrGammaControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrGammaControlV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrGammaControlV1,
        event: <ZwlrGammaControlV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols_wlr::gamma_control::v1::client::zwlr_gamma_control_v1::Event;
        match event {
            Event::GammaSize { size } => state.size = Some(size),
            Event::Failed => state.failed = true,
            _ => {}
        }
    }
}
