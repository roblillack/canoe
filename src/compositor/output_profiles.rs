//! Output profiles: canoe remembers how the outputs were set up for each
//! combination of connected monitors, and sets them up that way again when
//! the combination comes back, so no kanshi is needed.
//!
//! * A profile is saved whenever a client (wlr-randr, wdisplays, kanshi, ...)
//!   applies an output configuration, or a primary output is chosen with
//!   xx-primary-output-v1. It holds every connected output: whether it is
//!   enabled, its mode, position, scale and transform, and which output is
//!   primary.
//! * When monitors come or go, the profile for the new combination is
//!   applied once things settle (an idle callback, so a dock's monitors
//!   arriving one by one are taken as one change). Without a profile canoe
//!   lines up new outputs left to right, as before.
//! * The primary output is where new windows open after such a change, and
//!   where layer surfaces without an output go. Choosing it moves keyboard
//!   focus there at once.
//!
//! Monitors are told apart by make, model and serial number from their
//! EDID, so a profile follows a monitor to another port. Monitors without
//! EDID go by connector name, and identical monitors without a serial
//! number get their connector name added.
//!
//! A laptop's lid is part of the combination: the same monitors with the
//! lid open and with it closed have a profile each. Without a profile for
//! the combination, closing the lid turns the laptop's own panel off
//! (unless nothing else is on) and opening it turns the panel back on, left
//! of the other outputs. Whatever is set up with the lid closed (say, the
//! panel turned on again) is remembered for the lid closed.
//!
//! Profiles are kept in `$XDG_STATE_HOME/canoe/outputs.toml`
//! (`~/.local/state/canoe/outputs.toml`), or in the file
//! `CANOE_OUTPUT_PROFILES` names. The headless backend, being for tests,
//! keeps none unless `CANOE_OUTPUT_PROFILES` is set. With
//! `remember_outputs = false` in canoe.toml nothing is saved or restored.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use smithay::output::{Mode, Output, Scale};
use smithay::reexports::calloop::LoopHandle;
use smithay::utils::Transform;

use super::protocols::output_management::{HeadConfiguration, ModeConfiguration};
use super::protocols::primary_output::{PrimaryOutputHandler, PrimaryOutputState};
use super::State;

/// Profiles kept at most; the least recently saved go first.
const MAX_PROFILES: usize = 64;

/// The scales canoe accepts for an output.
pub const SCALE_RANGE: std::ops::RangeInclusive<f64> = 0.25..=8.0;

const FILE_HEADER: &str = "\
# How canoe set up the outputs, one profile per combination of connected
# monitors. canoe writes this file whenever outputs are configured (with
# wlr-randr, wdisplays, kanshi, ...) or a primary output is chosen; it may
# be edited while canoe isn't running.

";

// ----------------------------------------------------------------------
// Monitor identity

/// A monitor's serial number, kept with its [`Output`] (smithay's
/// `PhysicalProperties` has no field for it).
struct SerialNumber(Option<String>);

/// Record the serial number of the monitor behind `output`.
pub fn set_serial_number(output: &Output, serial: Option<String>) {
    let serial = serial.filter(|s| !s.is_empty());
    output
        .user_data()
        .insert_if_missing_threadsafe(|| SerialNumber(serial));
}

/// The serial number of the monitor behind `output`, if known.
pub fn serial_number(output: &Output) -> Option<String> {
    output
        .user_data()
        .get::<SerialNumber>()
        .and_then(|serial| serial.0.clone())
}

/// What identifies the monitor behind `output`: make, model and serial
/// number, or the connector name if its make is unknown.
pub fn identity(output: &Output) -> String {
    let props = output.physical_properties();
    identity_of(
        &output.name(),
        &props.make,
        &props.model,
        serial_number(output).as_deref(),
    )
}

fn identity_of(connector: &str, make: &str, model: &str, serial: Option<&str>) -> String {
    if make.is_empty() || make == "Unknown" {
        return connector.to_string();
    }
    let mut parts = vec![make, model];
    parts.extend(serial);
    parts.join(" ")
}

/// The names of `outputs` in profiles: their identity, with the connector
/// name added to those that would otherwise share one.
pub fn profile_names(outputs: &[(String, String)]) -> Vec<String> {
    outputs
        .iter()
        .map(|(connector, identity)| {
            let shared = outputs
                .iter()
                .filter(|(_, other)| other == identity)
                .count()
                > 1;
            if shared {
                format!("{identity} ({connector})")
            } else {
                identity.clone()
            }
        })
        .collect()
}

/// Marks an [`Output`] as a laptop's own panel, which the lid covers.
struct Internal;

/// Mark `output` as a laptop's own panel.
pub fn set_internal(output: &Output) {
    output.user_data().insert_if_missing_threadsafe(|| Internal);
}

/// Whether `output` is a laptop's own panel.
pub fn is_internal(output: &Output) -> bool {
    output.user_data().get::<Internal>().is_some()
}

/// Whether a connector of this kind (`eDP`, `LVDS`, `DSI`) is a laptop's
/// own panel.
pub fn is_internal_connector(interface: &str) -> bool {
    matches!(interface, "eDP" | "LVDS" | "DSI")
}

/// A laptop's lid.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lid {
    #[default]
    Open,
    Closed,
}

impl Lid {
    fn name(self) -> &'static str {
        match self {
            Lid::Open => "open",
            Lid::Closed => "closed",
        }
    }
}

/// What a profile is looked up by: the connected outputs' names, and the
/// lid if one of them is a laptop's own panel.
#[derive(Debug, Clone, Default)]
struct ProfileKey {
    /// Sorted.
    outputs: Vec<String>,
    lid: Option<Lid>,
}

/// Profiles from before lids counted are for the lid open.
impl PartialEq for ProfileKey {
    fn eq(&self, other: &Self) -> bool {
        self.outputs == other.outputs
            && self.lid.unwrap_or_default() == other.lid.unwrap_or_default()
    }
}

impl std::fmt::Display for ProfileKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.outputs.join(", "))?;
        if let Some(lid) = self.lid {
            write!(f, " (lid {})", lid.name())?;
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------
// The file

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Store {
    #[serde(default, rename = "profile")]
    profiles: Vec<Profile>,
}

/// How the outputs were set up while a certain set of monitors was
/// connected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// The lid, if a laptop's own panel is among the outputs. Left out
    /// means open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lid: Option<Lid>,
    /// The primary output's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    /// Every connected output.
    #[serde(default, rename = "output")]
    pub outputs: Vec<OutputSetup>,
}

impl Profile {
    /// What it is looked up by.
    fn key(&self) -> ProfileKey {
        let mut outputs: Vec<String> = self.outputs.iter().map(|o| o.name.clone()).collect();
        outputs.sort();
        ProfileKey {
            outputs,
            lid: self.lid,
        }
    }
}

/// How one output was set up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputSetup {
    /// The monitor, see [`profile_names`].
    pub name: String,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    /// `WIDTHxHEIGHT@HZ`, e.g. `2560x1440@59.951`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Top left corner in the layout, in logical pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<(i32, i32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// `normal`, `90`, `180`, `270`, `flipped`, `flipped-90`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
}

fn enabled_default() -> bool {
    true
}

/// `2560x1440@59.951` for a mode.
fn format_mode(mode: Mode) -> String {
    format!(
        "{}x{}@{:.3}",
        mode.size.w,
        mode.size.h,
        mode.refresh as f64 / 1000.0
    )
}

/// Size and refresh rate (mHz, `None` if left out) from `2560x1440@59.951`
/// or `2560x1440`.
fn parse_mode(text: &str) -> Option<(i32, i32, Option<i32>)> {
    let (size, refresh) = match text.split_once('@') {
        Some((size, refresh)) => {
            let hz: f64 = refresh.trim().trim_end_matches("Hz").parse().ok()?;
            (size, Some((hz * 1000.0).round() as i32))
        }
        None => (text, None),
    };
    let (w, h) = size.split_once('x')?;
    let (w, h): (i32, i32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h, refresh))
}

/// Of `modes`, the one of the given size with the closest refresh rate.
fn closest_mode(modes: &[Mode], (w, h, refresh): (i32, i32, Option<i32>)) -> Option<Mode> {
    let matching = modes.iter().filter(|m| m.size.w == w && m.size.h == h);
    match refresh {
        Some(refresh) => matching
            .min_by_key(|m| (m.refresh - refresh).abs())
            .copied(),
        None => matching.max_by_key(|m| m.refresh).copied(),
    }
}

const TRANSFORMS: [(Transform, &str); 8] = [
    (Transform::Normal, "normal"),
    (Transform::_90, "90"),
    (Transform::_180, "180"),
    (Transform::_270, "270"),
    (Transform::Flipped, "flipped"),
    (Transform::Flipped90, "flipped-90"),
    (Transform::Flipped180, "flipped-180"),
    (Transform::Flipped270, "flipped-270"),
];

fn transform_name(transform: Transform) -> &'static str {
    TRANSFORMS
        .iter()
        .find(|(t, _)| *t == transform)
        .map_or("normal", |(_, name)| name)
}

fn parse_transform(name: &str) -> Option<Transform> {
    TRANSFORMS.iter().find(|(_, n)| *n == name).map(|(t, _)| *t)
}

/// How `output` is set up now.
fn setup_of(output: &Output, name: String, enabled: bool) -> OutputSetup {
    let location = output.current_location();
    OutputSetup {
        name,
        enabled,
        mode: output.current_mode().map(format_mode),
        position: Some((location.x, location.y)),
        scale: Some(output.current_scale().fractional_scale()),
        transform: Some(transform_name(output.current_transform()).to_string()),
    }
}

/// What to do to `output` to set it up as `setup` says. What `setup`
/// leaves out, or what doesn't fit the output, stays as it is.
fn head_configuration(output: &Output, setup: &OutputSetup) -> HeadConfiguration {
    let mut head = HeadConfiguration {
        output: output.clone(),
        enabled: setup.enabled,
        mode: None,
        position: None,
        transform: None,
        scale: None,
        adaptive_sync: None,
    };
    if !setup.enabled {
        return head;
    }
    head.mode = setup
        .mode
        .as_deref()
        .and_then(parse_mode)
        .and_then(|mode| closest_mode(&output.modes(), mode))
        .map(ModeConfiguration::Mode);
    head.position = setup.position.map(Into::into);
    head.transform = setup.transform.as_deref().and_then(parse_transform);
    head.scale = setup.scale.filter(|s| SCALE_RANGE.contains(s));
    head
}

/// Where profiles are kept, if anywhere.
pub fn store_path(headless: bool) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CANOE_OUTPUT_PROFILES").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    if headless {
        return None;
    }
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })?;
    Some(state_home.join("canoe").join("outputs.toml"))
}

// ----------------------------------------------------------------------
// Runtime state

/// The remembered profiles, and which of them applies now.
#[derive(Debug, Default)]
pub struct OutputProfiles {
    /// The file, or `None` to remember nothing.
    path: Option<PathBuf>,
    store: Store,
    /// The file couldn't be read: leave it alone rather than overwrite it.
    unreadable: bool,
    /// The connected outputs and the lid when the layout was last restored
    /// or saved.
    current: ProfileKey,
    /// The lid of the laptop canoe runs on (open if it has none).
    lid: Lid,
    /// The primary output's profile name.
    primary: Option<String>,
    /// A restore is queued for when things settle.
    restore_queued: bool,
    /// The pointer was put on the outputs once they first settled.
    pointer_placed: bool,
}

impl OutputProfiles {
    /// Read the profiles in `path`.
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut profiles = Self {
            path,
            ..Self::default()
        };
        let Some(path) = profiles.path.clone() else {
            return profiles;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Store>(&text) {
                Ok(store) => profiles.store = store,
                Err(err) => {
                    eprintln!(
                        "canoe: ignoring the output profiles in {}: {err}",
                        path.display()
                    );
                    profiles.unreadable = true;
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                eprintln!("canoe: cannot read {}: {err}", path.display());
                profiles.unreadable = true;
            }
        }
        profiles
    }

    fn find(&self, key: &ProfileKey) -> Option<&Profile> {
        self.store.profiles.iter().find(|p| p.key() == *key)
    }

    /// Keep `profile`, in place of the one for the same outputs, and save.
    fn remember(&mut self, profile: Profile) {
        let key = profile.key();
        self.store.profiles.retain(|p| p.key() != key);
        self.store.profiles.insert(0, profile);
        self.store.profiles.truncate(MAX_PROFILES);
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if self.unreadable {
            return;
        }
        if let Err(err) = write_store(path, &self.store) {
            eprintln!(
                "canoe: cannot save the output profiles to {}: {err}",
                path.display()
            );
        }
    }
}

fn write_store(path: &Path, store: &Store) -> Result<(), Box<dyn std::error::Error>> {
    let text = format!("{FILE_HEADER}{}", toml::to_string(store)?);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".new");
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

impl State {
    /// Whether profiles are saved and restored.
    fn remembering_outputs(&self) -> bool {
        self.output_profiles.path.is_some() && self.context.borrow().config.remember_outputs
    }

    /// Every connected output, enabled or not, with its profile name.
    fn profile_heads(&self) -> Vec<(Output, String, bool)> {
        let ids: Vec<(String, String)> = self
            .outputs
            .iter()
            .map(|o| (o.output.name(), identity(&o.output)))
            .collect();
        self.outputs
            .iter()
            .zip(profile_names(&ids))
            .map(|(o, name)| (o.output.clone(), name, o.enabled))
            .collect()
    }

    /// What the profile for `heads` is looked up by.
    fn profile_key(&self, heads: &[(Output, String, bool)]) -> ProfileKey {
        let mut outputs: Vec<String> = heads.iter().map(|(_, name, _)| name.clone()).collect();
        outputs.sort();
        let lid = heads
            .iter()
            .any(|(output, _, _)| is_internal(output))
            .then_some(self.output_profiles.lid);
        ProfileKey { outputs, lid }
    }

    /// The laptop's lid.
    pub fn lid(&self) -> Lid {
        self.output_profiles.lid
    }

    /// The lid was opened or closed: set up the outputs as remembered for
    /// that, once things settle.
    pub fn set_lid(&mut self, lid: Lid) {
        if self.output_profiles.lid == lid {
            return;
        }
        eprintln!("canoe: lid {}", lid.name());
        self.output_profiles.lid = lid;
        self.announce_lids();
        self.queue_output_profile();
    }

    /// Tell xx-primary-output-v1 clients which outputs are a laptop's panel
    /// and whether the lid is closed over them.
    pub fn announce_lids(&mut self) {
        let closed = self.output_profiles.lid == Lid::Closed;
        let lids = self
            .outputs
            .iter()
            .filter(|o| is_internal(&o.output))
            .map(|o| (o.output.name(), closed))
            .collect();
        self.primary_output_state.set_lids(lids);
    }

    /// Monitors came or went: restore the layout for those connected once
    /// things settle.
    pub fn queue_output_profile(&mut self) {
        if self.output_profiles.restore_queued {
            return;
        }
        self.output_profiles.restore_queued = true;
        let handle: LoopHandle<'static, State> = self.loop_handle.clone();
        handle.insert_idle(|state| {
            state.output_profiles.restore_queued = false;
            state.restore_output_profile();
        });
    }

    /// Set the outputs up as remembered for those connected, if they
    /// changed since the last time.
    fn restore_output_profile(&mut self) {
        let heads = self.profile_heads();
        let key = self.profile_key(&heads);
        if key == self.output_profiles.current {
            return;
        }
        self.output_profiles.current = key.clone();
        self.output_profiles.primary = None;
        let profile = if self.remembering_outputs() {
            self.output_profiles.find(&key).cloned()
        } else {
            None
        };
        if let Some(profile) = profile {
            let config: Vec<HeadConfiguration> = heads
                .iter()
                .filter_map(|(output, name, _)| {
                    let setup = profile.outputs.iter().find(|s| &s.name == name)?;
                    Some(head_configuration(output, setup))
                })
                .collect();
            if self.check_output_layout(&config) && self.apply_output_layout(config) {
                eprintln!("canoe: restored the layout of {key}");
            } else {
                eprintln!("canoe: cannot restore the layout of {key}");
            }
            self.output_management_state.update::<State>();
            let primary = profile.primary.and_then(|primary| {
                self.profile_heads()
                    .into_iter()
                    .find(|(_, name, enabled)| *enabled && *name == primary)
            });
            if let Some((output, name, _)) = primary {
                self.output_profiles.primary = Some(name);
                self.focus_primary_output(&output);
            }
        } else if let Some(config) = self.lid_layout(&heads) {
            if self.check_output_layout(&config) && self.apply_output_layout(config) {
                eprintln!(
                    "canoe: turned the laptop's panel {} for {key}",
                    if self.output_profiles.lid == Lid::Open {
                        "on"
                    } else {
                        "off"
                    }
                );
            }
            self.output_management_state.update::<State>();
        }
        self.announce_primary_output();
        if !self.output_profiles.pointer_placed {
            self.output_profiles.pointer_placed = true;
            self.center_pointer();
        }
    }

    /// Without a profile: the laptop's panels turned off while the lid is
    /// closed, unless nothing else is on, and on again left of the other
    /// outputs when it opens. `None` if nothing is to change.
    fn lid_layout(&self, heads: &[(Output, String, bool)]) -> Option<Vec<HeadConfiguration>> {
        let on = self.output_profiles.lid == Lid::Open;
        let switch = |output: &Output, enabled: bool| is_internal(output) && enabled != on;
        if !heads
            .iter()
            .any(|(output, _, enabled)| switch(output, *enabled))
        {
            return None;
        }
        let others: Vec<_> = heads
            .iter()
            .filter(|(output, _, enabled)| *enabled && !is_internal(output))
            .filter_map(|(output, _, _)| self.output_geometry(output))
            .collect();
        if !on && others.is_empty() {
            return None;
        }
        let mut left = others.iter().map(|geo| geo.loc.x).min().unwrap_or(0);
        let top = others.iter().map(|geo| geo.loc.y).min().unwrap_or(0);
        let config = heads
            .iter()
            .map(|(output, _, enabled)| {
                let mut head = HeadConfiguration {
                    output: output.clone(),
                    enabled: *enabled,
                    mode: None,
                    position: None,
                    transform: None,
                    scale: None,
                    adaptive_sync: None,
                };
                if switch(output, *enabled) {
                    head.enabled = on;
                    if on {
                        let width = self.output_geometry(output).map_or(0, |geo| geo.size.w);
                        left -= width;
                        head.position = Some((left, top).into());
                    }
                }
                head
            })
            .collect();
        Some(config)
    }

    /// Put the pointer in the middle of the primary output, or else of the
    /// output it is on (at the start, the one at the top left).
    fn center_pointer(&mut self) {
        let primary = self.output_profiles.primary.clone().and_then(|primary| {
            self.profile_heads()
                .into_iter()
                .find(|(_, name, enabled)| *enabled && *name == primary)
                .map(|(output, _, _)| output)
        });
        let Some(output) = primary
            .or_else(|| self.output_at(self.pointer_location))
            .or_else(|| self.active_outputs().next().map(|o| o.output.clone()))
        else {
            return;
        };
        let Some(area) = self.output_geometry(&output) else {
            return;
        };
        let area = area.to_f64();
        self.pointer_location = area.loc + area.size.downscale(2.0).to_point();
        self.refresh_pointer_focus();
    }

    /// Remember how the outputs are set up now, for the monitors connected.
    /// A primary output that was disabled is primary no more.
    pub fn remember_output_layout(&mut self) {
        let heads = self.profile_heads();
        let key = self.profile_key(&heads);
        let lid = key.lid;
        self.output_profiles.current = key;
        let primary = self.output_profiles.primary.take().filter(|primary| {
            heads
                .iter()
                .any(|(_, name, enabled)| *enabled && name == primary)
        });
        self.output_profiles.primary = primary.clone();
        self.announce_primary_output();
        if !self.remembering_outputs() {
            return;
        }
        let profile = Profile {
            lid,
            primary,
            outputs: heads
                .iter()
                .map(|(output, name, enabled)| setup_of(output, name.clone(), *enabled))
                .collect(),
        };
        self.output_profiles.remember(profile);
    }

    /// Have new windows and focus go to `output`.
    fn focus_primary_output(&mut self, output: &Output) {
        let Some(id) = self.managed_output(output).map(|o| o.id) else {
            return;
        };
        self.context.borrow_mut().pending_primary_output = Some(id);
        self.schedule_wm();
    }

    /// Tell xx-primary-output-v1 clients which output is primary.
    fn announce_primary_output(&mut self) {
        let primary = self.output_profiles.primary.clone();
        let name = primary.and_then(|primary| {
            self.profile_heads()
                .into_iter()
                .find(|(_, name, _)| *name == primary)
                .map(|(output, _, _)| output.name())
        });
        self.primary_output_state.set_primary(name);
    }

    /// Check the outputs' new setup before applying any of it.
    pub fn check_output_layout(&self, config: &[HeadConfiguration]) -> bool {
        // Leave at least one output on.
        if !config.iter().any(|head| head.enabled) {
            return false;
        }
        config.iter().all(|head| {
            let scale_ok = head.scale.is_none_or(|scale| SCALE_RANGE.contains(&scale));
            let mode_ok = head.mode.is_none_or(|mode| {
                super::backend::Backend::can_set_mode(self, &head.output, mode_of(mode))
            });
            scale_ok && mode_ok
        })
    }

    /// Set the outputs up as `config` says (checked with
    /// [`Self::check_output_layout`]). Returns whether that worked.
    pub fn apply_output_layout(&mut self, config: Vec<HeadConfiguration>) -> bool {
        // Move the layout so it starts at 0,0 at the least: Xwayland's
        // screen starts there, and the pointer can't reach X11 windows on
        // parts of outputs above or left of it.
        let (min_x, min_y) = config
            .iter()
            .filter(|head| head.enabled)
            .map(|head| {
                head.position
                    .unwrap_or_else(|| head.output.current_location())
            })
            .fold((0, 0), |(x, y), pos| (x.min(pos.x), y.min(pos.y)));
        let shift = smithay::utils::Point::from((-min_x, -min_y));

        let mut ok = true;
        for head in config {
            let output = head.output.clone();
            if !head.enabled {
                self.set_output_enabled(&output, false);
                continue;
            }
            if let Some(mode) = head.mode {
                if !super::backend::Backend::set_mode(self, &output, mode_of(mode)) {
                    ok = false;
                    continue;
                }
            }
            output.change_current_state(
                None,
                head.transform,
                head.scale.map(Scale::Fractional),
                Some(head.position.unwrap_or_else(|| output.current_location()) + shift),
            );
            self.set_output_enabled(&output, true);
            self.output_changed(&output);
        }
        self.queue_redraw_all();
        ok
    }
}

fn mode_of(mode: ModeConfiguration) -> Mode {
    match mode {
        ModeConfiguration::Mode(mode) => mode,
        ModeConfiguration::Custom { size, refresh } => Mode {
            size,
            refresh: refresh.unwrap_or(60_000),
        },
    }
}

impl PrimaryOutputHandler for State {
    fn primary_output_state(&mut self) -> &mut PrimaryOutputState {
        &mut self.primary_output_state
    }

    fn choose_primary_output(&mut self, name: Option<String>) {
        match name {
            Some(name) => {
                let chosen = self
                    .profile_heads()
                    .into_iter()
                    .find(|(output, _, enabled)| *enabled && output.name() == name);
                let Some((output, profile_name, _)) = chosen else {
                    return;
                };
                self.output_profiles.primary = Some(profile_name);
                self.focus_primary_output(&output);
            }
            None => self.output_profiles.primary = None,
        }
        self.announce_primary_output();
        self.remember_output_layout();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(w: i32, h: i32, refresh: i32) -> Mode {
        Mode {
            size: (w, h).into(),
            refresh,
        }
    }

    #[test]
    fn identity_falls_back_to_the_connector() {
        assert_eq!(identity_of("DP-1", "Unknown", "DP-1", None), "DP-1");
        assert_eq!(
            identity_of("DP-1", "Dell Inc.", "DELL U2723QE", Some("5KX")),
            "Dell Inc. DELL U2723QE 5KX"
        );
        assert_eq!(identity_of("eDP-1", "BOE", "0x095F", None), "BOE 0x095F");
    }

    #[test]
    fn identical_monitors_get_their_connector() {
        let outputs = vec![
            ("DP-1".to_string(), "Dell U24".to_string()),
            ("DP-2".to_string(), "Dell U24".to_string()),
            ("eDP-1".to_string(), "BOE 0x095F".to_string()),
        ];
        assert_eq!(
            profile_names(&outputs),
            vec!["Dell U24 (DP-1)", "Dell U24 (DP-2)", "BOE 0x095F"]
        );
    }

    #[test]
    fn modes_round_trip() {
        let m = mode(2560, 1440, 59_951);
        assert_eq!(format_mode(m), "2560x1440@59.951");
        assert_eq!(
            parse_mode("2560x1440@59.951"),
            Some((2560, 1440, Some(59_951)))
        );
        assert_eq!(
            parse_mode("1920x1080@60Hz"),
            Some((1920, 1080, Some(60_000)))
        );
        assert_eq!(parse_mode("1920x1080"), Some((1920, 1080, None)));
        assert_eq!(parse_mode("0x1080"), None);
        assert_eq!(parse_mode("big"), None);
    }

    #[test]
    fn picks_the_closest_refresh_rate() {
        let modes = [
            mode(1920, 1080, 60_000),
            mode(1920, 1080, 59_940),
            mode(1920, 1080, 50_000),
            mode(1280, 720, 60_000),
        ];
        assert_eq!(
            closest_mode(&modes, (1920, 1080, Some(59_950))),
            Some(modes[1])
        );
        assert_eq!(closest_mode(&modes, (1920, 1080, None)), Some(modes[0]));
        assert_eq!(closest_mode(&modes, (3840, 2160, Some(60_000))), None);
    }

    #[test]
    fn transforms_round_trip() {
        for (transform, name) in TRANSFORMS {
            assert_eq!(transform_name(transform), name);
            assert_eq!(parse_transform(name), Some(transform));
        }
        assert_eq!(parse_transform("sideways"), None);
    }

    #[test]
    fn store_round_trips_through_toml() {
        let store = Store {
            profiles: vec![Profile {
                lid: Some(Lid::Closed),
                primary: Some("Dell Inc. DELL U2723QE 5KX".into()),
                outputs: vec![
                    OutputSetup {
                        name: "Dell Inc. DELL U2723QE 5KX".into(),
                        enabled: true,
                        mode: Some("3840x2160@59.997".into()),
                        position: Some((0, 0)),
                        scale: Some(1.5),
                        transform: Some("normal".into()),
                    },
                    OutputSetup {
                        name: "BOE 0x095F".into(),
                        enabled: false,
                        mode: None,
                        position: None,
                        scale: None,
                        transform: None,
                    },
                ],
            }],
        };
        let text = toml::to_string(&store).unwrap();
        assert!(text.contains("lid = \"closed\""), "{text}");
        assert_eq!(toml::from_str::<Store>(&text).unwrap(), store);
    }

    #[test]
    fn profiles_are_found_by_their_outputs_in_any_order() {
        let setup = |name: &str| OutputSetup {
            name: name.into(),
            enabled: true,
            mode: None,
            position: None,
            scale: None,
            transform: None,
        };
        let key = |outputs: &[&str], lid| ProfileKey {
            outputs: outputs.iter().map(|o| o.to_string()).collect(),
            lid,
        };
        let mut profiles = OutputProfiles::default();
        profiles.store.profiles.push(Profile {
            lid: None,
            primary: None,
            outputs: vec![setup("B"), setup("A")],
        });
        assert!(profiles.find(&key(&["A", "B"], None)).is_some());
        assert!(profiles.find(&key(&["A"], None)).is_none());
        // Without a lid, a profile is for the lid open.
        assert!(profiles.find(&key(&["A", "B"], Some(Lid::Open))).is_some());
        assert!(profiles
            .find(&key(&["A", "B"], Some(Lid::Closed)))
            .is_none());
    }

    #[test]
    fn the_lid_open_and_closed_have_a_profile_each() {
        let profile = |lid| Profile {
            lid,
            primary: None,
            outputs: vec![OutputSetup {
                name: "A".into(),
                enabled: true,
                mode: None,
                position: None,
                scale: None,
                transform: None,
            }],
        };
        let mut profiles = OutputProfiles::default();
        profiles.remember(profile(None));
        profiles.remember(profile(Some(Lid::Closed)));
        assert_eq!(profiles.store.profiles.len(), 2);
        // Open replaces the one from before lids counted.
        profiles.remember(profile(Some(Lid::Open)));
        assert_eq!(profiles.store.profiles.len(), 2);
        let closed = ProfileKey {
            outputs: vec!["A".into()],
            lid: Some(Lid::Closed),
        };
        assert_eq!(profiles.find(&closed).unwrap().lid, Some(Lid::Closed));
        assert_eq!(closed.to_string(), "A (lid closed)");
    }

    #[test]
    fn remembering_replaces_and_saves() {
        let dir = std::env::temp_dir().join(format!("canoe-profiles-{}", std::process::id()));
        let path = dir.join("state").join("outputs.toml");
        let mut profiles = OutputProfiles::load(Some(path.clone()));
        let profile = |primary: Option<&str>| Profile {
            lid: None,
            primary: primary.map(Into::into),
            outputs: vec![OutputSetup {
                name: "A".into(),
                enabled: true,
                mode: Some("1920x1080@60.000".into()),
                position: Some((0, 0)),
                scale: Some(1.0),
                transform: Some("normal".into()),
            }],
        };
        profiles.remember(profile(None));
        profiles.remember(profile(Some("A")));
        assert_eq!(profiles.store.profiles.len(), 1);

        let loaded = OutputProfiles::load(Some(path.clone()));
        assert_eq!(loaded.store, profiles.store);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("# How canoe"));

        // A file canoe can't read is left alone.
        std::fs::write(&path, "not [toml").unwrap();
        let mut broken = OutputProfiles::load(Some(path.clone()));
        broken.remember(profile(None));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not [toml");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
