use crate::config::WaylandProtocol;
use crate::frame::object::Object;
use crate::frame::vulkan::Vulkan;
use crate::predictor::Controller;
use anyhow::{anyhow, Context, Result};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_callback::WlCallback;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::Connection;
use wayland_client::Dispatch;
use wayland_client::EventQueue;
use wayland_client::Proxy;
use wayland_client::QueueHandle;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::Options;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::Flags;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use wayland_protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_frame_v1::ZwlrExportDmabufFrameV1;
use wayland_protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

const DELAY_SUCCESS: Duration = super::CAPTURE_INTERVAL;
const DELAY_FAILURE: Duration = Duration::from_millis(1000);

pub struct Capturer {
    protocol: WaylandProtocol,
    is_processing_frame: bool,
    vulkan: Option<Vulkan>,
    vulkan_device: Option<String>,
    drm_device: Option<(u32, u32)>,
    output: Option<WlOutput>,
    output_global_id: Option<u32>,
    output_match: Option<OutputMatch>,
    output_match_ambiguous: bool,
    pending_frame: Option<Object>,
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
    failure: Option<anyhow::Error>,
    successful_frames: usize,
    latest_luma: Option<u8>,
    last_adjustment: Option<Instant>,
    discard_stale_inputs_before_first_frame: bool,
    controller: Option<Controller>,
    // linux-dmabuf-v1
    dmabuf: Option<ZwpLinuxDmabufV1>,
    dmabuf_feedback: Option<ZwpLinuxDmabufFeedbackV1>,
    wl_buffer: Option<WlBuffer>,
    // ext-image-capture-source-v1
    img_capture_source_manager: Option<ExtOutputImageCaptureSourceManagerV1>,
    // ext-image-copy-capture-v1
    img_copy_capture_manager: Option<ExtImageCopyCaptureManagerV1>,
    img_copy_capture_session: Option<ExtImageCopyCaptureSessionV1>,
    // wlr-screencopy-unstable-v1
    screencopy_manager: Option<ZwlrScreencopyManagerV1>,
    // wlr-export-dmabuf-unstable-v1
    dmabuf_manager: Option<ZwlrExportDmabufManagerV1>,
}

#[derive(Clone)]
struct GlobalsContext {
    global_id: Option<u32>,
    desired_output: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum OutputMatch {
    Substring,
    Exact,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum MatchAction {
    Select,
    Replace,
    Ambiguous,
    Ignore,
}

impl Capturer {
    pub fn new(protocol: WaylandProtocol) -> Self {
        Self {
            protocol,
            is_processing_frame: false,
            vulkan: None,
            vulkan_device: None,
            drm_device: None,
            output: None,
            output_global_id: None,
            output_match: None,
            output_match_ambiguous: false,
            pending_frame: None,
            dmabuf_formats: Vec::new(),
            failure: None,
            successful_frames: 0,
            latest_luma: None,
            last_adjustment: None,
            discard_stale_inputs_before_first_frame: false,
            controller: None,
            // linux-dmabuf-v1
            dmabuf: None,
            dmabuf_feedback: None,
            wl_buffer: None,
            // ext-image-capture-source-v1
            img_capture_source_manager: None,
            // ext-image-copy-capture-v1
            img_copy_capture_manager: None,
            img_copy_capture_session: None,
            // wlr-screencopy-unstable-v1
            screencopy_manager: None,
            // wlr-export-dmabuf-unstable-v1
            dmabuf_manager: None,
        }
    }
}

impl Capturer {
    pub fn protocol(&self) -> &WaylandProtocol {
        &self.protocol
    }

    pub fn supported_protocols(deadline: Instant) -> Result<Vec<WaylandProtocol>> {
        let mut capturer = Self::new(WaylandProtocol::Any);
        let connection = Connection::connect_to_env().context("Unable to connect to Wayland")?;
        let display = connection.display();
        let mut event_queue = connection.new_event_queue();
        let qh = event_queue.handle();
        display.get_registry(
            &qh,
            GlobalsContext {
                global_id: None,
                desired_output: String::new(),
            },
        );
        timed_roundtrip(&connection, &mut event_queue, &mut capturer, deadline)
            .context("Unable to query Wayland protocols")?;
        let mut protocols = Vec::new();
        if capturer.img_copy_capture_manager.is_some()
            && capturer.img_capture_source_manager.is_some()
            && capturer.dmabuf.is_some()
        {
            protocols.push(WaylandProtocol::ExtImageCopyCaptureV1);
        }
        if capturer.screencopy_manager.is_some() && capturer.dmabuf.is_some() {
            protocols.push(WaylandProtocol::WlrScreencopyUnstableV1);
        }
        if capturer.dmabuf_manager.is_some() {
            protocols.push(WaylandProtocol::WlrExportDmabufUnstableV1);
        }
        Ok(protocols)
    }

    pub(super) fn run(
        &mut self,
        output_name: &str,
        controller: Controller,
        vulkan_device: Option<&str>,
        active: Arc<AtomicBool>,
        status: &crate::control::Hub,
        startup: super::Startup,
    ) -> (Controller, usize, Result<()>) {
        self.controller = Some(controller);
        let result = self.run_inner(output_name, vulkan_device, active, status, startup);
        (
            self.controller.take().unwrap(),
            self.successful_frames,
            result,
        )
    }

    fn run_inner(
        &mut self,
        output_name: &str,
        vulkan_device: Option<&str>,
        active: Arc<AtomicBool>,
        status: &crate::control::Hub,
        startup: super::Startup,
    ) -> Result<()> {
        self.vulkan_device = vulkan_device.map(str::to_string);
        self.discard_stale_inputs_before_first_frame =
            startup.discard_stale_inputs_before_first_frame;
        let connection =
            Connection::connect_to_env().context("Unable to connect to Wayland display")?;
        let display = connection.display();
        let mut event_queue = connection.new_event_queue();
        let qh = event_queue.handle();

        let ctx = GlobalsContext {
            global_id: None,
            desired_output: output_name.to_string(),
        };

        display.get_registry(&qh, ctx);

        // 1. process registry events
        timed_roundtrip(&connection, &mut event_queue, self, startup.deadline)
            .context("Unable to perform initial Wayland roundtrip")?;

        // 2. registry requested wl_output events, process those
        timed_roundtrip(&connection, &mut event_queue, self, startup.deadline)
            .context("Unable to perform second initial Wayland roundtrip")?;

        if self.output.is_none() {
            return Err(anyhow!(
                "Unable to match config '{output_name}' to any Wayland output"
            ));
        }
        if self.output_match_ambiguous {
            return Err(anyhow!(
                "Multiple Wayland outputs match config '{output_name}'"
            ));
        }

        let protocol_to_use = match self.protocol {
            WaylandProtocol::ExtImageCopyCaptureV1 => {
                if self.img_copy_capture_manager.is_none() {
                    return Err(anyhow!(
                        "Requested ext-image-copy-capture-v1 protocol is not available"
                    ));
                }
                if self.img_capture_source_manager.is_none() {
                    return Err(anyhow!("ext-image-copy-capture-v1 requires unavailable ext-image-capture-source-v1"));
                }
                if self.dmabuf.is_none() {
                    return Err(anyhow!(
                        "ext-image-copy-capture-v1 requires unavailable linux-dmabuf-v1"
                    ));
                }
                WaylandProtocol::ExtImageCopyCaptureV1
            }
            WaylandProtocol::WlrScreencopyUnstableV1 => {
                if self.screencopy_manager.is_none() {
                    return Err(anyhow!(
                        "Requested wlr-screencopy-unstable-v1 protocol is not available"
                    ));
                }
                if self.dmabuf.is_none() {
                    return Err(anyhow!(
                        "wlr-screencopy-unstable-v1 requires unavailable linux-dmabuf-v1"
                    ));
                }
                WaylandProtocol::WlrScreencopyUnstableV1
            }
            WaylandProtocol::WlrExportDmabufUnstableV1 => {
                if self.dmabuf_manager.is_none() {
                    return Err(anyhow!(
                        "Requested wlr-export-dmabuf-unstable-v1 protocol is not available"
                    ));
                }
                WaylandProtocol::WlrExportDmabufUnstableV1
            }
            WaylandProtocol::Any => {
                if self.img_copy_capture_manager.is_some()
                    && self.img_capture_source_manager.is_some()
                    && self.dmabuf.is_some()
                {
                    WaylandProtocol::ExtImageCopyCaptureV1
                } else if self.screencopy_manager.is_some() && self.dmabuf.is_some() {
                    WaylandProtocol::WlrScreencopyUnstableV1
                } else if self.dmabuf_manager.is_some() {
                    WaylandProtocol::WlrExportDmabufUnstableV1
                } else {
                    return Err(anyhow!(
                        "No supported Wayland screen capture protocol is available"
                    ));
                }
            }
        };
        self.protocol = protocol_to_use.clone();
        log::info!("Using {protocol_to_use} protocol to request frames");

        self.vulkan = Some(
            if let Some(path) = self.vulkan_device.as_deref() {
                Vulkan::new(Some(path))
            } else if let Some((major, minor)) = self.drm_device {
                Vulkan::new_for_drm_device(major, minor)
            } else {
                Vulkan::new(None)
            }
            .context("Unable to initialize Vulkan for Wayland capture")?,
        );
        status.set_capturer(
            output_name,
            protocol_to_use
                .actual_name()
                .expect("the selected Wayland protocol is concrete"),
        );

        while active.load(Ordering::Relaxed) {
            if self.successful_frames < startup.required_frames
                && Instant::now() >= startup.deadline
            {
                return Err(anyhow!(
                    "Wayland screen capture produced only {} of {} required startup frames",
                    self.successful_frames,
                    startup.required_frames,
                ));
            }
            if !self.is_processing_frame {
                if let Some(output) = self.output.as_ref() {
                    match protocol_to_use {
                        WaylandProtocol::ExtImageCopyCaptureV1 => {
                            if self.img_copy_capture_session.is_none() {
                                let capture_src = self
                                    .img_capture_source_manager
                                    .as_ref()
                                    .unwrap()
                                    .create_source(output, &event_queue.handle(), ());

                                self.img_copy_capture_session = Some(
                                    self.img_copy_capture_manager
                                        .as_ref()
                                        .unwrap()
                                        .create_session(
                                            &capture_src,
                                            Options::empty(),
                                            &event_queue.handle(),
                                            (),
                                        ),
                                );
                            }

                            if let Some(buffer) = self.wl_buffer.as_ref() {
                                let frame = self
                                    .img_copy_capture_session
                                    .as_ref()
                                    .unwrap()
                                    .create_frame(&event_queue.handle(), ());
                                let pending_frame = self.pending_frame.as_ref().unwrap();
                                frame.attach_buffer(buffer);
                                frame.damage_buffer(
                                    0,
                                    0,
                                    pending_frame.width as i32,
                                    pending_frame.height as i32,
                                );
                                frame.capture();

                                self.frame_requested();
                            }
                        }
                        WaylandProtocol::WlrScreencopyUnstableV1 => {
                            self.screencopy_manager.as_ref().unwrap().capture_output(
                                0,
                                output,
                                &event_queue.handle(),
                                (),
                            );
                            self.frame_requested();
                        }
                        WaylandProtocol::WlrExportDmabufUnstableV1 => {
                            self.dmabuf_manager.as_ref().unwrap().capture_output(
                                0,
                                output,
                                &event_queue.handle(),
                                (),
                            );
                            self.frame_requested();
                        }
                        WaylandProtocol::Any => unreachable!(),
                    }
                }
            }

            event_queue
                .dispatch_pending(self)
                .context("Error dispatching Wayland events")?;
            if let Some(error) = self.failure.take() {
                return Err(error);
            }
            self.process_prediction_tick();
            connection
                .flush()
                .context("Error flushing Wayland requests")?;
            if let Some(guard) = event_queue.prepare_read() {
                let mut fd = libc::pollfd {
                    fd: connection.as_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut fd, 1, 200) };
                if ready < 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("Error polling Wayland display");
                }
                if ready > 0 {
                    guard.read().context("Error reading Wayland events")?;
                }
            }
        }

        Ok(())
    }

    fn frame_requested(&mut self) {
        self.is_processing_frame = true;
    }

    fn process_luma(&mut self, luma: u8) {
        if self.discard_stale_inputs_before_first_frame {
            self.controller.as_mut().unwrap().discard_stale_inputs();
            self.discard_stale_inputs_before_first_frame = false;
        }
        self.latest_luma = Some(luma);
        self.successful_frames += 1;
        self.adjust_prediction(luma, Instant::now());
    }

    fn process_prediction_tick(&mut self) {
        let now = Instant::now();
        if super::prediction_due(self.last_adjustment, now) {
            if let Some(luma) = self.latest_luma {
                self.adjust_prediction(luma, now);
            }
        }
    }

    fn adjust_prediction(&mut self, luma: u8, now: Instant) {
        self.last_adjustment = Some(now);
        // TODO: replace with await
        smol::block_on(self.controller.as_mut().unwrap().adjust(luma));
    }
}

fn timed_roundtrip<State>(
    connection: &Connection,
    event_queue: &mut EventQueue<State>,
    state: &mut State,
    deadline: Instant,
) -> Result<()>
where
    State: Dispatch<WlCallback, Arc<AtomicBool>> + 'static,
{
    let done = Arc::new(AtomicBool::new(false));
    connection
        .display()
        .sync(&event_queue.handle(), done.clone());

    while !done.load(Ordering::Relaxed) {
        if Instant::now() >= deadline {
            return Err(anyhow!("Timed out waiting for the Wayland compositor"));
        }
        event_queue.dispatch_pending(state)?;
        connection.flush()?;
        let Some(guard) = event_queue.prepare_read() else {
            continue;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
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
    }
    event_queue.dispatch_pending(state)?;
    Ok(())
}

impl Dispatch<WlCallback, Arc<AtomicBool>> for Capturer {
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

// ==== Globals ====

pub(super) fn output_match(value: &str, desired_output: &str, exact: bool) -> Option<OutputMatch> {
    if desired_output.is_empty() || !value.contains(desired_output) {
        None
    } else if exact && value == desired_output {
        Some(OutputMatch::Exact)
    } else {
        Some(OutputMatch::Substring)
    }
}

pub(super) fn match_action(
    current: Option<OutputMatch>,
    candidate: OutputMatch,
    same_output: bool,
) -> MatchAction {
    match current {
        None => MatchAction::Select,
        Some(current) if candidate > current => MatchAction::Replace,
        Some(current) if candidate == current && !same_output => MatchAction::Ambiguous,
        _ => MatchAction::Ignore,
    }
}

impl Dispatch<WlOutput, GlobalsContext> for Capturer {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        ctx: &GlobalsContext,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_output::Event;

        let candidate = match event {
            Event::Name { name } => {
                output_match(&name, &ctx.desired_output, true).map(|quality| (name, quality))
            }
            Event::Description { description } => {
                output_match(&description, &ctx.desired_output, false)
                    .map(|quality| (description, quality))
            }
            _ => None,
        };

        if let Some((value, quality)) = candidate {
            let same_output = state.output.as_ref() == Some(output);
            match match_action(state.output_match, quality, same_output) {
                MatchAction::Select => {
                    log::debug!(
                        "Using output '{}' for config '{}'",
                        value,
                        ctx.desired_output,
                    );
                    state.output = Some(output.clone());
                    state.output_global_id = ctx.global_id;
                    state.output_match = Some(quality);
                }
                MatchAction::Replace => {
                    log::debug!(
                        "Using output '{}' for config '{}' instead of a less specific match",
                        value,
                        ctx.desired_output,
                    );
                    state.output = Some(output.clone());
                    state.output_global_id = ctx.global_id;
                    state.output_match = Some(quality);
                    state.output_match_ambiguous = false;
                }
                MatchAction::Ambiguous => state.output_match_ambiguous = true,
                MatchAction::Ignore => {}
            }
        }
    }
}

impl Dispatch<WlRegistry, GlobalsContext> for Capturer {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        ctx: &GlobalsContext,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_registry::Event;

        match event {
            Event::Global {
                name,
                interface,
                version,
            } => {
                match &interface[..] {
                    _ if interface == WlOutput::interface().name => {
                        registry.bind::<WlOutput, _, _>(
                            name,
                            version,
                            qh,
                            GlobalsContext {
                                global_id: Some(name),
                                desired_output: ctx.desired_output.clone(),
                            },
                        );
                    }
                    _ if interface == ZwlrExportDmabufManagerV1::interface().name => {
                        log::debug!("Detected support for wlr-export-dmabuf-unstable-v1 protocol");
                        state.dmabuf_manager = Some(
                            registry.bind::<ZwlrExportDmabufManagerV1, _, _>(name, version, qh, ()),
                        );
                    }
                    _ if interface == ZwpLinuxDmabufV1::interface().name => {
                        log::debug!("Detected support for linux-dmabuf-v1 protocol");
                        let dmabuf = registry.bind::<ZwpLinuxDmabufV1, _, _>(name, version, qh, ());
                        if dmabuf.version() >= 4 {
                            state.dmabuf_feedback = Some(dmabuf.get_default_feedback(qh, ()));
                        }
                        state.dmabuf = Some(dmabuf);
                    }
                    _ if interface == ZwlrScreencopyManagerV1::interface().name => {
                        log::debug!("Detected support for wlr-screencopy-unstable-v1 protocol");
                        state.screencopy_manager = Some(
                            registry.bind::<ZwlrScreencopyManagerV1, _, _>(name, version, qh, ()),
                        );
                    }
                    _ if interface == ExtOutputImageCaptureSourceManagerV1::interface().name => {
                        log::debug!("Detected support for ext-image-capture-source-v1 protocol");
                        state.img_capture_source_manager =
                            Some(registry.bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(
                                name,
                                version,
                                qh,
                                (),
                            ));
                    }
                    _ if interface == ExtImageCopyCaptureManagerV1::interface().name => {
                        log::debug!("Detected support for ext-image-copy-capture-v1 protocol");
                        state.img_copy_capture_manager =
                            Some(registry.bind::<ExtImageCopyCaptureManagerV1, _, _>(
                                name,
                                version,
                                qh,
                                (),
                            ));
                    }
                    _ => {}
                };
            }

            Event::GlobalRemove { name } if Some(name) == state.output_global_id => {
                log::debug!("Disconnected screen {}", ctx.desired_output);
                state.output = None;
                state.output_global_id = None;
                state.output_match = None;
                state.output_match_ambiguous = false;
            }
            _ => {}
        }
    }
}

// ==== wlr-export-dmabuf-unstable-v1 protocol ====

impl Dispatch<ZwlrExportDmabufManagerV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ZwlrExportDmabufManagerV1,
        _: <ZwlrExportDmabufManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrExportDmabufFrameV1, ()> for Capturer {
    fn event(
        state: &mut Self,
        frame: &ZwlrExportDmabufFrameV1,
        event: <ZwlrExportDmabufFrameV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_frame_v1::Event;

        match event {
            Event::Frame {
                width,
                height,
                num_objects,
                format,
                mod_high,
                mod_low,
                ..
            } => {
                let modifier = ((mod_high as u64) << 32) | mod_low as u64;
                log::trace!(
                    "wlr-export-dmabuf frame: DRM format={format}, size={width}x{height}, objects={num_objects}, modifier={modifier:#018x}"
                );
                if num_objects > 1 {
                    log::error!(
                        "The compositor sent a multi-object DMA-BUF, which wluma cannot import. Set WLR_DRM_NO_MODIFIERS=1 before launching the compositor"
                    );
                }
                let mut pending_frame = Object::new(width, height, num_objects, format);
                pending_frame.layout = Some((modifier, 0, 0));
                state.pending_frame = Some(pending_frame);
            }

            Event::Object {
                index,
                fd,
                size,
                offset,
                stride,
                ..
            } => {
                log::trace!(
                    "wlr-export-dmabuf object: index={index}, size={size}, offset={offset}, stride={stride}"
                );
                let pending_frame = state.pending_frame.as_mut().unwrap();
                pending_frame.layout = Some((pending_frame.layout.unwrap().0, offset, stride));
                pending_frame.set_object(index, fd, size);
            }

            Event::Ready { .. } => {
                let result = state
                    .vulkan
                    .as_mut()
                    .unwrap()
                    .luma_percent_from_external_fd(&state.pending_frame.take().unwrap());
                let luma = match result {
                    Ok(luma) => luma,
                    Err(error) => {
                        state.failure = Some(error.context("Unable to process Wayland DMA-BUF"));
                        frame.destroy();
                        return;
                    }
                };

                state.process_luma(luma);

                frame.destroy();

                thread::sleep(DELAY_SUCCESS);
                state.is_processing_frame = false;
            }

            Event::Cancel { reason } => {
                log::debug!("Frame was cancelled, reason: {reason:?}");
                state.pending_frame.take();
                frame.destroy();

                thread::sleep(DELAY_FAILURE);
                state.is_processing_frame = false;
            }

            _ => unreachable!(),
        }
    }
}

// ==== linux-dmabuf-v1 protocol ====

fn parse_drm_device(bytes: &[u8]) -> Option<(u32, u32)> {
    let bytes: [u8; std::mem::size_of::<libc::dev_t>()] = bytes.try_into().ok()?;
    let device = libc::dev_t::from_ne_bytes(bytes);
    Some((libc::major(device), libc::minor(device)))
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ZwpLinuxDmabufV1,
        event: <ZwpLinuxDmabufV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::Event;

        match event {
            Event::Format { format } => {
                log::debug!("linux-dmabuf-v1 advertised DRM format={format}");
            }
            Event::Modifier {
                format,
                modifier_hi,
                modifier_lo,
            } => {
                let modifier = ((modifier_hi as u64) << 32) | modifier_lo as u64;
                log::debug!(
                    "linux-dmabuf-v1 advertised DRM format={format}, modifier={modifier:#018x}"
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for Capturer {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufFeedbackV1,
        event: <ZwpLinuxDmabufFeedbackV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::Event;

        if let Event::MainDevice { device } = event {
            let Some((major, minor)) = parse_drm_device(&device) else {
                state.failure = Some(anyhow!("Compositor sent an invalid DMA-BUF main device"));
                return;
            };
            log::debug!("linux-dmabuf-v1 main device={major}:{minor}");
            state.drm_device = Some((major, minor));
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        _: <ZwpLinuxBufferParamsV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        _: <WlBuffer as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ==== wlr-screencopy-unstable-v1 protocol ====

impl Dispatch<ZwlrScreencopyManagerV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for Capturer {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::Event;

        match event {
            Event::LinuxDmabuf {
                width,
                height,
                format,
            } => {
                if let Some(pending_frame) = state.pending_frame.as_ref() {
                    if pending_frame.width != width
                        || pending_frame.height != height
                        || pending_frame.format != format
                    {
                        if let Some(buffer) = state.wl_buffer.take() {
                            buffer.destroy()
                        }
                    }
                }

                if state.wl_buffer.is_none() {
                    log::debug!(
                        "wlr-screencopy DMA-BUF constraints: DRM format={format}, size={width}x{height}, using explicit linear modifier"
                    );
                    let pending_frame = Object::new(width, height, 1, format);
                    let dmabuf_params = state.dmabuf.as_ref().unwrap().create_params(qh, ());
                    let allowed_modifiers = [0];
                    let result = state
                        .vulkan
                        .as_mut()
                        .unwrap()
                        .init_exportable_frame_image(&pending_frame, &allowed_modifiers);
                    let (fd, offset, stride, modifier) = match result {
                        Ok(frame) => frame,
                        Err(error) => {
                            state.failure =
                                Some(error.context(
                                    "Vulkan cannot export the compositor-requested DMA-BUF",
                                ));
                            frame.destroy();
                            return;
                        }
                    };

                    let fd = unsafe { BorrowedFd::borrow_raw(fd) };

                    dmabuf_params.add(
                        fd,
                        0,
                        offset,
                        stride,
                        (modifier >> 32) as u32,
                        (modifier & 0xFFFFFFFF) as u32,
                    );

                    let wl_buffer = dmabuf_params.create_immed(
                        width as i32,
                        height as i32,
                        format,
                        Flags::empty(),
                        qh,
                        (),
                    );

                    dmabuf_params.destroy();
                    state.wl_buffer = Some(wl_buffer);
                    state.pending_frame = Some(pending_frame);
                }

                let buffer = state.wl_buffer.as_ref().unwrap();
                if frame.version() >= 2 {
                    frame.copy_with_damage(buffer);
                } else {
                    frame.copy(buffer);
                }
            }

            Event::Ready { .. } => {
                let result = state
                    .vulkan
                    .as_mut()
                    .unwrap()
                    .luma_percent_from_internal_fd();
                let luma = match result {
                    Ok(luma) => luma,
                    Err(error) => {
                        state.failure = Some(error.context("Unable to process Wayland DMA-BUF"));
                        frame.destroy();
                        return;
                    }
                };

                state.process_luma(luma);

                frame.destroy();

                thread::sleep(DELAY_SUCCESS);
                state.is_processing_frame = false;
            }

            Event::Failed => {
                log::debug!("Frame copy failed");
                frame.destroy();

                if let Some(buffer) = state.wl_buffer.take() {
                    buffer.destroy()
                }

                thread::sleep(DELAY_FAILURE);
                state.is_processing_frame = false;
            }

            _ => {}
        }
    }
}

// ==== ext-image-capture-source-v1 protocol ====

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ExtOutputImageCaptureSourceManagerV1,
        _: <ExtOutputImageCaptureSourceManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ExtImageCaptureSourceV1,
        _: <ExtImageCaptureSourceV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ==== ext-image-copy-capture-v1 protocol ====

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for Capturer {
    fn event(
        _: &mut Self,
        _: &ExtImageCopyCaptureManagerV1,
        _: <ExtImageCopyCaptureManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for Capturer {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: <ExtImageCopyCaptureSessionV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::Event;

        match event {
            Event::BufferSize { width, height } => {
                log::debug!("ext-image-copy-capture DMA-BUF size constraint: {width}x{height}");
                state.pending_frame = Some(Object::new(width, height, 1, 0));
            }

            Event::DmabufDevice { device } => {
                let Some((major, minor)) = parse_drm_device(&device) else {
                    state.failure = Some(anyhow!(
                        "Compositor sent an invalid DMA-BUF allocation device"
                    ));
                    return;
                };
                log::debug!("ext-image-copy-capture DMA-BUF allocation device={major}:{minor}");
                let device_changed = state.drm_device != Some((major, minor));
                state.drm_device = Some((major, minor));
                if state.vulkan_device.is_none() && device_changed {
                    match Vulkan::new_for_drm_device(major, minor) {
                        Ok(vulkan) => state.vulkan = Some(vulkan),
                        Err(error) => {
                            state.failure = Some(error.context(
                                "Unable to initialize Vulkan on the compositor's DMA-BUF device",
                            ));
                        }
                    }
                }
            }

            Event::DmabufFormat { format, modifiers } => {
                let modifiers: Vec<_> = modifiers
                    .chunks_exact(8)
                    .map(|bytes| u64::from_ne_bytes(bytes.try_into().unwrap()))
                    .collect();
                log::debug!(
                    "ext-image-copy-capture DMA-BUF format constraint: DRM format={format}, modifiers={modifiers:x?}"
                );
                state.dmabuf_formats.push((format, modifiers));
            }

            Event::Done => {
                if state.failure.is_some() {
                    return;
                }
                if let Some(buffer) = state.wl_buffer.take() {
                    buffer.destroy()
                }

                let dimensions = {
                    let pending_frame = state.pending_frame.as_ref().unwrap();
                    (pending_frame.width, pending_frame.height)
                };
                let selection = state.dmabuf_formats.iter().find_map(|(format, offered)| {
                    let supported = state
                        .vulkan
                        .as_ref()
                        .unwrap()
                        .exportable_modifiers(*format)
                        .ok()?;
                    let modifiers: Vec<_> = offered
                        .iter()
                        .copied()
                        .filter(|modifier| supported.contains(modifier))
                        .collect();
                    (!modifiers.is_empty()).then_some((*format, modifiers))
                });
                state.dmabuf_formats.clear();
                let Some((format, modifiers)) = selection else {
                    state.failure = Some(anyhow::anyhow!(
                        "No compositor-provided DMA-BUF format and modifier can be exported by Vulkan"
                    ));
                    return;
                };
                log::debug!(
                    "ext-image-copy-capture selected DMA-BUF constraints: DRM format={format}, size={}x{}, modifiers={modifiers:x?}",
                    dimensions.0,
                    dimensions.1,
                );
                state.pending_frame = Some(Object::new(dimensions.0, dimensions.1, 1, format));
                let pending_frame = state.pending_frame.as_ref().unwrap();

                let dmabuf_params = state.dmabuf.as_ref().unwrap().create_params(qh, ());
                let result = state
                    .vulkan
                    .as_mut()
                    .unwrap()
                    .init_exportable_frame_image(pending_frame, &modifiers);
                let (fd, offset, stride, modifier) = match result {
                    Ok(frame) => frame,
                    Err(error) => {
                        state.failure = Some(
                            error.context("Vulkan cannot export the compositor-requested DMA-BUF"),
                        );
                        return;
                    }
                };

                let fd = unsafe { BorrowedFd::borrow_raw(fd) };

                dmabuf_params.add(
                    fd,
                    0,
                    offset,
                    stride,
                    (modifier >> 32) as u32,
                    (modifier & 0xFFFFFFFF) as u32,
                );

                let wl_buffer = dmabuf_params.create_immed(
                    pending_frame.width as i32,
                    pending_frame.height as i32,
                    pending_frame.format,
                    Flags::empty(),
                    qh,
                    (),
                );

                dmabuf_params.destroy();

                state.wl_buffer = Some(wl_buffer);
            }

            Event::Stopped => {
                log::debug!("Image copy session stopped");
                state.img_copy_capture_session.take().unwrap().destroy();
                if let Some(buffer) = state.wl_buffer.take() {
                    buffer.destroy()
                }

                thread::sleep(DELAY_FAILURE);
                state.is_processing_frame = false;
            }

            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for Capturer {
    fn event(
        state: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: <ExtImageCopyCaptureFrameV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::Event;

        match event {
            Event::Ready => {
                let result = state
                    .vulkan
                    .as_mut()
                    .unwrap()
                    .luma_percent_from_internal_fd();
                let luma = match result {
                    Ok(luma) => luma,
                    Err(error) => {
                        state.failure = Some(error.context("Unable to process Wayland DMA-BUF"));
                        frame.destroy();
                        return;
                    }
                };

                state.process_luma(luma);

                frame.destroy();

                thread::sleep(DELAY_SUCCESS);
                state.is_processing_frame = false;
            }

            Event::Failed { reason } => {
                log::debug!("Frame copy failed, reason: {reason:?}");
                frame.destroy();

                thread::sleep(DELAY_FAILURE);
                state.is_processing_frame = false;
            }

            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{match_action, output_match, parse_drm_device, MatchAction, OutputMatch};
    use std::time::{Duration, Instant};

    #[test]
    fn prediction_runs_at_fixed_interval() {
        let now = Instant::now();
        assert!(super::super::prediction_due(None, now));
        assert!(!super::super::prediction_due(
            Some(now),
            now + super::super::PREDICTION_INTERVAL - Duration::from_nanos(1)
        ));
        assert!(super::super::prediction_due(
            Some(now),
            now + super::super::PREDICTION_INTERVAL
        ));
    }

    #[test]
    fn parses_drm_device() {
        let device = libc::makedev(226, 128);
        assert_eq!(parse_drm_device(&device.to_ne_bytes()), Some((226, 128)));
    }

    #[test]
    fn ranks_exact_names_above_substrings() {
        assert_eq!(output_match("DP-1", "DP-1", true), Some(OutputMatch::Exact));
        assert_eq!(
            output_match("eDP-1", "DP-1", true),
            Some(OutputMatch::Substring)
        );
        assert_eq!(
            output_match("BNQ BenQ PD3225U", "PD3225", false),
            Some(OutputMatch::Substring)
        );
    }

    #[test]
    fn descriptions_are_substring_matches() {
        assert_eq!(
            output_match("DP-1", "DP-1", false),
            Some(OutputMatch::Substring)
        );
    }

    #[test]
    fn does_not_match_an_empty_output_name() {
        assert_eq!(output_match("Some display (DP-1)", "", false), None);
    }

    #[test]
    fn ignores_a_later_edp_substring_after_an_exact_dp_match() {
        assert_eq!(
            match_action(Some(OutputMatch::Exact), OutputMatch::Substring, false),
            MatchAction::Ignore
        );
    }

    #[test]
    fn replaces_a_substring_with_a_later_exact_match() {
        assert_eq!(
            match_action(Some(OutputMatch::Substring), OutputMatch::Exact, false),
            MatchAction::Replace
        );
    }

    #[test]
    fn detects_equally_specific_matches_from_different_outputs() {
        assert_eq!(
            match_action(Some(OutputMatch::Substring), OutputMatch::Substring, false),
            MatchAction::Ambiguous
        );
        assert_eq!(
            match_action(Some(OutputMatch::Substring), OutputMatch::Substring, true),
            MatchAction::Ignore
        );
    }
}
