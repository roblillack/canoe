//! Server side of `wlr-output-management-unstable-v1` (`zwlr_output_manager_v1`, version 4).
//!
//! This lets tools such as `wlr-randr`, `kanshi` or `wdisplays` list the
//! compositor's outputs ("heads") and change their mode, position, transform,
//! scale and enabled state.
//!
//! # How it fits into the compositor
//!
//! Heads are tracked independently of whether the compositor currently maps an
//! output, so a disabled output stays visible to clients (with `enabled = 0`)
//! and can be re-enabled through the protocol. The compositor owns the
//! [`Output`]s and tells this module about them:
//!
//! * [`OutputManagementState::add_head`] when an output (connector/monitor)
//!   appears, [`OutputManagementState::remove_head`] when it disappears for
//!   good (unplugged), and [`OutputManagementState::set_enabled`] when it is
//!   turned on or off.
//! * Mode, position, transform and scale are read straight from the
//!   [`Output`] (`current_mode`, `current_location`, ...), so after changing
//!   them with [`Output::change_current_state`] there is nothing to report
//!   besides calling update.
//! * [`OutputManagementState::update`] after any batch of changes. It sends
//!   only what changed since the last update, followed by a `done` event with
//!   a new serial. Nothing is sent to clients before `update` is called.
//!
//! When a client applies or tests a configuration, the module checks it
//! against the protocol rules (complete, current serial, no duplicate
//! properties, valid values) and hands it to
//! [`OutputManagementHandler::apply_output_configuration`]. On a successful
//! (non-test) apply the module calls `update` itself right after sending
//! `succeeded`, so the new state reaches all clients.
//!
//! ```ignore
//! impl OutputManagementHandler for State {
//!     fn output_management_state(&mut self) -> &mut OutputManagementState {
//!         &mut self.output_management_state
//!     }
//!     fn apply_output_configuration(&mut self, config: Vec<HeadConfiguration>, test_only: bool) -> bool {
//!         // validate, and unless `test_only` apply via Output::change_current_state()
//!         // and output_management_state.set_enabled(..)
//!         true
//!     }
//! }
//! delegate_output_management!(State);
//!
//! let output_management_state = OutputManagementState::new::<State>(&display_handle);
//! ```

use std::sync::{Arc, Mutex};

use smithay::output::{Mode, Output};
use smithay::reexports::wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, AdaptiveSyncState, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use smithay::reexports::wayland_server::{
    backend::{ClientId, GlobalId},
    protocol::wl_output,
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};
use smithay::utils::{Logical, Physical, Point, Size, Transform};

const MANAGER_VERSION: u32 = 4;

/// A requested mode for a head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeConfiguration {
    /// One of the modes advertised for the head (i.e. one of [`Output::modes`]).
    Mode(Mode),
    /// A custom mode (`set_custom_mode`). Size is in hardware pixels, and
    /// `refresh` (mHz) is `None` if the client left the refresh rate
    /// unspecified. Both dimensions are guaranteed to be positive.
    Custom {
        /// Mode size in hardware pixels.
        size: Size<i32, Physical>,
        /// Refresh rate in mHz, if specified.
        refresh: Option<i32>,
    },
}

/// The requested configuration of a single head, as handed to
/// [`OutputManagementHandler::apply_output_configuration`].
///
/// For an enabled head, every `None` means the client did not set that
/// property: keep the current value (or, when the head is being re-enabled,
/// use whatever the compositor considers its default, e.g. the preferred mode).
/// For a disabled head (`enabled == false`) all other fields are `None`.
#[derive(Debug, Clone)]
pub struct HeadConfiguration {
    /// The output this head belongs to.
    pub output: Output,
    /// Whether the head should be enabled (mapped into the global space).
    pub enabled: bool,
    /// Requested mode.
    pub mode: Option<ModeConfiguration>,
    /// Requested position in the global compositor space.
    pub position: Option<Point<i32, Logical>>,
    /// Requested transform.
    pub transform: Option<Transform>,
    /// Requested scale, guaranteed to be `> 0`.
    pub scale: Option<f64>,
    /// Requested adaptive sync (VRR) state.
    pub adaptive_sync: Option<bool>,
}

/// Handler trait for the output management protocol.
pub trait OutputManagementHandler:
    GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>
    + Dispatch<ZwlrOutputManagerV1, ()>
    + Dispatch<ZwlrOutputHeadV1, OutputHeadData>
    + Dispatch<ZwlrOutputModeV1, OutputModeData>
    + Dispatch<ZwlrOutputConfigurationV1, OutputConfigurationData>
    + Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadData>
    + 'static
{
    /// Returns the protocol state.
    fn output_management_state(&mut self) -> &mut OutputManagementState;

    /// A client applied (`test_only == false`) or tested (`test_only == true`)
    /// a configuration.
    ///
    /// `config` contains exactly one entry for every head currently known to
    /// the module (every output added with
    /// [`OutputManagementState::add_head`] and not removed), in the order the
    /// heads were added.
    ///
    /// Return `true` if the configuration was (or, when testing, could be)
    /// applied, `false` otherwise. When applying fails halfway, the compositor
    /// should revert the changes it already made.
    ///
    /// When applying, change the outputs via [`Output::change_current_state`]
    /// and [`OutputManagementState::set_enabled`]. There is no need to call
    /// [`OutputManagementState::update`] from here: the module does it right
    /// after replying to the client.
    fn apply_output_configuration(&mut self, config: Vec<HeadConfiguration>, test_only: bool) -> bool;
}

/// Global data of the `zwlr_output_manager_v1` global.
pub struct OutputManagementGlobalData {
    filter: Box<dyn Fn(&Client) -> bool + Send + Sync>,
}

impl std::fmt::Debug for OutputManagementGlobalData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputManagementGlobalData").finish_non_exhaustive()
    }
}

/// User data of `zwlr_output_head_v1` objects.
#[derive(Debug)]
pub struct OutputHeadData {
    head_id: u64,
}

/// User data of `zwlr_output_mode_v1` objects.
#[derive(Debug)]
pub struct OutputModeData {
    head_id: u64,
    mode: Mode,
}

/// User data of `zwlr_output_configuration_v1` objects.
#[derive(Debug)]
pub struct OutputConfigurationData {
    inner: Arc<Mutex<PendingConfiguration>>,
}

/// User data of `zwlr_output_configuration_head_v1` objects.
#[derive(Debug)]
pub struct OutputConfigurationHeadData {
    config: Arc<Mutex<PendingConfiguration>>,
    index: usize,
}

#[derive(Debug)]
struct PendingConfiguration {
    serial: u32,
    /// Set once `apply` or `test` was received.
    used: bool,
    heads: Vec<PendingHead>,
}

#[derive(Debug, Default)]
struct PendingHead {
    head_id: u64,
    enabled: bool,
    mode: Option<ModeConfiguration>,
    position: Option<Point<i32, Logical>>,
    transform: Option<Transform>,
    scale: Option<f64>,
    adaptive_sync: Option<bool>,
}

/// Everything a client gets told about a head. Used to compute what changed.
#[derive(Debug, Clone, PartialEq)]
struct HeadSnapshot {
    // Identity: these are only sent once per head object, so a change
    // re-creates the head object.
    name: String,
    description: String,
    make: String,
    model: String,
    serial_number: Option<String>,

    physical_size: Size<i32, smithay::utils::Raw>,
    modes: Vec<Mode>,
    preferred_mode: Option<Mode>,
    enabled: bool,
    current_mode: Option<Mode>,
    position: Point<i32, Logical>,
    transform: Transform,
    scale: f64,
}

impl HeadSnapshot {
    fn same_identity(&self, other: &Self) -> bool {
        self.name == other.name
            && self.description == other.description
            && self.make == other.make
            && self.model == other.model
            && self.serial_number == other.serial_number
    }
}

#[derive(Debug)]
struct HeadEntry {
    id: u64,
    output: Output,
    enabled: bool,
    serial_number: Option<String>,
    /// Pending removal, executed by the next `update`.
    removed: bool,
    /// The state that was last announced to clients (`None`: not yet announced).
    sent: Option<HeadSnapshot>,
}

impl HeadEntry {
    fn snapshot(&self) -> HeadSnapshot {
        let physical = self.output.physical_properties();
        HeadSnapshot {
            name: self.output.name(),
            description: self.output.description(),
            make: physical.make,
            model: physical.model,
            serial_number: self.serial_number.clone(),
            physical_size: physical.size,
            modes: self.output.modes(),
            preferred_mode: self.output.preferred_mode(),
            enabled: self.enabled,
            current_mode: self.output.current_mode(),
            position: self.output.current_location(),
            transform: self.output.current_transform(),
            scale: self.output.current_scale().fractional_scale(),
        }
    }
}

/// One head as seen by one bound manager object.
#[derive(Debug)]
struct HeadInstance {
    head_id: u64,
    head: ZwlrOutputHeadV1,
    modes: Vec<(Mode, ZwlrOutputModeV1)>,
}

impl HeadInstance {
    fn finish(self) {
        for (_, mode) in self.modes {
            mode.finished();
        }
        self.head.finished();
    }
}

/// One bound `zwlr_output_manager_v1`.
#[derive(Debug)]
struct ManagerInstance {
    manager: ZwlrOutputManagerV1,
    heads: Vec<HeadInstance>,
}

/// State of the output management global.
#[derive(Debug)]
pub struct OutputManagementState {
    display: DisplayHandle,
    #[allow(dead_code)]
    global: GlobalId,
    serial: u32,
    next_head_id: u64,
    heads: Vec<HeadEntry>,
    managers: Vec<ManagerInstance>,
}

impl OutputManagementState {
    /// Creates the `zwlr_output_manager_v1` global, visible to all clients.
    pub fn new<D: OutputManagementHandler>(display: &DisplayHandle) -> Self {
        Self::new_with_filter::<D, _>(display, |_| true)
    }

    /// Creates the `zwlr_output_manager_v1` global, visible only to clients
    /// for which `filter` returns `true`.
    pub fn new_with_filter<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: OutputManagementHandler,
        F: Fn(&Client) -> bool + Send + Sync + 'static,
    {
        let global = display.create_global::<D, ZwlrOutputManagerV1, _>(
            MANAGER_VERSION,
            OutputManagementGlobalData {
                filter: Box::new(filter),
            },
        );
        Self {
            display: display.clone(),
            global,
            serial: 1,
            next_head_id: 1,
            heads: Vec::new(),
            managers: Vec::new(),
        }
    }

    /// The id of the global.
    #[allow(dead_code)]
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// The serial sent with the most recent `done` event.
    pub fn serial(&self) -> u32 {
        self.serial
    }

    /// Starts advertising a head for `output`, initially enabled. Call
    /// [`Self::set_enabled`] before the next [`Self::update`] if it starts out
    /// disabled. Adding an output that is already known does nothing.
    pub fn add_head(&mut self, output: &Output) {
        if self.entry(output).is_some() {
            return;
        }
        let id = self.next_head_id;
        self.next_head_id += 1;
        self.heads.push(HeadEntry {
            id,
            output: output.clone(),
            enabled: true,
            serial_number: None,
            removed: false,
            sent: None,
        });
    }

    /// Stops advertising the head of `output` (e.g. the monitor was
    /// unplugged). Clients are told on the next [`Self::update`].
    ///
    /// Do *not* use this for an output that is merely turned off: use
    /// [`Self::set_enabled`] so it can be turned on again.
    pub fn remove_head(&mut self, output: &Output) {
        if let Some(entry) = self.entry_mut(output) {
            entry.removed = true;
        }
    }

    /// Marks the head of `output` as enabled or disabled.
    pub fn set_enabled(&mut self, output: &Output, enabled: bool) {
        if let Some(entry) = self.entry_mut(output) {
            entry.enabled = enabled;
        }
    }

    /// Whether the head of `output` is enabled, or `None` if it is unknown.
    pub fn is_enabled(&self, output: &Output) -> Option<bool> {
        self.entry(output).map(|entry| entry.enabled)
    }

    /// The outputs of all known heads, in the order they were added.
    #[allow(dead_code)]
    pub fn outputs(&self) -> impl Iterator<Item = &Output> + '_ {
        self.heads.iter().filter(|h| !h.removed).map(|h| &h.output)
    }

    /// Sets the serial number advertised for the head of `output` (smithay's
    /// [`PhysicalProperties`](smithay::output::PhysicalProperties) has no
    /// field for it). Best done before the head is first announced.
    #[allow(dead_code)]
    pub fn set_serial_number(&mut self, output: &Output, serial_number: Option<String>) {
        if let Some(entry) = self.entry_mut(output) {
            entry.serial_number = serial_number.filter(|s| !s.is_empty());
        }
    }

    /// Sends everything that changed since the last update (new and removed
    /// heads, enabled state, modes, current mode, position, transform, scale)
    /// to all clients, followed by `done` with a new serial. Does nothing if
    /// nothing changed.
    ///
    /// Call this after every batch of output changes. Until it is called,
    /// configurations from clients are answered with `cancelled`, since they
    /// were made against outdated information.
    pub fn update<D: OutputManagementHandler>(&mut self) {
        self.managers.retain(|m| m.manager.is_alive());
        for manager in &mut self.managers {
            manager.heads.retain(|h| h.head.is_alive());
        }

        let mut changed = false;

        // Removed heads.
        for entry in self.heads.iter().filter(|h| h.removed) {
            changed |= entry.sent.is_some();
            for manager in &mut self.managers {
                if let Some(pos) = manager.heads.iter().position(|h| h.head_id == entry.id) {
                    manager.heads.remove(pos).finish();
                }
            }
        }
        self.heads.retain(|h| !h.removed);

        // New and changed heads.
        for entry in &mut self.heads {
            let new = entry.snapshot();
            match &entry.sent {
                Some(old) if *old == new => continue,
                Some(old) if old.same_identity(&new) => {
                    for manager in &mut self.managers {
                        if let Some(head) = manager.heads.iter_mut().find(|h| h.head_id == entry.id) {
                            send_head_changes::<D>(&self.display, head, old, &new);
                        }
                    }
                }
                Some(_) => {
                    // Name or other "sent once" properties changed: re-create
                    // the head object.
                    for manager in &mut self.managers {
                        if let Some(pos) = manager.heads.iter().position(|h| h.head_id == entry.id) {
                            manager.heads.remove(pos).finish();
                        }
                        if let Some(head) = create_head::<D>(&self.display, &manager.manager, entry.id, &new) {
                            manager.heads.push(head);
                        }
                    }
                }
                None => {
                    for manager in &mut self.managers {
                        if let Some(head) = create_head::<D>(&self.display, &manager.manager, entry.id, &new) {
                            manager.heads.push(head);
                        }
                    }
                }
            }
            entry.sent = Some(new);
            changed = true;
        }

        if changed {
            self.serial = self.serial.wrapping_add(1);
            for manager in &self.managers {
                manager.manager.done(self.serial);
            }
        }
    }

    fn entry(&self, output: &Output) -> Option<&HeadEntry> {
        self.heads.iter().find(|h| !h.removed && h.output == *output)
    }

    fn entry_mut(&mut self, output: &Output) -> Option<&mut HeadEntry> {
        self.heads.iter_mut().find(|h| !h.removed && h.output == *output)
    }

    /// Whether the compositor's state differs from what clients were told.
    fn has_pending_changes(&self) -> bool {
        self.heads
            .iter()
            .any(|h| h.removed || h.sent.as_ref() != Some(&h.snapshot()))
    }

    fn add_manager<D: OutputManagementHandler>(&mut self, manager: ZwlrOutputManagerV1) {
        let mut heads = Vec::new();
        for entry in &self.heads {
            if let (false, Some(snapshot)) = (entry.removed, &entry.sent) {
                if let Some(head) = create_head::<D>(&self.display, &manager, entry.id, snapshot) {
                    heads.push(head);
                }
            }
        }
        manager.done(self.serial);
        self.managers.push(ManagerInstance { manager, heads });
    }

    fn remove_manager(&mut self, manager: &ZwlrOutputManagerV1) {
        self.managers.retain(|m| m.manager != *manager);
    }
}

fn to_wl_transform(transform: Transform) -> wl_output::Transform {
    match transform {
        Transform::Normal => wl_output::Transform::Normal,
        Transform::_90 => wl_output::Transform::_90,
        Transform::_180 => wl_output::Transform::_180,
        Transform::_270 => wl_output::Transform::_270,
        Transform::Flipped => wl_output::Transform::Flipped,
        Transform::Flipped90 => wl_output::Transform::Flipped90,
        Transform::Flipped180 => wl_output::Transform::Flipped180,
        Transform::Flipped270 => wl_output::Transform::Flipped270,
    }
}

fn from_wl_transform(transform: wl_output::Transform) -> Option<Transform> {
    Some(match transform {
        wl_output::Transform::Normal => Transform::Normal,
        wl_output::Transform::_90 => Transform::_90,
        wl_output::Transform::_180 => Transform::_180,
        wl_output::Transform::_270 => Transform::_270,
        wl_output::Transform::Flipped => Transform::Flipped,
        wl_output::Transform::Flipped90 => Transform::Flipped90,
        wl_output::Transform::Flipped180 => Transform::Flipped180,
        wl_output::Transform::Flipped270 => Transform::Flipped270,
        _ => return None,
    })
}

/// Creates a head object for the client of `manager` and sends its full state.
fn create_head<D: OutputManagementHandler>(
    dh: &DisplayHandle,
    manager: &ZwlrOutputManagerV1,
    head_id: u64,
    snapshot: &HeadSnapshot,
) -> Option<HeadInstance> {
    let client = manager.client()?;
    let head = client
        .create_resource::<ZwlrOutputHeadV1, _, D>(dh, manager.version(), OutputHeadData { head_id })
        .ok()?;
    manager.head(&head);

    head.name(snapshot.name.clone());
    head.description(snapshot.description.clone());
    if snapshot.physical_size.w > 0 && snapshot.physical_size.h > 0 {
        head.physical_size(snapshot.physical_size.w, snapshot.physical_size.h);
    }
    if head.version() >= zwlr_output_head_v1::EVT_MAKE_SINCE {
        if !snapshot.make.is_empty() {
            head.make(snapshot.make.clone());
        }
        if !snapshot.model.is_empty() {
            head.model(snapshot.model.clone());
        }
        if let Some(serial_number) = &snapshot.serial_number {
            head.serial_number(serial_number.clone());
        }
    }

    let mut instance = HeadInstance {
        head_id,
        head,
        modes: Vec::new(),
    };
    send_modes::<D>(dh, &client, &mut instance, snapshot);

    instance.head.enabled(snapshot.enabled as i32);
    if snapshot.enabled {
        send_current_mode(&instance, snapshot);
        instance.head.position(snapshot.position.x, snapshot.position.y);
        instance.head.transform(to_wl_transform(snapshot.transform));
        instance.head.scale(snapshot.scale);
    }
    if instance.head.version() >= zwlr_output_head_v1::EVT_ADAPTIVE_SYNC_SINCE {
        instance.head.adaptive_sync(AdaptiveSyncState::Disabled);
    }

    Some(instance)
}

fn send_modes<D: OutputManagementHandler>(
    dh: &DisplayHandle,
    client: &Client,
    instance: &mut HeadInstance,
    snapshot: &HeadSnapshot,
) {
    for &mode in &snapshot.modes {
        let data = OutputModeData {
            head_id: instance.head_id,
            mode,
        };
        // Like libwayland, a new_id created by an event gets its parent's version.
        let Ok(resource) = client.create_resource::<ZwlrOutputModeV1, _, D>(dh, instance.head.version(), data)
        else {
            continue;
        };
        instance.head.mode(&resource);
        resource.size(mode.size.w, mode.size.h);
        if mode.refresh > 0 {
            resource.refresh(mode.refresh);
        }
        if snapshot.preferred_mode == Some(mode) {
            resource.preferred();
        }
        instance.modes.push((mode, resource));
    }
}

fn send_current_mode(instance: &HeadInstance, snapshot: &HeadSnapshot) {
    let current = snapshot
        .current_mode
        .and_then(|current| {
            instance
                .modes
                .iter()
                .find(|(mode, resource)| *mode == current && resource.is_alive())
        });
    if let Some((_, resource)) = current {
        instance.head.current_mode(resource);
    }
}

fn send_head_changes<D: OutputManagementHandler>(
    dh: &DisplayHandle,
    instance: &mut HeadInstance,
    old: &HeadSnapshot,
    new: &HeadSnapshot,
) {
    let head = instance.head.clone();
    if old.physical_size != new.physical_size && new.physical_size.w > 0 && new.physical_size.h > 0 {
        head.physical_size(new.physical_size.w, new.physical_size.h);
    }

    let modes_changed = old.modes != new.modes || old.preferred_mode != new.preferred_mode;
    if modes_changed {
        // "preferred" cannot be revoked, so simply re-create all mode objects.
        for (_, mode) in instance.modes.drain(..) {
            mode.finished();
        }
        if let Some(client) = head.client() {
            send_modes::<D>(dh, &client, instance, new);
        }
    }

    if old.enabled != new.enabled {
        head.enabled(new.enabled as i32);
    }
    if new.enabled {
        // These are only sent while enabled, so after (re-)enabling send all of them.
        let all = !old.enabled;
        if all || modes_changed || old.current_mode != new.current_mode {
            send_current_mode(instance, new);
        }
        if all || old.position != new.position {
            head.position(new.position.x, new.position.y);
        }
        if all || old.transform != new.transform {
            head.transform(to_wl_transform(new.transform));
        }
        if all || old.scale != new.scale {
            head.scale(new.scale);
        }
    }
}

impl<D: OutputManagementHandler> GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData, D>
    for OutputManagementState
{
    fn bind(
        state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _global_data: &OutputManagementGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        let oms = state.output_management_state();
        // Flush pending changes to existing clients first, so the new client
        // gets the same state for the same serial.
        oms.update::<D>();
        oms.add_manager::<D>(manager);
    }

    fn can_view(client: Client, global_data: &OutputManagementGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D: OutputManagementHandler> Dispatch<ZwlrOutputManagerV1, (), D> for OutputManagementState {
    fn request(
        state: &mut D,
        _client: &Client,
        manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    OutputConfigurationData {
                        inner: Arc::new(Mutex::new(PendingConfiguration {
                            serial,
                            used: false,
                            heads: Vec::new(),
                        })),
                    },
                );
            }
            zwlr_output_manager_v1::Request::Stop => {
                state.output_management_state().remove_manager(manager);
                manager.finished();
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, manager: &ZwlrOutputManagerV1, _data: &()) {
        state.output_management_state().remove_manager(manager);
    }
}

impl<D: OutputManagementHandler> Dispatch<ZwlrOutputHeadV1, OutputHeadData, D> for OutputManagementState {
    fn request(
        _state: &mut D,
        _client: &Client,
        _head: &ZwlrOutputHeadV1,
        request: zwlr_output_head_v1::Request,
        _data: &OutputHeadData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        // `release` is a destructor; nothing else to do.
        let _ = request;
    }

    fn destroyed(state: &mut D, _client: ClientId, head: &ZwlrOutputHeadV1, _data: &OutputHeadData) {
        for manager in &mut state.output_management_state().managers {
            manager.heads.retain(|h| h.head != *head);
        }
    }
}

impl<D: OutputManagementHandler> Dispatch<ZwlrOutputModeV1, OutputModeData, D> for OutputManagementState {
    fn request(
        _state: &mut D,
        _client: &Client,
        _mode: &ZwlrOutputModeV1,
        request: zwlr_output_mode_v1::Request,
        _data: &OutputModeData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        // `release` is a destructor; nothing else to do.
        let _ = request;
    }
}

enum Verdict {
    Cancelled,
    Configuration(Vec<HeadConfiguration>),
}

impl<D: OutputManagementHandler> Dispatch<ZwlrOutputConfigurationV1, OutputConfigurationData, D>
    for OutputManagementState
{
    fn request(
        state: &mut D,
        _client: &Client,
        configuration: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        data: &OutputConfigurationData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use zwlr_output_configuration_v1::{Error, Request};

        match request {
            Request::EnableHead { id, head } => {
                let mut inner = data.inner.lock().unwrap();
                let index = inner.heads.len();
                data_init.init(
                    id,
                    OutputConfigurationHeadData {
                        config: data.inner.clone(),
                        index,
                    },
                );
                if inner.used {
                    configuration.post_error(Error::AlreadyUsed, "configuration has already been used");
                    return;
                }
                let Some(head_id) = head.data::<OutputHeadData>().map(|d| d.head_id) else {
                    return;
                };
                if inner.heads.iter().any(|h| h.head_id == head_id) {
                    configuration.post_error(Error::AlreadyConfiguredHead, "head has already been configured");
                    return;
                }
                inner.heads.push(PendingHead {
                    head_id,
                    enabled: true,
                    ..Default::default()
                });
            }
            Request::DisableHead { head } => {
                let mut inner = data.inner.lock().unwrap();
                if inner.used {
                    configuration.post_error(Error::AlreadyUsed, "configuration has already been used");
                    return;
                }
                let Some(head_id) = head.data::<OutputHeadData>().map(|d| d.head_id) else {
                    return;
                };
                if inner.heads.iter().any(|h| h.head_id == head_id) {
                    configuration.post_error(Error::AlreadyConfiguredHead, "head has already been configured");
                    return;
                }
                inner.heads.push(PendingHead {
                    head_id,
                    enabled: false,
                    ..Default::default()
                });
            }
            request @ (Request::Apply | Request::Test) => {
                let test_only = matches!(request, Request::Test);
                let pending = {
                    let mut inner = data.inner.lock().unwrap();
                    if inner.used {
                        configuration.post_error(Error::AlreadyUsed, "configuration has already been used");
                        return;
                    }
                    inner.used = true;
                    PendingConfiguration {
                        serial: inner.serial,
                        used: true,
                        heads: std::mem::take(&mut inner.heads),
                    }
                };

                let oms = state.output_management_state();
                let verdict = match oms.check_configuration(pending) {
                    Ok(verdict) => verdict,
                    Err(message) => {
                        configuration.post_error(Error::UnconfiguredHead, message);
                        return;
                    }
                };
                match verdict {
                    Verdict::Cancelled => configuration.cancelled(),
                    Verdict::Configuration(config) => {
                        let ok = state.apply_output_configuration(config, test_only);
                        if ok {
                            configuration.succeeded();
                        } else {
                            configuration.failed();
                        }
                        if !test_only {
                            state.output_management_state().update::<D>();
                        }
                    }
                }
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl OutputManagementState {
    /// Checks a configuration that is about to be applied/tested. `Err` is a
    /// protocol error (a head was left unconfigured).
    fn check_configuration(&self, pending: PendingConfiguration) -> Result<Verdict, &'static str> {
        if pending.serial != self.serial || self.has_pending_changes() {
            return Ok(Verdict::Cancelled);
        }
        // A head that is no longer known: the client has outdated information.
        // (Cannot really happen with a current serial, but be safe.)
        if pending
            .heads
            .iter()
            .any(|p| !self.heads.iter().any(|h| h.id == p.head_id))
        {
            return Ok(Verdict::Cancelled);
        }

        let mut pending_heads = pending.heads;
        let mut config = Vec::with_capacity(self.heads.len());
        for entry in &self.heads {
            let Some(pos) = pending_heads.iter().position(|p| p.head_id == entry.id) else {
                return Err("not all heads have been configured");
            };
            let p = pending_heads.swap_remove(pos);
            config.push(if p.enabled {
                HeadConfiguration {
                    output: entry.output.clone(),
                    enabled: true,
                    mode: p.mode,
                    position: p.position,
                    transform: p.transform,
                    scale: p.scale,
                    adaptive_sync: p.adaptive_sync,
                }
            } else {
                HeadConfiguration {
                    output: entry.output.clone(),
                    enabled: false,
                    mode: None,
                    position: None,
                    transform: None,
                    scale: None,
                    adaptive_sync: None,
                }
            });
        }
        Ok(Verdict::Configuration(config))
    }
}

impl<D: OutputManagementHandler> Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadData, D>
    for OutputManagementState
{
    fn request(
        _state: &mut D,
        _client: &Client,
        config_head: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        data: &OutputConfigurationHeadData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        use zwlr_output_configuration_head_v1::{Error, Request};

        let mut config = data.config.lock().unwrap();
        // After apply/test (or if enable_head failed) the object is inert.
        if config.used {
            return;
        }
        let Some(head) = config.heads.get_mut(data.index) else {
            return;
        };

        let already_set = |what: &str| {
            config_head.post_error(Error::AlreadySet, format!("{what} has already been set"));
        };

        match request {
            Request::SetMode { mode } => {
                if head.mode.is_some() {
                    return already_set("mode");
                }
                match mode.data::<OutputModeData>() {
                    Some(m) if m.head_id == head.head_id => head.mode = Some(ModeConfiguration::Mode(m.mode)),
                    _ => config_head.post_error(Error::InvalidMode, "mode does not belong to head"),
                }
            }
            Request::SetCustomMode { width, height, refresh } => {
                if head.mode.is_some() {
                    return already_set("mode");
                }
                if width <= 0 || height <= 0 || refresh < 0 {
                    config_head.post_error(Error::InvalidCustomMode, "invalid custom mode");
                    return;
                }
                head.mode = Some(ModeConfiguration::Custom {
                    size: (width, height).into(),
                    refresh: (refresh > 0).then_some(refresh),
                });
            }
            Request::SetPosition { x, y } => {
                if head.position.is_some() {
                    return already_set("position");
                }
                head.position = Some((x, y).into());
            }
            Request::SetTransform { transform } => {
                if head.transform.is_some() {
                    return already_set("transform");
                }
                match transform {
                    WEnum::Value(t) => match from_wl_transform(t) {
                        Some(t) => head.transform = Some(t),
                        None => config_head.post_error(Error::InvalidTransform, "invalid transform"),
                    },
                    WEnum::Unknown(_) => config_head.post_error(Error::InvalidTransform, "invalid transform"),
                }
            }
            Request::SetScale { scale } => {
                if head.scale.is_some() {
                    return already_set("scale");
                }
                if !(scale > 0.0 && scale.is_finite()) {
                    config_head.post_error(Error::InvalidScale, "scale must be positive");
                    return;
                }
                head.scale = Some(scale);
            }
            Request::SetAdaptiveSync { state } => {
                if head.adaptive_sync.is_some() {
                    return already_set("adaptive sync");
                }
                match state {
                    WEnum::Value(AdaptiveSyncState::Enabled) => head.adaptive_sync = Some(true),
                    WEnum::Value(AdaptiveSyncState::Disabled) => head.adaptive_sync = Some(false),
                    _ => config_head.post_error(Error::InvalidAdaptiveSyncState, "invalid adaptive sync state"),
                }
            }
            _ => {}
        }
    }
}

/// Implements the wayland dispatch traits for the output management protocol
/// on `$ty` (which must implement [`OutputManagementHandler`]).
macro_rules! delegate_output_management {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1:
                $crate::compositor::protocols::output_management::OutputManagementGlobalData
        ] => $crate::compositor::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1: ()
        ] => $crate::compositor::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_head_v1::ZwlrOutputHeadV1:
                $crate::compositor::protocols::output_management::OutputHeadData
        ] => $crate::compositor::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_mode_v1::ZwlrOutputModeV1:
                $crate::compositor::protocols::output_management::OutputModeData
        ] => $crate::compositor::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_configuration_v1::ZwlrOutputConfigurationV1:
                $crate::compositor::protocols::output_management::OutputConfigurationData
        ] => $crate::compositor::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1:
                $crate::compositor::protocols::output_management::OutputConfigurationHeadData
        ] => $crate::compositor::protocols::output_management::OutputManagementState);
    };
}
pub(crate) use delegate_output_management;
