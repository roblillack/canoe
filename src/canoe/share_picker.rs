//! Interactive picker for screen sharing.
//!
//! The screencast portal backend (xdg-desktop-portal-wlr) runs
//! `canoe --share-picker` to ask the user what to share. That process hands the
//! portal's offer to the running window manager, which shows this picker: every
//! output is dimmed by an overlay, the window or screen under the pointer is
//! highlighted, and a click shares it. Esc or a right click declines.

use memmap2::MmapMut;
use std::fs::File;
use std::os::fd::AsFd;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::QueueHandle;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1;

use super::render::Renderer;
use super::scale::{FractionalSurface, Origin, Scale};
use super::{font, shmfile, OutputId, SeatId, WindowId};

/// Identifies one pending `canoe --share-picker` request.
pub type ShareRequestId = u64;

/// When to nudge a window after it was picked for sharing.
///
/// wlroots (0.20.2 and earlier) captures a window only when it changes. A
/// capture session that starts while another one of the same window is
/// running, and Chrome runs its preview while asking for the real share,
/// waits for the window's next change and shows black until then. If the
/// window is idle that can be forever. So after the user picks a window,
/// canoe makes it repaint, a little later and once more in case the portal
/// was slow to start capturing.
pub const NUDGE_DELAYS: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(300),
    std::time::Duration::from_millis(1500),
];

/// How long a nudge holds the changed state.
pub const NUDGE_LENGTH: std::time::Duration = std::time::Duration::from_millis(50);

/// One step of a nudge. Windows repaint when their xdg_toplevel state
/// changes, but which state they react to varies: terminals like foot react
/// to "resizing", GTK and Firefox to "tiled". A nudge sets both, then clears
/// them again; the window's size stays the same, so the capture doesn't have
/// to renegotiate its buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeStep {
    Start,
    End,
}

/// A nudge step that's due at `due`.
#[derive(Debug, Clone, Copy)]
pub struct Nudge {
    pub window: WindowId,
    pub due: std::time::Instant,
    pub step: NudgeStep,
}

/// Something the user can choose to share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareTarget {
    Output(OutputId),
    Window(WindowId),
}

/// A target together with the line to hand back to the portal if it's chosen.
#[derive(Debug, Clone)]
pub struct ShareCandidate {
    pub target: ShareTarget,
    pub reply: String,
}

/// How the picker is drawn, resolved from the UI config.
#[derive(Debug, Clone)]
pub struct PickerTheme {
    /// Overlay over everything that isn't highlighted (premultiplied ARGB).
    pub dim: u32,
    /// Fill over the highlighted target (premultiplied ARGB).
    pub highlight_fill: u32,
    /// Outline of the highlighted target (ARGB).
    pub highlight_border: u32,
    pub label_bg: u32,
    pub label_text: u32,
    pub banner_bg: u32,
    pub banner_text: u32,
    pub font_name: Option<String>,
    pub font_size: f32,
}

impl PickerTheme {
    pub fn from_ui(ui: &crate::config::UiConfig) -> Self {
        Self {
            dim: premultiply(0x00000099),
            highlight_fill: premultiply((ui.menu_highlight_bg & 0xFFFFFF00) | 0x33),
            highlight_border: opaque(ui.menu_highlight_bg),
            label_bg: opaque(ui.menu_highlight_bg),
            label_text: opaque(ui.menu_highlight_text),
            banner_bg: opaque(ui.menu_bg),
            banner_text: opaque(ui.menu_text),
            font_name: ui.font_name.clone(),
            font_size: ui.font_size,
        }
    }
}

/// What a picker surface shows, in logical pixels relative to its output.
pub struct PickerScene<'a> {
    /// The highlighted target's rectangle, `(x, y, width, height)`.
    pub highlight: Option<(i32, i32, i32, i32)>,
    /// Text naming the highlighted target, drawn inside its rectangle.
    pub label: Option<&'a str>,
    /// Instructions at the top of the output.
    pub banner: &'a str,
    pub theme: &'a PickerTheme,
}

/// Outline width of the highlighted target, in logical pixels.
const HIGHLIGHT_BORDER: i32 = 4;
/// Distance of the banner from the top of the output, in logical pixels.
const BANNER_MARGIN: i32 = 32;

/// The full-output overlay surface the picker draws on one output. It also
/// takes all pointer input there while the picker is open.
pub struct PickerSurface {
    pub surface: wl_surface::WlSurface,
    pub layer_surface: ZwlrLayerSurfaceV1,
    pub fractional: FractionalSurface,
    pub output_id: OutputId,
    pub configured: bool,
    pub width: i32,
    pub height: i32,
    buffer: Option<wl_buffer::WlBuffer>,
    pool: Option<wl_shm_pool::WlShmPool>,
    memfile: Option<File>,
    mmap: Option<MmapMut>,
    buf_width: i32,
    buf_height: i32,
    scale: Scale,
}

impl PickerSurface {
    pub fn new(
        surface: wl_surface::WlSurface,
        layer_surface: ZwlrLayerSurfaceV1,
        fractional: FractionalSurface,
        output_id: OutputId,
    ) -> Self {
        Self {
            surface,
            layer_surface,
            fractional,
            output_id,
            configured: false,
            width: 0,
            height: 0,
            buffer: None,
            pool: None,
            memfile: None,
            mmap: None,
            buf_width: 0,
            buf_height: 0,
            scale: Scale::default(),
        }
    }

    pub fn configure(&mut self, width: i32, height: i32) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.configured = true;
    }

    fn reset_buffer(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        if let Some(pool) = self.pool.take() {
            pool.destroy();
        }
        self.memfile = None;
        self.mmap = None;
    }

    fn ensure_buffer<D>(&mut self, shm: &wl_shm::WlShm, qh: &QueueHandle<D>, scale: Scale) -> bool
    where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, ()>,
    {
        let (buf_w, buf_h) = self.fractional.apply(
            &self.surface,
            self.width,
            self.height,
            &scale,
            Origin::default(),
        );
        self.scale = scale;
        if self.buffer.is_some() && (self.buf_width, self.buf_height) == (buf_w, buf_h) {
            return true;
        }
        self.reset_buffer();

        let stride = buf_w * 4;
        let size = stride * buf_h;
        let Ok(memfile) = shmfile::create("canoe-share-picker", size as i64) else {
            return false;
        };
        let Ok(mmap) = (unsafe { MmapMut::map_mut(&memfile) }) else {
            return false;
        };
        let pool = shm.create_pool(memfile.as_fd(), size, qh, ());
        let buffer = pool.create_buffer(0, buf_w, buf_h, stride, wl_shm::Format::Argb8888, qh, ());
        self.memfile = Some(memfile);
        self.mmap = Some(mmap);
        self.pool = Some(pool);
        self.buffer = Some(buffer);
        self.buf_width = buf_w;
        self.buf_height = buf_h;
        true
    }

    /// Draw `scene` at `scale` and present it.
    pub fn render<D>(
        &mut self,
        scene: &PickerScene,
        scale: Scale,
        shm: &wl_shm::WlShm,
        compositor: &wl_compositor::WlCompositor,
        qh: &QueueHandle<D>,
    ) where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, ()>
            + wayland_client::Dispatch<wl_region::WlRegion, ()>,
    {
        if !self.configured || !self.ensure_buffer(shm, qh, scale) {
            return;
        }
        let (buf_w, buf_h) = (self.buf_width, self.buf_height);
        let Some(mmap) = self.mmap.as_mut() else {
            return;
        };
        let Some(mut renderer) = Renderer::new(mmap.as_mut(), buf_w, buf_h) else {
            return;
        };
        draw_scene(&mut renderer, scene, &scale);

        let region = compositor.create_region(qh, ());
        region.add(0, 0, self.width, self.height);
        self.surface.set_input_region(Some(&region));
        region.destroy();

        if let Some(ref buffer) = self.buffer {
            self.surface.attach(Some(buffer), 0, 0);
            self.surface.damage_buffer(0, 0, buf_w, buf_h);
            self.surface.commit();
        }
    }
}

impl Drop for PickerSurface {
    fn drop(&mut self) {
        self.reset_buffer();
        self.fractional.destroy();
        self.layer_surface.destroy();
        self.surface.destroy();
    }
}

/// An open picker: what the portal offered, and what the user points at.
pub struct SharePicker {
    pub request: ShareRequestId,
    /// The seat whose binding mode was switched for the picker.
    pub seat_id: SeatId,
    /// Shareable targets: windows top to bottom, then outputs.
    pub candidates: Vec<ShareCandidate>,
    /// Index into `candidates` of the highlighted target.
    pub hovered: Option<usize>,
    pub surfaces: Vec<PickerSurface>,
    pub theme: PickerTheme,
}

impl SharePicker {
    pub fn new(
        request: ShareRequestId,
        seat_id: SeatId,
        candidates: Vec<ShareCandidate>,
        theme: PickerTheme,
    ) -> Self {
        Self {
            request,
            seat_id,
            candidates,
            hovered: None,
            surfaces: Vec::new(),
            theme,
        }
    }

    pub fn offers_windows(&self) -> bool {
        self.candidates
            .iter()
            .any(|c| matches!(c.target, ShareTarget::Window(_)))
    }

    pub fn offers_outputs(&self) -> bool {
        self.candidates
            .iter()
            .any(|c| matches!(c.target, ShareTarget::Output(_)))
    }

    pub fn index_of(&self, target: ShareTarget) -> Option<usize> {
        self.candidates.iter().position(|c| c.target == target)
    }

    /// Forget a target that went away, keeping the highlight on what it was.
    pub fn remove_target(&mut self, target: ShareTarget) {
        let hovered = self
            .hovered
            .and_then(|idx| self.candidates.get(idx))
            .map(|c| c.target)
            .filter(|&t| t != target);
        self.candidates.retain(|c| c.target != target);
        self.hovered = hovered.and_then(|t| self.index_of(t));
    }

    /// Highlight the candidate `step` places after the current one, wrapping.
    pub fn cycle(&mut self, step: isize) {
        let count = self.candidates.len() as isize;
        if count == 0 {
            return;
        }
        let next = match self.hovered {
            Some(idx) => (idx as isize + step).rem_euclid(count),
            None if step >= 0 => 0,
            None => count - 1,
        };
        self.hovered = Some(next as usize);
    }

    /// The instructions shown at the top of every output.
    pub fn banner(&self) -> &'static str {
        match (self.offers_windows(), self.offers_outputs()) {
            (true, true) => "Click a window or screen to share it \u{2022} Esc to cancel",
            (true, false) => "Click a window to share it \u{2022} Esc to cancel",
            _ => "Click a screen to share it \u{2022} Esc to cancel",
        }
    }

    pub fn surface_mut(&mut self, output_id: OutputId) -> Option<&mut PickerSurface> {
        self.surfaces.iter_mut().find(|s| s.output_id == output_id)
    }

    pub fn owns_surface(&self, surface: &wl_surface::WlSurface) -> Option<OutputId> {
        self.surfaces
            .iter()
            .find(|s| &s.surface == surface)
            .map(|s| s.output_id)
    }
}

/// Turn the portal's offer into candidates.
///
/// `lines` are what xdg-desktop-portal-wlr wrote to a "dmenu" chooser's stdin:
/// `Monitor: <name> <description>` and `Window: <title> (<identifier>)`. Each
/// is matched to an output by connector name or to a window by its
/// ext-foreign-toplevel identifier; the reply is the line itself. Lines that
/// match nothing we manage are dropped.
///
/// A "simple" chooser gets no lines at all. Then everything is offered, and
/// the reply takes the simple form: `Monitor: <name>` or `Window: <identifier>`.
pub fn resolve_candidates(
    lines: &[String],
    outputs: &[(OutputId, String)],
    windows: &[WindowInfo],
) -> Vec<ShareCandidate> {
    let mut candidates: Vec<ShareCandidate> = Vec::new();

    if lines.is_empty() {
        for window in windows {
            if let Some(identifier) = window.identifier.as_deref() {
                candidates.push(ShareCandidate {
                    target: ShareTarget::Window(window.id),
                    reply: format!("Window: {identifier}"),
                });
            }
        }
        for (output_id, name) in outputs {
            candidates.push(ShareCandidate {
                target: ShareTarget::Output(*output_id),
                reply: format!("Monitor: {name}"),
            });
        }
        return candidates;
    }

    let mut window_candidates = Vec::new();
    let mut output_candidates = Vec::new();
    for line in lines {
        if let Some(rest) = line.strip_prefix("Monitor: ") {
            let name = rest.split_whitespace().next().unwrap_or("");
            if let Some((output_id, _)) = outputs.iter().find(|(_, n)| n == name) {
                output_candidates.push(ShareCandidate {
                    target: ShareTarget::Output(*output_id),
                    reply: line.clone(),
                });
            }
        } else if let Some(rest) = line.strip_prefix("Window: ") {
            if let Some(window) = windows.iter().find(|w| w.matches(rest)) {
                window_candidates.push((
                    window.stack_index,
                    ShareCandidate {
                        target: ShareTarget::Window(window.id),
                        reply: line.clone(),
                    },
                ));
            }
        }
    }

    window_candidates.sort_by_key(|(stack_index, _)| *stack_index);
    for (_, candidate) in window_candidates {
        if !candidates.iter().any(|c| c.target == candidate.target) {
            candidates.push(candidate);
        }
    }
    for candidate in output_candidates {
        if !candidates.iter().any(|c| c.target == candidate.target) {
            candidates.push(candidate);
        }
    }
    candidates
}

/// What [`resolve_candidates`] needs to know about a window.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: WindowId,
    pub identifier: Option<String>,
    pub title: Option<String>,
    /// Position in the stacking order, topmost first.
    pub stack_index: usize,
}

impl WindowInfo {
    /// Whether a portal `Window: ` line (without the prefix) names this window.
    fn matches(&self, rest: &str) -> bool {
        match self.identifier.as_deref() {
            // dmenu form "<title> (<identifier>)", or simple form "<identifier>".
            Some(identifier) => {
                rest == identifier
                    || rest
                        .strip_suffix(')')
                        .and_then(|r| r.strip_suffix(identifier))
                        .is_some_and(|r| r.ends_with(" ("))
            }
            // Without an identifier (River before version 4 of the window
            // management protocol) the title is the best we have.
            None => self.title.as_deref().is_some_and(|title| {
                rest.strip_prefix(title)
                    .is_some_and(|r| r.starts_with(" (") && r.ends_with(')'))
            }),
        }
    }
}

fn draw_scene(renderer: &mut Renderer, scene: &PickerScene, scale: &Scale) {
    let buf_w = renderer.width();
    let buf_h = renderer.height();
    let theme = scene.theme;
    renderer.fill_rect(0, 0, buf_w, buf_h, theme.dim);

    let font_name = theme.font_name.as_deref();
    let font_px = scale.px_f(theme.font_size);
    let pad_x = scale.px(12);
    let pill_h = scale.px((theme.font_size * 2.0).round() as i32);

    if let Some((x, y, width, height)) = scene.highlight {
        let x0 = scale.position(x).max(0);
        let y0 = scale.position(y).max(0);
        let x1 = scale.position(x + width).min(buf_w);
        let y1 = scale.position(y + height).min(buf_h);
        if x1 > x0 && y1 > y0 {
            let (w, h) = (x1 - x0, y1 - y0);
            renderer.fill_rect(x0, y0, w, h, theme.highlight_fill);
            let line = scale.px(HIGHLIGHT_BORDER).max(1).min(w / 2).min(h / 2);
            let border = theme.highlight_border;
            renderer.fill_rect(x0, y0, w, line, border);
            renderer.fill_rect(x0, y1 - line, w, line, border);
            renderer.fill_rect(x0, y0 + line, line, h - 2 * line, border);
            renderer.fill_rect(x1 - line, y0 + line, line, h - 2 * line, border);

            if let Some(label) = scene.label {
                let text_w = font::measure_text(font_name, font_px, label).unwrap_or(0.0) as i32;
                let max_w = (w - 2 * line - scale.px(16)).max(0);
                let pill_w = (text_w + 2 * pad_x).min(max_w);
                if pill_w > 2 * pad_x && pill_h < h - 2 * line {
                    let px = x0 + (w - pill_w) / 2;
                    let py = y0 + (h - pill_h) / 2;
                    draw_pill(
                        renderer,
                        scene,
                        scale,
                        label,
                        (px, py, pill_w, pill_h),
                        true,
                    );
                }
            }
        }
    }

    let text_w = font::measure_text(font_name, font_px, scene.banner).unwrap_or(0.0) as i32;
    let banner_w = (text_w + 2 * pad_x).min(buf_w);
    let banner_x = (buf_w - banner_w) / 2;
    let banner_y = scale.px(BANNER_MARGIN);
    draw_pill(
        renderer,
        scene,
        scale,
        scene.banner,
        (banner_x, banner_y, banner_w, pill_h),
        false,
    );
}

/// A rounded-off text box: highlight colors for the target label, menu colors
/// for the banner.
fn draw_pill(
    renderer: &mut Renderer,
    scene: &PickerScene,
    scale: &Scale,
    text: &str,
    (x, y, width, height): (i32, i32, i32, i32),
    highlight: bool,
) {
    let theme = scene.theme;
    let (bg, fg) = if highlight {
        (theme.label_bg, theme.label_text)
    } else {
        (theme.banner_bg, theme.banner_text)
    };
    // Chip the corners by a line to soften the box.
    let line = scale.line();
    renderer.fill_rect(x + line, y, width - 2 * line, height, bg);
    renderer.fill_rect(x, y + line, width, height - 2 * line, bg);
    renderer.render_text(
        text,
        x,
        y,
        width,
        height,
        scale.factor(),
        fg,
        theme.font_size,
        theme.font_name.as_deref(),
        scale.px(12),
    );
}

/// `0xRRGGBBAA` to premultiplied `0xAARRGGBB`.
fn premultiply(rgba: u32) -> u32 {
    let a = rgba & 0xff;
    let channel = |shift: u32| (((rgba >> shift) & 0xff) * a + 127) / 255;
    (a << 24) | (channel(24) << 16) | (channel(16) << 8) | channel(8)
}

/// `0xRRGGBBAA` to opaque `0xFFRRGGBB`.
fn opaque(rgba: u32) -> u32 {
    0xFF00_0000 | (rgba >> 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: WindowId, identifier: Option<&str>, title: &str, stack: usize) -> WindowInfo {
        WindowInfo {
            id,
            identifier: identifier.map(str::to_string),
            title: Some(title.to_string()),
            stack_index: stack,
        }
    }

    #[test]
    fn dmenu_lines_resolve_to_outputs_and_windows() {
        let outputs = vec![(0, "DP-1".to_string()), (1, "HDMI-A-1".to_string())];
        let windows = vec![
            window(10, Some("abc123"), "Terminal (foot)", 1),
            window(11, Some("def456"), "Browser", 0),
        ];
        let lines: Vec<String> = [
            "Monitor: DP-1 Dell Inc. U2720Q",
            "Monitor: HDMI-A-1 Some TV",
            "Window: Terminal (foot) (abc123)",
            "Window: Browser (def456)",
            "Window: Gone (zzz)",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let candidates = resolve_candidates(&lines, &outputs, &windows);
        let targets: Vec<_> = candidates.iter().map(|c| c.target).collect();
        assert_eq!(
            targets,
            vec![
                ShareTarget::Window(11),
                ShareTarget::Window(10),
                ShareTarget::Output(0),
                ShareTarget::Output(1),
            ]
        );
        assert_eq!(candidates[1].reply, "Window: Terminal (foot) (abc123)");
        assert_eq!(candidates[2].reply, "Monitor: DP-1 Dell Inc. U2720Q");
    }

    #[test]
    fn monitors_only_offer_skips_windows() {
        let outputs = vec![(0, "DP-1".to_string())];
        let windows = vec![window(10, Some("abc"), "Terminal", 0)];
        let lines = vec!["Monitor: DP-1 Dell".to_string()];
        let candidates = resolve_candidates(&lines, &outputs, &windows);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].target, ShareTarget::Output(0));
    }

    #[test]
    fn identifier_must_match_whole_parenthesized_suffix() {
        let w = window(1, Some("12"), "x", 0);
        assert!(w.matches("Title (12)"));
        assert!(w.matches("12"));
        assert!(!w.matches("Title (112)"));
        assert!(!w.matches("Title 12)"));
    }

    #[test]
    fn title_fallback_without_identifier() {
        let w = window(1, None, "Editor", 0);
        assert!(w.matches("Editor (deadbeef)"));
        assert!(!w.matches("Editor2 (deadbeef)"));
    }

    #[test]
    fn simple_chooser_offers_everything() {
        let outputs = vec![(0, "DP-1".to_string())];
        let windows = vec![window(10, Some("abc"), "T", 0), window(11, None, "U", 1)];
        let candidates = resolve_candidates(&[], &outputs, &windows);
        let replies: Vec<_> = candidates.iter().map(|c| c.reply.as_str()).collect();
        assert_eq!(replies, vec!["Window: abc", "Monitor: DP-1"]);
    }

    #[test]
    fn cycling_wraps() {
        let mut picker = SharePicker::new(
            0,
            0,
            vec![
                ShareCandidate {
                    target: ShareTarget::Output(0),
                    reply: String::new(),
                },
                ShareCandidate {
                    target: ShareTarget::Output(1),
                    reply: String::new(),
                },
            ],
            PickerTheme::from_ui(&crate::config::UiConfig::default()),
        );
        picker.cycle(-1);
        assert_eq!(picker.hovered, Some(1));
        picker.cycle(1);
        assert_eq!(picker.hovered, Some(0));
    }

    #[test]
    fn premultiplies_rgba() {
        assert_eq!(premultiply(0xFF000080), 0x80800000);
        assert_eq!(opaque(0x11223344), 0xFF112233);
    }
}
