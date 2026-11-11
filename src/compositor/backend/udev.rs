//! DRM/KMS outputs and libinput devices of a logind/seatd session.
//!
//! Single GPU: everything is rendered with OpenGL ES on the primary GPU and
//! shown on its connectors. Hotplugging connectors, VT switching and mode
//! changes through wlr-output-management are supported; multi-GPU setups
//! and DRM leasing are not.
//!
//! Gamma ramps from gamma control go to the CRTC through the legacy gamma
//! ioctl. The kernel turns it into a blocking atomic commit on drivers
//! that are atomic, which waits for a page flip in flight and the next
//! vblank: up to two frames per change. Night-light tools change ramps
//! rarely enough for that.
//!
//! An output whose frames stop reaching the screen (a page flip that never
//! completes, e.g. as a dock drops the link) is started over after a
//! second: its CRTC is turned off, frames in flight are forgotten and the
//! next frame sets the mode again. Turning an output on, even if it is on
//! already, or enabling it does the same, so a monitor that went dark
//! behind canoe's back can be brought back by hand.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

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

use super::{edid, Backend};
use crate::compositor::render::{CanoeElement, CLEAR_COLOR};
use crate::compositor::State;

/// How long a queued frame may take to reach the screen before its output
/// is started over.
const FLIP_TIMEOUT: Duration = Duration::from_secs(1);

/// How often to look again at a connector that is connected but has no
/// modes yet, e.g. while its monitor is still being probed, and how long
/// apart. No further hotplug event need come once it has them.
const PROBE_RETRIES: u32 = 10;
const PROBE_RETRY_INTERVAL: Duration = Duration::from_secs(1);

type CanoeDrmOutput =
    DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

/// What a connector shows.
struct Surface {
    output: Output,
    connector: connector::Handle,
    drm_output: CanoeDrmOutput,
    /// The connector's modes, for mode changes.
    modes: Vec<smithay::reexports::drm::control::Mode>,
    /// The CRTC shows the output. False while the output is disabled or
    /// powered off: the CRTC is then blank (DPMS off) until the next frame
    /// is queued after it lights up again.
    lit: bool,
    /// Gamma ramps a client set (red, green, blue). Without, the CRTC has
    /// linear ramps.
    gamma: Option<Vec<u16>>,
    /// Since when the oldest queued frame waits to reach the screen.
    flip_since: Option<Instant>,
    /// The last frame couldn't be queued (logged once until one can).
    failing: bool,
}

impl Surface {
    /// Light the CRTC up, starting it over, or blank it. `active`: canoe
    /// has the display.
    fn set_lit(&mut self, lit: bool, active: bool) {
        if lit {
            self.lit = true;
            self.restart(active);
        } else if self.lit {
            self.lit = false;
            self.blank(active);
        }
    }

    /// Turn the CRTC off and drop the frames in flight. Returns whether
    /// that worked. Without the display, activating the session does it.
    fn blank(&mut self, active: bool) -> bool {
        self.flip_since = None;
        if !active {
            return false;
        }
        match self.drm_output.with_compositor(|c| c.clear()) {
            Ok(()) => true,
            Err(err) => {
                eprintln!("canoe: turning off {} failed: {err}", self.output.name());
                false
            }
        }
    }

    /// Start the CRTC over: turn it off, let go of frames in flight and
    /// re-read what the kernel shows, so the next frame is drawn in full
    /// and sets the mode again. Recovers a CRTC that went dark behind
    /// canoe's back or a page flip that never completed. Without the
    /// display, activating the session does this.
    fn restart(&mut self, active: bool) {
        self.drm_output.reset_buffers();
        if !active {
            self.flip_since = None;
            return;
        }
        let cleared = self.blank(true);
        let reset = self.drm_output.with_compositor(|c| {
            if !cleared {
                // Frames in flight would hold back the next one.
                while let Ok(Some(())) = c.frame_submitted() {}
            }
            c.reset_state()
        });
        if let Err(err) = reset {
            eprintln!("canoe: resetting {} failed: {err}", self.output.name());
        }
    }
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
    /// The timer that checks frames reach the screen is running.
    watchdog_armed: bool,
    /// How many more times to look again at connectors without modes.
    probe_retries: u32,
    /// The timer that looks at the connectors again is running.
    rescan_armed: bool,
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

    pub fn gamma_size(&self, output: &Output) -> Option<u32> {
        let crtc = self.crtc_for(output)?;
        crtc_gamma_size(self.manager.device(), crtc)
    }

    /// Apply gamma ramps to an output's CRTC, or linear ones. While another
    /// session has the display they are applied when canoe gets it back.
    pub fn set_gamma(&mut self, output: &Output, ramps: Option<&[u16]>) -> bool {
        let Some(crtc) = self.crtc_for(output) else {
            return false;
        };
        let Some(size) = crtc_gamma_size(self.manager.device(), crtc) else {
            return false;
        };
        if ramps.is_some_and(|r| r.len() != 3 * size as usize) {
            return false;
        }
        if self.active {
            if let Err(err) = apply_gamma(self.manager.device(), crtc, size, ramps) {
                eprintln!(
                    "canoe: setting the gamma of {} failed: {err}",
                    output.name()
                );
                return false;
            }
        }
        if let Some(surface) = self.surfaces.get_mut(&crtc) {
            surface.gamma = ramps.map(<[u16]>::to_vec);
        }
        true
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
    let mut renderer = unsafe { GlesRenderer::new(context)? };
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
    super::init_dmabuf(state, &mut renderer, Some(node));

    state.backend = Backend::Udev(UdevData {
        session: session.clone(),
        renderer,
        manager,
        surfaces: HashMap::new(),
        active: true,
        watchdog_armed: false,
        probe_retries: PROBE_RETRIES,
        rescan_armed: false,
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
                if let Backend::Udev(ref mut data) = state.backend {
                    data.probe_retries = PROBE_RETRIES;
                }
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
                    // Whoever had the display meanwhile may have changed what
                    // the CRTCs show, and frames in flight when canoe lost it
                    // never complete: start over.
                    for surface in data.surfaces.values_mut() {
                        if surface.lit {
                            surface.restart(true);
                        } else {
                            surface.blank(true);
                        }
                    }
                    // Or changed their gamma.
                    for (crtc, surface) in &data.surfaces {
                        let device = data.manager.device();
                        if let Some(size) = crtc_gamma_size(device, *crtc) {
                            let ramps = surface.gamma.as_deref();
                            if let Err(err) = apply_gamma(device, *crtc, size, ramps) {
                                eprintln!(
                                    "canoe: restoring the gamma of {} failed: {err}",
                                    surface.output.name()
                                );
                            }
                        }
                    }
                }
                for output in &mut state.outputs {
                    output.frame_pending = false;
                }
                state.queue_redraw_all();
                // Monitors may have come and gone meanwhile.
                if let Backend::Udev(ref mut data) = state.backend {
                    data.probe_retries = PROBE_RETRIES;
                }
                scan_connectors(state);
            }
        })?;

    scan_connectors(state);
    Ok(())
}

/// The number of entries in each of a CRTC's gamma ramps, if it has any.
fn crtc_gamma_size(device: &impl ControlDevice, crtc: crtc::Handle) -> Option<u32> {
    let size = device.get_crtc(crtc).ok()?.gamma_length();
    (size > 1).then_some(size)
}

/// Set a CRTC's gamma ramps: `ramps` holds the red, green and blue ramps of
/// `size` entries each; `None` sets linear ones.
fn apply_gamma(
    device: &impl ControlDevice,
    crtc: crtc::Handle,
    size: u32,
    ramps: Option<&[u16]>,
) -> std::io::Result<()> {
    let size = size as usize;
    let linear: Vec<u16>;
    let ramps = match ramps {
        Some(ramps) => ramps,
        None => {
            let ramp = (0..size).map(|i| (i * 0xffff / (size - 1)) as u16);
            linear = ramp.clone().chain(ramp.clone()).chain(ramp).collect();
            &linear
        }
    };
    let (red, rest) = ramps.split_at(size);
    let (green, blue) = rest.split_at(size);
    device.set_gamma(crtc, red, green, blue)
}

/// Touchpads tap to click, like everywhere else.
fn configure_device(device: &mut smithay::reexports::input::Device) {
    let _ = device.config_tap_set_enabled(true);
}

/// Set up connected connectors, tear down disconnected ones. Without the
/// display, activating the session does it: whoever has it meanwhile
/// may make monitors come and go just by setting their modes, and canoe
/// can't set up new ones.
fn scan_connectors(state: &mut State) {
    let Backend::Udev(ref mut data) = state.backend else {
        return;
    };
    if !data.active {
        return;
    }
    let mut probing = false;
    let device = data.manager.device();
    let Ok(resources) = device.resource_handles() else {
        return;
    };
    let mut connected = Vec::new();
    for handle in resources.connectors() {
        let Ok(info) = device.get_connector(*handle, true) else {
            continue;
        };
        if info.state() != connector::State::Connected {
            continue;
        }
        if info.modes().is_empty() {
            // E.g. while a monitor is still being probed: look again soon.
            if data.probe_retries == PROBE_RETRIES {
                eprintln!(
                    "canoe: {} is connected but has no modes yet",
                    connector_name(&info)
                );
            }
            probing = true;
            // An output set up already stays, rather than going and coming
            // back, which clients may not survive.
            if !data.surfaces.values().any(|s| s.connector == info.handle()) {
                continue;
            }
        }
        connected.push(info);
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
            eprintln!("canoe: {} disconnected", surface.output.name());
            removed.push(surface.output.clone());
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
        match crtc {
            Some(crtc) => {
                used.insert(crtc);
                new.push((info, crtc));
            }
            None => eprintln!(
                "canoe: {} connected, but no CRTC is free for it",
                connector_name(&info)
            ),
        }
    }

    for output in removed {
        state.remove_output(&output);
    }
    for (info, crtc) in new {
        connector_connected(state, info, crtc);
    }
    if probing {
        arm_rescan(state);
    }
}

/// Look at the connectors again in a moment, for those without modes yet,
/// up to `PROBE_RETRIES` times since the last hotplug event.
fn arm_rescan(state: &mut State) {
    let Backend::Udev(ref mut data) = state.backend else {
        return;
    };
    if data.rescan_armed {
        return;
    }
    if data.probe_retries == 0 {
        eprintln!("canoe: giving up on connectors that have no modes");
        return;
    }
    data.probe_retries -= 1;
    data.rescan_armed = true;
    let timer = Timer::from_duration(PROBE_RETRY_INTERVAL);
    let inserted = state.loop_handle.insert_source(timer, |_, _, state| {
        if let Backend::Udev(ref mut data) = state.backend {
            data.rescan_armed = false;
        }
        scan_connectors(state);
        TimeoutAction::Drop
    });
    if let Err(err) = inserted {
        eprintln!("canoe: cannot look at the connectors again: {err}");
        if let Backend::Udev(ref mut data) = state.backend {
            data.rescan_armed = false;
        }
    }
}

/// The name of a connector, which is also its output's name.
fn connector_name(info: &connector::Info) -> String {
    format!("{}-{}", info.interface().as_str(), info.interface_id())
}

fn connector_connected(state: &mut State, info: connector::Info, crtc: crtc::Handle) {
    let name = connector_name(&info);
    let mode_index = info
        .modes()
        .iter()
        .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .unwrap_or(0);
    let drm_mode = info.modes()[mode_index];
    let (width_mm, height_mm) = info.size().unwrap_or((0, 0));
    let identity = match state.backend {
        Backend::Udev(ref data) => edid::read(data.manager.device(), info.handle()),
        _ => None,
    };
    let (make, model, serial) = match identity {
        Some(id) => (id.make, id.model, id.serial),
        None => ("Unknown".into(), name.clone(), None),
    };
    let output = Output::new(
        name.clone(),
        PhysicalProperties {
            size: (width_mm as i32, height_mm as i32).into(),
            subpixel: Subpixel::Unknown,
            make,
            model,
        },
    );
    crate::compositor::output_profiles::set_serial_number(&output, serial);
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
            lit: true,
            gamma: None,
            flip_since: None,
            failing: false,
        },
    );
    eprintln!(
        "canoe: {} ({}) connected: {}x{}@{}Hz on CRTC {crtc:?}",
        output.name(),
        crate::compositor::output_profiles::identity(&output),
        drm_mode.size().0,
        drm_mode.size().1,
        drm_mode.vrefresh()
    );
    // Don't keep ramps an earlier session left on the CRTC.
    let device = data.manager.device();
    if let Some(size) = crtc_gamma_size(device, crtc) {
        if let Err(err) = apply_gamma(device, crtc, size, None) {
            eprintln!(
                "canoe: resetting the gamma of {} failed: {err}",
                output.name()
            );
        }
    }
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
        .filter(|o| o.displayed() && o.redraw_needed && !o.frame_pending)
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
        if let Some(surface) = surfaces.get_mut(&crtc).filter(|s| s.lit) {
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
                        match surface.drm_output.queue_frame(()) {
                            Ok(()) => {
                                queued = true;
                                surface.flip_since.get_or_insert_with(Instant::now);
                                if std::mem::take(&mut surface.failing) {
                                    eprintln!("canoe: frames reach {} again", output.name());
                                }
                            }
                            Err(err) => {
                                if !std::mem::replace(&mut surface.failing, true) {
                                    eprintln!(
                                        "canoe: showing a frame on {} failed: {err}",
                                        output.name()
                                    );
                                }
                            }
                        }
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
    if queued {
        arm_watchdog(state);
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
        // Queued for an output that is gone since.
        eprintln!("canoe: a frame completed on CRTC {crtc:?}, which shows no output");
        return;
    };
    surface.flip_since = None;
    // Submits a frame queued meanwhile.
    if let Err(err) = surface.drm_output.frame_submitted() {
        if !std::mem::replace(&mut surface.failing, true) {
            eprintln!(
                "canoe: showing a frame on {} failed: {err}",
                surface.output.name()
            );
        }
    }
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

/// An output that is disabled or powered off keeps its CRTC, blanked. One
/// that is turned on (again) is started over.
pub fn update_output_power(state: &mut State, output: &Output) {
    let lit = state.managed_output(output).is_some_and(|o| o.displayed());
    if let Backend::Udev(ref mut data) = state.backend {
        let active = data.active;
        if let Some(surface) = data.surfaces.values_mut().find(|s| &s.output == output) {
            surface.set_lit(lit, active);
        }
    }
    state.queue_redraw_all();
}

/// Start a shown output over (see `Surface::restart`).
pub fn restart_output(state: &mut State, output: &Output) {
    if !state.managed_output(output).is_some_and(|o| o.displayed()) {
        return;
    }
    let Backend::Udev(ref mut data) = state.backend else {
        return;
    };
    let active = data.active;
    let Some(surface) = data.surfaces.values_mut().find(|s| &s.output == output) else {
        return;
    };
    surface.lit = true;
    surface.restart(active);
    if let Some(managed) = state.outputs.iter_mut().find(|o| &o.output == output) {
        managed.frame_pending = false;
    }
    state.queue_redraw_all();
}

/// Check that queued frames reach the screen, while there are any.
fn arm_watchdog(state: &mut State) {
    let Backend::Udev(ref mut data) = state.backend else {
        return;
    };
    if data.watchdog_armed {
        return;
    }
    data.watchdog_armed = true;
    let timer = Timer::from_duration(FLIP_TIMEOUT);
    if let Err(err) = state
        .loop_handle
        .insert_source(timer, |_, _, state| check_flips(state))
    {
        eprintln!("canoe: cannot watch frames reach the screen: {err}");
        if let Backend::Udev(ref mut data) = state.backend {
            data.watchdog_armed = false;
        }
    }
}

/// Start over the outputs whose frames haven't reached the screen for
/// `FLIP_TIMEOUT`. Keeps checking while frames are in flight.
fn check_flips(state: &mut State) -> TimeoutAction {
    let Backend::Udev(ref mut data) = state.backend else {
        return TimeoutAction::Drop;
    };
    let now = Instant::now();
    let mut stuck = Vec::new();
    // Without the display no frames complete; activating starts over.
    if data.active {
        for surface in data.surfaces.values_mut() {
            if let Some(since) = surface.flip_since {
                if now - since >= FLIP_TIMEOUT {
                    surface.flip_since = None;
                    stuck.push((surface.output.clone(), now - since));
                }
            }
        }
    }
    let in_flight = data.surfaces.values().any(|s| s.flip_since.is_some());
    if !in_flight {
        data.watchdog_armed = false;
    }
    for (output, waited) in stuck {
        eprintln!(
            "canoe: no frame reached {} for {} ms, starting it over",
            output.name(),
            waited.as_millis()
        );
        restart_output(state, &output);
    }
    if in_flight {
        TimeoutAction::ToDuration(FLIP_TIMEOUT)
    } else {
        TimeoutAction::Drop
    }
}
