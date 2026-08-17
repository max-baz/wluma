use crate::config::PipewireProtocol;
use crate::frame::object::Object;
use crate::frame::vulkan::Vulkan;
use crate::predictor::Controller;
use anyhow::{anyhow, Result};
use drm_fourcc::DrmFourcc;
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use std::cell::RefCell;
use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod kwin;
mod mutter;
mod portal;

pub(super) type Portal = (dbus::arg::OwnedFd, dbus::blocking::Connection);
pub(super) type Source = (u32, Option<Portal>);

pub(super) struct Prepared {
    source: Source,
    pub protocol: PipewireProtocol,
}

const FRAME_RATE: u32 = 10;
const FRAME_INTERVAL: Duration = Duration::from_millis(1000 / FRAME_RATE as u64);

pub fn run(
    output_name: &str,
    protocol: PipewireProtocol,
    controller: Controller,
    vulkan_device: Option<&str>,
    active: Arc<AtomicBool>,
    status: crate::control::Hub,
) {
    match prepare(output_name, protocol) {
        Ok(prepared) => {
            let (_, result) = run_prepared(
                prepared,
                controller,
                vulkan_device,
                active,
                &status,
                output_name,
            );
            if let Err(error) = result {
                status.set_capturer(output_name, "failed");
                log::error!("Unable to capture PipeWire screen stream: {error:#}");
            }
        }
        Err(error) => {
            status.set_capturer(output_name, "failed");
            log::error!("Unable to create PipeWire screen stream: {error:#}");
        }
    }
}

pub(super) fn prepare(output_name: &str, protocol: PipewireProtocol) -> Result<Prepared> {
    let source = match protocol {
        PipewireProtocol::Any => return automatic_source(output_name),
        PipewireProtocol::Portal => portal_source(output_name),
        PipewireProtocol::Kwin => kwin::node(output_name).and_then(|(node, _)| {
            node.map(|node| (node, None))
                .ok_or_else(|| anyhow!("KWin PipeWire screencast protocol is not available"))
        }),
        PipewireProtocol::Mutter => kwin::connector(output_name)
            .and_then(|connector| mutter::node(&connector))
            .map(|node| (node, None)),
    }?;
    Ok(Prepared { source, protocol })
}

pub(super) fn run_prepared(
    prepared: Prepared,
    controller: Controller,
    vulkan_device: Option<&str>,
    active: Arc<AtomicBool>,
    status: &crate::control::Hub,
    output_name: &str,
) -> (Controller, Result<()>) {
    let Prepared { source, protocol } = prepared;
    let controller = Rc::new(RefCell::new(controller));
    let result = capture(
        source.0,
        source.1,
        Rc::clone(&controller),
        vulkan_device,
        active,
        || {
            status.set_capturer(
                output_name,
                protocol
                    .actual_name()
                    .expect("a prepared PipeWire source has a concrete protocol"),
            );
        },
    );
    let controller = Rc::into_inner(controller)
        .expect("PipeWire capture retained the predictor controller")
        .into_inner();
    (controller, result)
}

fn automatic_source(output_name: &str) -> Result<Prepared> {
    let connector = match kwin::node(output_name) {
        Ok((Some(node), _)) => {
            return Ok(Prepared {
                source: (node, None),
                protocol: PipewireProtocol::Kwin,
            })
        }
        Ok((None, connector)) => connector,
        Err(error) => {
            log::debug!("KWin PipeWire capture is unavailable: {error:#}");
            None
        }
    };
    match mutter::node(connector.as_deref().unwrap_or(output_name)) {
        Ok(node) => Ok(Prepared {
            source: (node, None),
            protocol: PipewireProtocol::Mutter,
        }),
        Err(error) => {
            log::debug!("Mutter PipeWire capture is unavailable: {error:#}");
            portal_source(output_name).map(|source| Prepared {
                source,
                protocol: PipewireProtocol::Portal,
            })
        }
    }
}

fn portal_source(output_name: &str) -> Result<(u32, Option<Portal>)> {
    let source = portal::source(output_name)?;
    Ok((source.node, Some((source.remote, source.connection))))
}

struct Data {
    controller: Rc<RefCell<Controller>>,
    format: spa::param::video::VideoInfoRaw,
    vulkan: Vulkan,
    last_frame_at: Option<Instant>,
}

fn capture(
    node: u32,
    portal: Option<Portal>,
    controller: Rc<RefCell<Controller>>,
    vulkan_device: Option<&str>,
    active: Arc<AtomicBool>,
    on_ready: impl FnOnce(),
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let timer_loop = mainloop.clone();
    let shutdown_active = active.clone();
    let shutdown_timer = mainloop.loop_().add_timer(move |_| {
        if !shutdown_active.load(Ordering::Relaxed) {
            timer_loop.quit();
        }
    });
    shutdown_timer.update_timer(
        Some(Duration::from_millis(100)),
        Some(Duration::from_millis(100)),
    );
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let (core, _portal_connection) = match portal {
        Some((remote, connection)) => {
            let remote = unsafe { OwnedFd::from_raw_fd(remote.into_raw_fd()) };
            (context.connect_fd_rc(remote, None)?, Some(connection))
        }
        None => (context.connect_rc(None)?, None),
    };
    let stream = pw::stream::StreamBox::new(
        &core,
        "wluma",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;
    let vulkan = Vulkan::new(vulkan_device)?;
    let mut modifiers = vulkan.importable_modifiers(DrmFourcc::Xrgb8888 as u32)?;
    let rgbx_modifiers = vulkan.importable_modifiers(DrmFourcc::Xbgr8888 as u32)?;
    modifiers.retain(|modifier| rgbx_modifiers.contains(modifier));
    if modifiers.is_empty() {
        return Err(anyhow!(
            "Vulkan cannot import any common single-plane PipeWire DMA-BUF modifier"
        ));
    }
    log::debug!("Advertising PipeWire DRM modifiers {modifiers:x?}");
    let data = Data {
        controller,
        format: Default::default(),
        vulkan,
        last_frame_at: None,
    };
    let stream_loop = mainloop.clone();
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(move |_, _, old, new| match new {
            pw::stream::StreamState::Error(error) => {
                log::error!("PipeWire stream failed: {error}");
                stream_loop.quit();
            }
            pw::stream::StreamState::Unconnected if old != pw::stream::StreamState::Unconnected => {
                log::debug!("PipeWire stream disconnected");
                stream_loop.quit();
            }
            _ => {}
        })
        .param_changed(|_, data, id, param| {
            if id == spa::param::ParamType::Format.as_raw() {
                if let Some(param) = param {
                    data.format
                        .parse(param)
                        .expect("Unable to parse PipeWire video format");
                    log::debug!(
                        "Negotiated PipeWire DMA-BUF format: SPA format={:?}, DRM format={:?}, size={:?}, framerate={:?}, max_framerate={:?}, modifier={:#018x}",
                        data.format.format(),
                        drm_format(data.format.format()),
                        data.format.size(),
                        data.format.framerate(),
                        data.format.max_framerate(),
                        data.format.modifier(),
                    );
                }
            }
        })
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.len() != 1 || datas[0].type_() != spa::buffer::DataType::DmaBuf {
                panic!("PipeWire compositor did not provide a single-plane DMA-BUF");
            }
            let data = &datas[0];
            let chunk = data.chunk();
            if chunk.size() == 0 || chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED) {
                return;
            }
            let now = Instant::now();
            if state
                .last_frame_at
                .is_some_and(|last_frame_at| now.duration_since(last_frame_at) < FRAME_INTERVAL)
            {
                return;
            }
            state.last_frame_at = Some(now);
            let stride = u32::try_from(chunk.stride())
                .ok()
                .filter(|stride| *stride > 0)
                .expect("PipeWire DMA-BUF has an invalid stride");
            if data.fd() < 0 {
                panic!("PipeWire DMA-BUF has an invalid file descriptor");
            }
            let fd = unsafe { BorrowedFd::borrow_raw(data.fd()) }
                .try_clone_to_owned()
                .expect("Unable to duplicate PipeWire DMA-BUF");
            let mut object = Object::new(
                state.format.size().width,
                state.format.size().height,
                1,
                drm_format(state.format.format())
                    .expect("PipeWire negotiated an unsupported video format")
                    as u32,
            );
            log::trace!(
                "Processing PipeWire DMA-BUF: DRM format={}, size={}x{}, modifier={:#018x}, offset={}, stride={}, object_size={}",
                object.format,
                object.width,
                object.height,
                state.format.modifier(),
                chunk.offset(),
                stride,
                data.as_raw().maxsize,
            );
            object.layout = Some((state.format.modifier(), chunk.offset(), stride));
            object.set_object(0, fd, data.as_raw().maxsize);
            let luma = state
                .vulkan
                .luma_percent_from_external_fd(&object)
                .expect("Unable to process PipeWire DMA-BUF with Vulkan");
            smol::block_on(state.controller.borrow_mut().adjust(luma));
        })
        .register()?;

    let format = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::RGBx,
        ),
        modifier_property(&modifiers),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction {
                num: FRAME_RATE,
                denom: 1
            },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction {
                num: FRAME_RATE,
                denom: 1
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoMaxFramerate,
            Fraction,
            pw::spa::utils::Fraction {
                num: FRAME_RATE,
                denom: 1
            }
        ),
    );
    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(format),
    )?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or_else(|| anyhow!("Invalid PipeWire format"))?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT,
        &mut params,
    )?;
    on_ready();
    mainloop.run();
    if active.load(Ordering::Relaxed) {
        Err(anyhow!("PipeWire screen stream stopped unexpectedly"))
    } else {
        Ok(())
    }
}

fn modifier_property(modifiers: &[u64]) -> spa::pod::Property {
    spa::pod::Property {
        key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
        flags: spa::pod::PropertyFlags::MANDATORY | spa::pod::PropertyFlags::DONT_FIXATE,
        value: spa::pod::Value::Choice(spa::pod::ChoiceValue::Long(spa::utils::Choice(
            spa::utils::ChoiceFlags::empty(),
            spa::utils::ChoiceEnum::Enum {
                default: modifiers[0] as i64,
                alternatives: modifiers.iter().map(|modifier| *modifier as i64).collect(),
            },
        ))),
    }
}

fn drm_format(format: spa::param::video::VideoFormat) -> Result<DrmFourcc> {
    match format {
        spa::param::video::VideoFormat::BGRx => Ok(DrmFourcc::Xrgb8888),
        spa::param::video::VideoFormat::RGBx => Ok(DrmFourcc::Xbgr8888),
        _ => Err(anyhow!("Unsupported PipeWire video format {format:?}")),
    }
}
