pub mod none;
pub mod pipewire;
pub mod wayland;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[allow(clippy::large_enum_variant)]
pub enum Capturer {
    Auto,
    None(none::Capturer),
    Pipewire(crate::config::PipewireProtocol),
    Wayland(wayland::Capturer),
}

impl Capturer {
    pub async fn run(
        self,
        output_name: &str,
        controller: crate::predictor::Controller,
        vulkan_device: Option<&str>,
        active: Arc<AtomicBool>,
        status: crate::control::Hub,
    ) {
        match self {
            Capturer::Auto => {
                let output = output_name.to_string();
                let vulkan_device = vulkan_device.map(str::to_string);
                smol::unblock(move || {
                    let controller = match wayland::Capturer::supported_protocols() {
                        Ok(protocols) if !protocols.is_empty() => {
                            let mut controller = controller;
                            for protocol in protocols {
                                log::debug!(
                                    "Auto capturer trying Wayland {protocol} for '{output}'"
                                );
                                let (returned_controller, result) =
                                    wayland::Capturer::new(protocol.clone()).run(
                                        &output,
                                        controller,
                                        vulkan_device.as_deref(),
                                        active.clone(),
                                        &status,
                                    );
                                controller = returned_controller;
                                match result {
                                    Ok(()) => return,
                                    Err(error) => {
                                        status.set_capturer(&output, "initializing");
                                        log::warn!(
                                            "Wayland {protocol} screen capture failed for '{output}': {error:#}"
                                        );
                                    }
                                }
                            }
                            log::debug!(
                                "No usable Wayland screen capture protocol found for '{output}', trying PipeWire"
                            );
                            controller
                        }
                        Ok(_) => {
                            log::debug!("Auto capturer found no supported Wayland capture protocol");
                            controller
                        }
                        Err(error) => {
                            log::debug!("Auto capturer could not inspect Wayland protocols: {error:#}");
                            controller
                        }
                    };

                    match pipewire::prepare(&output, crate::config::PipewireProtocol::Any) {
                        Ok(prepared) => {
                            log::debug!(
                                "Auto capturer selected {} for '{output}'",
                                prepared.protocol
                            );
                            let (controller, result) = pipewire::run_prepared(
                                prepared,
                                controller,
                                vulkan_device.as_deref(),
                                active.clone(),
                                &status,
                                &output,
                            );
                            if let Err(error) = result {
                                status.set_capturer(&output, "none");
                                log::warn!(
                                    "PipeWire screen capture failed for '{output}', using ALS only: {error:#}"
                                );
                                smol::block_on(
                                    none::Capturer::default().run(&output, controller, active),
                                );
                            }
                        }
                        Err(error) => {
                            status.set_capturer(&output, "none");
                            log::warn!(
                                "No supported screen capture protocol found for '{output}', using ALS only: {error:#}"
                            );
                            smol::block_on(none::Capturer::default().run(&output, controller, active));
                        }
                    }
                })
                .await;
            }
            Capturer::None(mut c) => {
                status.set_capturer(output_name, "none");
                c.run(output_name, controller, active).await
            }
            Capturer::Pipewire(protocol) => {
                let output = output_name.to_string();
                let vulkan_device = vulkan_device.map(str::to_string);
                smol::unblock(move || {
                    pipewire::run(
                        &output,
                        protocol,
                        controller,
                        vulkan_device.as_deref(),
                        active,
                        status,
                    )
                })
                .await;
            }
            Capturer::Wayland(mut c) => {
                let output = output_name.to_string();
                let vulkan_device = vulkan_device.map(str::to_string);
                smol::unblock(move || {
                    let (_, result) = c.run(
                        &output,
                        controller,
                        vulkan_device.as_deref(),
                        active,
                        &status,
                    );
                    if let Err(error) = result {
                        status.set_capturer(&output, "failed");
                        log::error!("Wayland screen capture failed for '{output}': {error:#}");
                    }
                })
                .await;
            }
        }
    }
}
