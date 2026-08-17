pub mod none;
pub mod pipewire;
pub mod wayland;

use anyhow::Result;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const PORTAL_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const PROBATION_FRAMES: usize = 3;
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
const DEGRADED_RETRY_INTERVAL: Duration = Duration::from_secs(30);

#[allow(clippy::large_enum_variant)]
pub enum Capturer {
    Auto,
    None(none::Capturer),
    Pipewire(crate::config::PipewireProtocol),
    Wayland(wayland::Capturer),
}

#[derive(Clone, Debug, PartialEq)]
enum Candidate {
    Wayland(crate::config::WaylandProtocol),
    Pipewire(crate::config::PipewireProtocol),
}

#[derive(Clone, Copy)]
enum CandidateFamily {
    Any,
    Wayland,
    Pipewire,
}

impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wayland(protocol) => write!(f, "Wayland {protocol}"),
            Self::Pipewire(protocol) => write!(f, "PipeWire {protocol}"),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Startup {
    deadline: Instant,
    required_frames: usize,
    discard_stale_inputs_before_first_frame: bool,
}

#[derive(Clone, Copy)]
struct RunPolicy {
    required_frames: usize,
    discard_stale_inputs_before_first_frame: bool,
}

struct Attempt {
    controller: crate::predictor::Controller,
    candidate: Candidate,
    frames: usize,
    result: Result<()>,
}

impl Candidate {
    fn startup_timeout(&self) -> Duration {
        match self {
            Self::Pipewire(
                crate::config::PipewireProtocol::Portal | crate::config::PipewireProtocol::Any,
            ) => PORTAL_STARTUP_TIMEOUT,
            _ => STARTUP_TIMEOUT,
        }
    }

    fn run(
        &self,
        output: &str,
        controller: crate::predictor::Controller,
        vulkan_device: Option<&str>,
        active: Arc<AtomicBool>,
        status: &crate::control::Hub,
        policy: RunPolicy,
    ) -> Attempt {
        let startup = Startup {
            deadline: Instant::now() + self.startup_timeout(),
            required_frames: policy.required_frames,
            discard_stale_inputs_before_first_frame: policy.discard_stale_inputs_before_first_frame,
        };
        let (controller, candidate, frames, result) = match self {
            Self::Wayland(protocol) => {
                let mut capturer = wayland::Capturer::new(protocol.clone());
                let (controller, frames, result) =
                    capturer.run(output, controller, vulkan_device, active, status, startup);
                (
                    controller,
                    Self::Wayland(capturer.protocol().clone()),
                    frames,
                    result,
                )
            }
            Self::Pipewire(protocol) => {
                match pipewire::prepare(output, protocol.clone(), startup.deadline, &active) {
                    Ok(prepared) => {
                        let candidate = Self::Pipewire(prepared.protocol.clone());
                        let (controller, frames, result) = pipewire::run_prepared(
                            prepared,
                            controller,
                            vulkan_device,
                            active,
                            status,
                            output,
                            startup,
                        );
                        (controller, candidate, frames, result)
                    }
                    Err(error) => (controller, self.clone(), 0, Err(error)),
                }
            }
        };
        Attempt {
            controller,
            candidate,
            frames,
            result,
        }
    }
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
                run_selecting(
                    CandidateFamily::Any,
                    output_name,
                    controller,
                    vulkan_device,
                    active,
                    status,
                )
                .await;
            }
            Capturer::None(mut capturer) => {
                status.set_capturer(output_name, "none");
                capturer.run(output_name, controller, active).await
            }
            Capturer::Pipewire(crate::config::PipewireProtocol::Any) => {
                run_selecting(
                    CandidateFamily::Pipewire,
                    output_name,
                    controller,
                    vulkan_device,
                    active,
                    status,
                )
                .await;
            }
            Capturer::Wayland(capturer)
                if capturer.protocol() == &crate::config::WaylandProtocol::Any =>
            {
                run_selecting(
                    CandidateFamily::Wayland,
                    output_name,
                    controller,
                    vulkan_device,
                    active,
                    status,
                )
                .await;
            }
            Capturer::Pipewire(protocol) => {
                run_explicit(
                    Candidate::Pipewire(protocol),
                    output_name,
                    controller,
                    vulkan_device,
                    active,
                    status,
                )
                .await;
            }
            Capturer::Wayland(capturer) => {
                run_explicit(
                    Candidate::Wayland(capturer.protocol().clone()),
                    output_name,
                    controller,
                    vulkan_device,
                    active,
                    status,
                )
                .await;
            }
        }
    }
}

async fn run_explicit(
    candidate: Candidate,
    output_name: &str,
    controller: crate::predictor::Controller,
    vulkan_device: Option<&str>,
    active: Arc<AtomicBool>,
    status: crate::control::Hub,
) {
    let output = output_name.to_string();
    let vulkan_device = vulkan_device.map(str::to_string);
    smol::unblock(move || {
        run_selected(
            candidate,
            &output,
            controller,
            vulkan_device.as_deref(),
            active,
            &status,
            false,
        );
    })
    .await;
}

async fn run_selecting(
    family: CandidateFamily,
    output_name: &str,
    controller: crate::predictor::Controller,
    vulkan_device: Option<&str>,
    active: Arc<AtomicBool>,
    status: crate::control::Hub,
) {
    let output = output_name.to_string();
    let vulkan_device = vulkan_device.map(str::to_string);
    smol::unblock(move || {
        select_and_run(
            family,
            &output,
            controller,
            vulkan_device.as_deref(),
            active,
            &status,
        );
    })
    .await;
}

fn select_and_run(
    family: CandidateFamily,
    output: &str,
    mut controller: crate::predictor::Controller,
    vulkan_device: Option<&str>,
    active: Arc<AtomicBool>,
    status: &crate::control::Hub,
) {
    let mut initial_probe = true;
    while active.load(Ordering::Relaxed) {
        let candidates = candidates(family, output, initial_probe);
        initial_probe = false;

        for candidate in candidates {
            log::debug!("Capturer selection trying {candidate} for '{output}'");
            let attempt = candidate.run(
                output,
                controller,
                vulkan_device,
                active.clone(),
                status,
                RunPolicy {
                    required_frames: PROBATION_FRAMES,
                    discard_stale_inputs_before_first_frame: false,
                },
            );
            controller = attempt.controller;
            match attempt.result {
                Ok(()) => return,
                Err(error) if probe_is_established(attempt.frames) => {
                    controller.discard_stale_inputs();
                    log::warn!(
                        "Established {} screen capture failed for '{output}': {error:#}",
                        attempt.candidate
                    );
                    run_selected(
                        attempt.candidate,
                        output,
                        controller,
                        vulkan_device,
                        active.clone(),
                        status,
                        true,
                    );
                    return;
                }
                Err(error) => {
                    controller.discard_stale_inputs();
                    status.set_capturer(output, "initializing");
                    status.clear_luma(output);
                    log::warn!(
                        "{candidate} screen capture did not become ready for '{output}': {error:#}"
                    );
                }
            }
        }

        status.set_capturer(output, "none");
        status.clear_luma(output);
        log::warn!(
            "No screen capturer is currently available for '{output}', using ALS only and retrying"
        );
        let retry_at = Instant::now() + DEGRADED_RETRY_INTERVAL;
        while active.load(Ordering::Relaxed) && Instant::now() < retry_at {
            smol::block_on(controller.adjust(0));
            if !wait_while_active(&active, Duration::from_millis(200)) {
                return;
            }
        }
        status.set_capturer(output, "initializing");
        status.clear_luma(output);
    }
}

fn candidates(
    family: CandidateFamily,
    output: &str,
    include_interactive_portal: bool,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    if matches!(family, CandidateFamily::Any | CandidateFamily::Wayland) {
        match wayland::Capturer::supported_protocols(Instant::now() + STARTUP_TIMEOUT) {
            Ok(protocols) => candidates.extend(protocols.into_iter().map(Candidate::Wayland)),
            Err(error) => {
                log::debug!("Capturer could not inspect Wayland protocols: {error:#}")
            }
        }
    }
    if matches!(family, CandidateFamily::Any | CandidateFamily::Pipewire) {
        candidates.extend(pipewire_candidates(
            include_interactive_portal || pipewire::portal_can_restore(output),
        ));
    }
    candidates
}

fn pipewire_candidates(include_portal: bool) -> Vec<Candidate> {
    let mut candidates = vec![
        Candidate::Pipewire(crate::config::PipewireProtocol::Kwin),
        Candidate::Pipewire(crate::config::PipewireProtocol::Mutter),
    ];
    if include_portal {
        candidates.push(Candidate::Pipewire(crate::config::PipewireProtocol::Portal));
    }
    candidates
}

fn probe_is_established(frames: usize) -> bool {
    frames >= PROBATION_FRAMES
}

/// Runs a chosen candidate until shutdown, reconnecting only that protocol.
fn run_selected(
    mut candidate: Candidate,
    output: &str,
    mut controller: crate::predictor::Controller,
    vulkan_device: Option<&str>,
    active: Arc<AtomicBool>,
    status: &crate::control::Hub,
    mut reconnecting: bool,
) {
    while active.load(Ordering::Relaxed) {
        let attempt = candidate.run(
            output,
            controller,
            vulkan_device,
            active.clone(),
            status,
            RunPolicy {
                required_frames: 1,
                discard_stale_inputs_before_first_frame: reconnecting,
            },
        );
        controller = attempt.controller;
        if attempt.frames > 0 {
            candidate = attempt.candidate;
        }
        match attempt.result {
            Ok(()) => return,
            Err(error) => {
                reconnecting = true;
                controller.discard_stale_inputs();
                log::warn!(
                    "{candidate} screen capture failed for '{output}': {error:#}; reconnecting"
                );
            }
        }

        status.set_capturer(output, "reconnecting");
        status.clear_luma(output);
        if !wait_while_active(&active, RECONNECT_INTERVAL) {
            return;
        }
    }
}

fn wait_while_active(active: &AtomicBool, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while active.load(Ordering::Relaxed) && Instant::now() < deadline {
        std::thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    active.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_requires_several_frames() {
        assert!(!probe_is_established(0));
        assert!(!probe_is_established(PROBATION_FRAMES - 1));
        assert!(probe_is_established(PROBATION_FRAMES));
    }

    #[test]
    fn pipewire_candidates_keep_portal_last_and_optional() {
        assert_eq!(
            pipewire_candidates(false),
            vec![
                Candidate::Pipewire(crate::config::PipewireProtocol::Kwin),
                Candidate::Pipewire(crate::config::PipewireProtocol::Mutter),
            ]
        );
        assert_eq!(
            pipewire_candidates(true).last(),
            Some(&Candidate::Pipewire(
                crate::config::PipewireProtocol::Portal
            ))
        );
    }

    #[test]
    fn portal_has_a_longer_startup_timeout() {
        assert_eq!(
            Candidate::Pipewire(crate::config::PipewireProtocol::Portal).startup_timeout(),
            PORTAL_STARTUP_TIMEOUT
        );
        assert_eq!(
            Candidate::Pipewire(crate::config::PipewireProtocol::Kwin).startup_timeout(),
            STARTUP_TIMEOUT
        );
    }
}
