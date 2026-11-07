//! DRM/KMS outputs and libinput devices of a logind/seatd session.
//!
//! Single GPU: everything is rendered with OpenGL ES on the primary GPU and
//! shown on its connectors. Hotplugging connectors, VT switching and mode
//! changes through wlr-output-management are supported; multi-GPU setups
//! and DRM leasing are not.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::input::InputEvent;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ImportDma, ImportMemWl};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{self, UdevBackend, UdevEvent};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::drm::control::{connector, crtc, Device as ControlDevice, ModeTypeFlags};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::DeviceFd;
use smithay::wayland::dmabuf::DmabufState;

use super::Backend;
use crate::compositor::handlers::DmabufGlobal;
use crate::compositor::render::{CanoeElement, CLEAR_COLOR};
use crate::compositor::State;

type CanoeDrmOutput =
    DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

/// What a connector shows.
struct Surface {
    output: Output,
    connector: connector::Handle,
    drm_output: CanoeDrmOutput,
    /// The connector's modes, for mode changes.
    modes: Vec<smithay::reexports::drm::control::Mode>,
    enabled: bool,
}

pub struct UdevData {
    session: LibSeatSession,
    renderer: GlesRenderer,
    manager: DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        (),
        DrmDeviceFd,
    >,
    surfaces: HashMap<crtc::Handle, Surface>,
    active: bool,
}

impl UdevData {
    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        self.renderer.import_dmabuf(dmabuf, None).is_ok()
    }

    pub fn switch_vt(&mut self, vt: i32) {
        if let Err(err) = self.session.change_vt(vt) {
            eprintln!("canoe: switching to VT {vt} failed: {err}");
        }
    }

    pub fn renderer(&mut self) -> Option<&mut GlesRenderer> {
        Some(&mut self.renderer)
    }

    pub fn can_set_mode(&self, output: &Output, mode: Mode) -> bool {
        self.surfaces
            .values()
            .find(|s| &s.output == output)
            .is_some_and(|s| s.modes.iter().any(|m| Mode::from(*m) == mode))
    }

    fn crtc_for(&self, output: &Output) -> Option<crtc::Handle> {
        self.surfaces
            .iter()
            .find(|(_, s)| &s.output == output)
            .map(|(crtc, _)| *crtc)
    }
}

/// Start a session on this seat's monitors and input devices.
pub fn init(state: &mut State) -> Result<(), Box<dyn std::error::Error>> {
    let (session, notifier) = LibSeatSession::new()
        .map_err(|err| format!("could not start a session (is seatd or logind running?): {err}"))?;
    let seat = session.seat();

    // The primary GPU.
    let path = udev::primary_gpu(&seat)?
        .or_else(|| udev::all_gpus(&seat).ok()?.into_iter().next())
        .ok_or("no GPU found")?;
    let node = DrmNode::from_path(&path)?;
    let primary = node
        .node_with_type(NodeType::Primary)
        .and_then(Result::ok)
        .unwrap_or(node);
    let device_path = primary.dev_path().unwrap_or_else(|| path.clone());

    let mut session = session;
    let fd = session.open(
        &device_path,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
    )?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));
    let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true)?;
    let gbm = GbmDevice::new(fd)?;

    let display = unsafe { EGLDisplay::new(gbm.clone())? };
    let context = EGLContext::new(&display)?;
    let renderer = unsafe { GlesRenderer::new(context)? };
    let render_formats = renderer.egl_context().dmabuf_render_formats().clone();

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let exporter = GbmFramebufferExporter::new(gbm.clone(), Some(node));
    let manager = DrmOutputManager::new(
        drm,
        allocator,
        exporter,
        Some(gbm),
        [Fourcc::Argb8888, Fourcc::Xrgb8888],
        render_formats,
    );

    // Hardware-accelerated clients.
    state.shm_state.update_formats(renderer.shm_formats());
    let formats = renderer.dmabuf_formats();
    let mut dmabuf_state = DmabufState::new();
    let global = dmabuf_state.create_global::<State>(&state.display_handle, formats);
    state.dmabuf = Some(DmabufGlobal {
        state: dmabuf_state,
        global,
    });

    state.backend = Backend::Udev(UdevData {
        session: session.clone(),
        renderer,
        manager,
        surfaces: HashMap::new(),
        active: true,
    });

    // Page flips.
    state
        .loop_handle
        .insert_source(drm_notifier, move |event, _, state| match event {
            DrmEvent::VBlank(crtc) => frame_finished(state, crtc),
            DrmEvent::Error(err) => eprintln!("canoe: DRM error: {err}"),
        })?;

    // Monitors coming and going.
    let udev_backend = UdevBackend::new(&seat)?;
    state
        .loop_handle
        .insert_source(udev_backend, move |event, _, state| match event {
            UdevEvent::Changed { device_id } if device_id == primary.dev_id() => {
                scan_connectors(state)
            }
            _ => {}
        })?;

    // Input devices.
    let mut libinput =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput
        .udev_assign_seat(&seat)
        .map_err(|_| "failed to assign the seat to libinput")?;
    let libinput_backend = LibinputInputBackend::new(libinput.clone());
    state
        .loop_handle
        .insert_source(libinput_backend, |event, _, state| {
            if let InputEvent::DeviceAdded { mut device } = event {
                configure_device(&mut device);
                return;
            }
            state.process_input_event(event);
        })?;

    // VT switches.
    state
        .loop_handle
        .insert_source(notifier, move |event, _, state| match event {
            SessionEvent::PauseSession => {
                libinput.suspend();
                if let Backend::Udev(ref mut data) = state.backend {
                    data.active = false;
                    data.manager.pause();
                }
            }
            SessionEvent::ActivateSession => {
                if libinput.resume().is_err() {
                    eprintln!("canoe: failed to resume libinput");
                }
                if let Backend::Udev(ref mut data) = state.backend {
                    data.active = true;
                    if let Err(err) = data.manager.activate(false) {
                        eprintln!("canoe: failed to resume DRM: {err}");
                    }
                }
                for output in &mut state.outputs {
                    output.frame_pending = false;
                }
                state.queue_redraw_all();
            }
        })?;

    scan_connectors(state);
    Ok(())
}

/// Touchpads tap to click, like everywhere else.
fn configure_device(device: &mut smithay::reexports::input::Device) {
    let _ = device.config_tap_set_enabled(true);
}

/// Set up connected connectors, tear down disconnected ones.
fn scan_connectors(state: &mut State) {
    let Backend::Udev(ref mut data) = state.backend else {
        return;
    };
    let device = data.manager.device();
    let Ok(resources) = device.resource_handles() else {
        return;
    };
    let mut connected = Vec::new();
    for handle in resources.connectors() {
        let Ok(info) = device.get_connector(*handle, true) else {
            continue;
        };
        if info.state() == connector::State::Connected && !info.modes().is_empty() {
            connected.push(info);
        }
    }

    // Gone.
    let still: HashSet<connector::Handle> = connected.iter().map(|c| c.handle()).collect();
    let gone: Vec<crtc::Handle> = data
        .surfaces
        .iter()
        .filter(|(_, s)| !still.contains(&s.connector))
        .map(|(crtc, _)| *crtc)
        .collect();
    let mut removed = Vec::new();
    for crtc in gone {
        if let Some(surface) = data.surfaces.remove(&crtc) {
            removed.push(surface.output);
        }
    }

    // New: pick a CRTC no other connector uses.
    let mut new = Vec::new();
    let known: HashSet<connector::Handle> = data.surfaces.values().map(|s| s.connector).collect();
    let mut used: HashSet<crtc::Handle> = data.surfaces.keys().copied().collect();
    for info in connected {
        if known.contains(&info.handle()) {
            continue;
        }
        let crtc = info
            .encoders()
            .iter()
            .filter_map(|e| device.get_encoder(*e).ok())
            .flat_map(|encoder| resources.filter_crtcs(encoder.possible_crtcs()))
            .find(|crtc| !used.contains(crtc));
        if let Some(crtc) = crtc {
            used.insert(crtc);
            new.push((info, crtc));
        }
    }

    for output in removed {
        state.remove_output(&output);
    }
    for (info, crtc) in new {
        connector_connected(state, info, crtc);
    }
}

fn connector_connected(state: &mut State, info: connector::Info, crtc: crtc::Handle) {
    let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
    let mode_index = info
        .modes()
        .iter()
        .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .unwrap_or(0);
    let drm_mode = info.modes()[mode_index];
    let (width_mm, height_mm) = info.size().unwrap_or((0, 0));
    let output = Output::new(
        name.clone(),
        PhysicalProperties {
            size: (width_mm as i32, height_mm as i32).into(),
            subpixel: Subpixel::Unknown,
            make: "Unknown".into(),
            model: name,
        },
    );
    for mode in info.modes() {
        output.add_mode(Mode::from(*mode));
    }
    let mode = Mode::from(drm_mode);
    output.set_preferred(mode);
    // Right of the outputs there are.
    let x = state
        .active_outputs()
        .filter_map(|o| state.output_geometry(&o.output))
        .map(|geo| geo.loc.x + geo.size.w)
        .max()
        .unwrap_or(0);
    output.change_current_state(Some(mode), None, None, Some((x, 0).into()));

    let Backend::Udev(ref mut data) = state.backend else {
        return;
    };
    let UdevData {
        manager, renderer, ..
    } = data;
    let drm_output = match manager.initialize_output::<_, CanoeElement<GlesRenderer>>(
        crtc,
        drm_mode,
        &[info.handle()],
        &output,
        None,
        renderer,
        &DrmOutputRenderElements::default(),
    ) {
        Ok(drm_output) => drm_output,
        Err(err) => {
            eprintln!("canoe: failed to set up {}: {err}", output.name());
            return;
        }
    };
    data.surfaces.insert(
        crtc,
        Surface {
            output: output.clone(),
            connector: info.handle(),
            drm_output,
            modes: info.modes().to_vec(),
            enabled: true,
        },
    );
    state.add_output(output);
    state.queue_redraw_all();
}

pub fn schedule_redraws(state: &mut State) {
    let Backend::Udev(ref data) = state.backend else {
        return;
    };
    if !data.active {
        return;
    }
    let due: Vec<(Output, crtc::Handle)> = state
        .outputs
        .iter()
        .filter(|o| o.enabled && o.redraw_needed && !o.frame_pending)
        .filter_map(|o| Some((o.output.clone(), data.crtc_for(&o.output)?)))
        .collect();
    for (output, crtc) in due {
        if let Some(managed) = state.outputs.iter_mut().find(|o| o.output == output) {
            // Rendered in an idle callback, after all pending events.
            managed.frame_pending = true;
        }
        state
            .loop_handle
            .insert_idle(move |state| render(state, crtc));
    }
}

fn render(state: &mut State, crtc: crtc::Handle) {
    let Backend::Udev(mut data) = std::mem::replace(&mut state.backend, Backend::None) else {
        return;
    };
    let output = data.surfaces.get(&crtc).map(|s| s.output.clone());
    let mut queued = false;
    let mut damage = None;
    if let Some(output) = output.as_ref() {
        let elements = state.output_elements(&mut data.renderer, output);
        let UdevData {
            surfaces, renderer, ..
        } = &mut data;
        if let Some(surface) = surfaces.get_mut(&crtc).filter(|s| s.enabled) {
            match surface.drm_output.render_frame(
                renderer,
                &elements,
                CLEAR_COLOR,
                FrameFlags::DEFAULT,
            ) {
                Ok(result) => {
                    if !result.is_empty {
                        let size = output.current_mode().map(|m| m.size).unwrap_or_default();
                        damage = Some(vec![smithay::utils::Rectangle::from_size(size)]);
                        queued = surface.drm_output.queue_frame(()).is_ok();
                    }
                }
                Err(err) => eprintln!("canoe: rendering failed: {err}"),
            }
        }
    }
    state.backend = Backend::Udev(data);
    let Some(output) = output else {
        return;
    };
    if let Some(managed) = state.outputs.iter_mut().find(|o| o.output == output) {
        managed.redraw_needed = false;
        managed.frame_pending = queued;
    }
    if let Some(damage) = damage {
        state.capture_damage(&output, &damage);
    }
    if !queued {
        // Nothing changed: let clients draw their next frame after about
        // a refresh, without a page flip.
        let refresh = output
            .current_mode()
            .map(|m| Duration::from_secs_f64(1000.0 / m.refresh.max(1) as f64))
            .unwrap_or(Duration::from_millis(16));
        let _ =
            state
                .loop_handle
                .insert_source(Timer::from_duration(refresh), move |_, _, state| {
                    state.post_render(&output);
                    TimeoutAction::Drop
                });
    }
}

fn frame_finished(state: &mut State, crtc: crtc::Handle) {
    let Backend::Udev(ref mut data) = state.backend else {
        return;
    };
    let Some(surface) = data.surfaces.get_mut(&crtc) else {
        return;
    };
    let _ = surface.drm_output.frame_submitted();
    let output = surface.output.clone();
    if let Some(managed) = state.outputs.iter_mut().find(|o| o.output == output) {
        managed.frame_pending = false;
    }
    state.post_render(&output);
    super::Backend::schedule_redraws(state);
}

pub fn set_mode(state: &mut State, output: &Output, mode: Mode) -> bool {
    let Backend::Udev(mut data) = std::mem::replace(&mut state.backend, Backend::None) else {
        return false;
    };
    let ok = match data.crtc_for(output) {
        Some(crtc) => {
            let drm_mode = data.surfaces[&crtc]
                .modes
                .iter()
                .find(|m| Mode::from(**m) == mode)
                .copied();
            match drm_mode {
                Some(drm_mode) => {
                    let UdevData {
                        manager, renderer, ..
                    } = &mut data;
                    manager
                        .use_mode::<_, CanoeElement<GlesRenderer>>(
                            &crtc,
                            drm_mode,
                            renderer,
                            &DrmOutputRenderElements::default(),
                        )
                        .is_ok()
                }
                None => false,
            }
        }
        None => false,
    };
    state.backend = Backend::Udev(data);
    if ok {
        output.change_current_state(Some(mode), None, None, None);
        state.output_changed(output);
    }
    ok
}

/// An output turned off keeps its CRTC but isn't drawn to anymore.
pub fn output_enabled(state: &mut State, output: &Output, enabled: bool) {
    if let Backend::Udev(ref mut data) = state.backend {
        if let Some(surface) = data.surfaces.values_mut().find(|s| &s.output == output) {
            surface.enabled = enabled;
        }
    }
    state.queue_redraw_all();
}
