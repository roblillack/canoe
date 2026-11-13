//! Warp animations: a window maximized, snapped or restored glides and
//! stretches from where it was shown to where the window manager put it,
//! instead of jumping there. A window minimized shrinks into its desktop
//! icon and fades out; brought back, it grows out of the icon again.
//!
//! Nothing is rendered offscreen. The client's current buffer is stretched
//! into the rectangle the animation is at: its old content at first, while
//! the client still draws the new size, and the new content squeezed once
//! it arrives. Canoe's own frame and shadow are cut at the content's edges
//! into nine pieces like a nine-patch, so borders, the titlebar's height and
//! the shadow's corners keep their size while the rest stretches along.
//! Flying to and from its icon, a window shrinks as a whole, frame and all.

use std::rc::Rc;
use std::time::{Duration, Instant};

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::Renderer;
use smithay::desktop::utils::bbox_from_surface_tree;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::{ManagedWindow, State};
use crate::wm::WindowKey;

/// How long after its end an animation waits for the client to draw the
/// size it ends at, showing the old content stretched meanwhile.
const CATCH_UP_TIMEOUT: Duration = Duration::from_millis(500);

/// What a window's animation shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarpKind {
    /// The window moves to a new position and size.
    Geometry,
    /// The window is minimized into its desktop icon.
    ToIcon,
    /// The window comes back out of its desktop icon.
    FromIcon,
}

impl WarpKind {
    fn iconify(self) -> bool {
        self != WarpKind::Geometry
    }
}

/// A window's animation in progress.
#[derive(Debug, Clone, Copy)]
pub struct Warp {
    /// Where the window was shown when the animation began: its content,
    /// or for an icon animation its content and frame.
    from: Rectangle<f64, Logical>,
    start: Instant,
    duration: Duration,
    kind: WarpKind,
}

impl Warp {
    /// How far the animation is, from 0 to 1.
    fn progress(&self, now: Instant) -> f64 {
        if self.duration.is_zero() {
            return 1.0;
        }
        (now.saturating_duration_since(self.start).as_secs_f64() / self.duration.as_secs_f64())
            .min(1.0)
    }
}

/// Where a window's content is in one frame of its animation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarpFrame {
    /// Where the content the client drew last would be shown without the
    /// animation.
    pub drawn: Rectangle<f64, Logical>,
    /// Where it is shown instead.
    pub shown: Rectangle<f64, Logical>,
}

/// One frame of a window's animation, as it is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarpView {
    pub frame: WarpFrame,
    /// The window flies to or from its icon: it shrinks as a whole, without
    /// its shadow and popups.
    pub iconify: bool,
    /// How opaque the window is.
    pub alpha: f32,
}

/// How an element of a warped window follows its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Stretched as a whole, like the content (the client's surfaces).
    Stretch,
    /// Cut at the content's edges; only the pieces alongside the content
    /// stretch, and only along it (canoe's frame and shadow).
    Slice,
    /// Moved with the content's top left corner, not stretched (popups).
    Move,
}

/// Ease-out cubic: fast at first, settling softly.
fn ease(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Where a window goes without an icon to fly to: a fifth of its size, in
/// its middle.
fn shrunk(rect: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    let size = rect.size.to_point().upscale(0.2);
    Rectangle::new(
        rect.loc + (rect.size.to_point() - size).downscale(2.0),
        (size.x, size.y).into(),
    )
}

fn lerp_rect(
    a: Rectangle<f64, Logical>,
    b: Rectangle<f64, Logical>,
    t: f64,
) -> Rectangle<f64, Logical> {
    Rectangle::new(
        (lerp(a.loc.x, b.loc.x, t), lerp(a.loc.y, b.loc.y, t)).into(),
        (lerp(a.size.w, b.size.w, t), lerp(a.size.h, b.size.h, t)).into(),
    )
}

impl ManagedWindow {
    /// The content's size as the client last drew it.
    fn drawn_size(&self) -> Size<i32, Logical> {
        match self.toplevel() {
            Some(_) => self.window.geometry().size,
            // An X11 window's geometry is what it was configured to, even
            // before the client drew that size.
            None => bbox_from_surface_tree(self.wl_surface(), (0, 0)).size,
        }
    }

    /// Where the content the client drew last is shown, unanimated.
    fn drawn_rect(&self) -> Rectangle<i32, Logical> {
        Rectangle::new(self.pos, self.drawn_size())
    }

    /// Where the content will be shown once the client drew the size the
    /// window manager asked for.
    fn target_rect(&self) -> Rectangle<i32, Logical> {
        let drawn = self.drawn_size();
        let size = match self.toplevel() {
            Some(toplevel) => {
                let proposed = toplevel.with_pending_state(|state| state.size);
                // Once the client acknowledged the size, the one it drew
                // is final, even if it differs (e.g. a terminal's size in
                // whole cells).
                let acked = toplevel.current_state().size;
                match proposed {
                    Some(size) if Some(size) != acked => size,
                    _ => drawn,
                }
            }
            None => self.x11_size.unwrap_or(drawn),
        };
        Rectangle::new(self.target_position(), size)
    }

    /// The animation is over: it reached its end and the client drew what
    /// it ends at (or took too long for that).
    fn warp_finished(&self, now: Instant) -> bool {
        let Some(warp) = self.warp.as_ref() else {
            return false;
        };
        if self.fullscreen.is_some() {
            return true;
        }
        let elapsed = now.saturating_duration_since(warp.start);
        match warp.kind {
            WarpKind::Geometry => {
                self.hidden
                    || elapsed >= warp.duration
                        && (self.drawn_rect() == self.target_rect()
                            || elapsed >= warp.duration + CATCH_UP_TIMEOUT)
            }
            // Ended early by showing or hiding the window without one.
            WarpKind::ToIcon => !self.hidden || elapsed >= warp.duration,
            WarpKind::FromIcon => self.hidden || elapsed >= warp.duration,
        }
    }
}

impl State {
    /// The window's content with canoe's frame around it, unanimated.
    fn window_bounds(&self, window: &ManagedWindow) -> Rectangle<f64, Logical> {
        let content = window.drawn_rect().to_f64();
        if !window.ssd {
            return content;
        }
        let context = self.context.borrow();
        let Some(w) = context.windows.get(&window.id) else {
            return content;
        };
        let w = w.borrow();
        let frame = w.titlebar.as_ref().and_then(|titlebar| {
            let crate::canoe::surface::Role::Decoration { offset, .. } = titlebar.surface.role()
            else {
                return None;
            };
            let (width, height) = titlebar.surface.content()?.logical_size;
            Some(Rectangle::<f64, Logical>::new(
                (window.pos + Point::from(offset)).to_f64(),
                (width as f64, height as f64).into(),
            ))
        });
        frame.map_or(content, |frame| content.merge(frame))
    }

    /// How `window` is drawn at `now`, if it is animated.
    pub fn warp_view(&self, window: &ManagedWindow, now: Instant) -> Option<WarpView> {
        let warp = window.warp.as_ref()?;
        let t = warp.progress(now);
        let (drawn, to) = match warp.kind {
            WarpKind::Geometry => (window.drawn_rect().to_f64(), window.target_rect().to_f64()),
            WarpKind::ToIcon => {
                let bounds = self.window_bounds(window);
                let icon = window
                    .icon
                    .map_or_else(|| shrunk(bounds), |icon| icon.to_f64());
                (bounds, icon)
            }
            WarpKind::FromIcon => {
                let bounds = self.window_bounds(window);
                (bounds, bounds)
            }
        };
        if drawn.is_empty() {
            return None;
        }
        // Fading out late on the way into the icon, in early on the way out.
        let alpha = match warp.kind {
            WarpKind::Geometry => 1.0,
            WarpKind::ToIcon => 1.0 - t * t,
            WarpKind::FromIcon => 1.0 - (1.0 - t) * (1.0 - t),
        };
        Some(WarpView {
            frame: WarpFrame {
                drawn,
                shown: lerp_rect(warp.from, to, ease(t)),
            },
            iconify: warp.kind.iconify(),
            alpha: alpha as f32,
        })
    }

    /// Animate a window from where it is shown now to the position and size
    /// the window manager asks for next.
    pub fn start_warp(&mut self, key: WindowKey, duration: Duration) {
        let now = Instant::now();
        let Some(window) = self.window_by_key(key) else {
            return;
        };
        // Only windows shown in their own place are animated.
        let from = if window.hidden
            || window.fullscreen.is_some()
            || window.reported_size.is_none()
            || window.drawn_rect().is_empty()
        {
            None
        } else {
            // A window still animating continues from where it is.
            Some(
                self.warp_view(window, now)
                    .filter(|view| !view.iconify)
                    .map_or(window.drawn_rect().to_f64(), |view| view.frame.shown),
            )
        };
        self.set_warp(key, from, now, duration, WarpKind::Geometry);
    }

    /// Animate a window into its desktop icon if it is shown, or out of it
    /// if it is minimized, before the window manager hides or shows it.
    pub fn start_iconify(&mut self, key: WindowKey, duration: Duration) {
        let now = Instant::now();
        let Some(window) = self.window_by_key(key) else {
            return;
        };
        let kind = if window.hidden {
            WarpKind::FromIcon
        } else {
            WarpKind::ToIcon
        };
        let from = if window.fullscreen.is_some()
            || window.reported_size.is_none()
            || window.drawn_rect().is_empty()
        {
            None
        } else {
            let bounds = self.window_bounds(window);
            // Turning back halfway continues from where the window is.
            let current = self
                .warp_view(window, now)
                .filter(|view| view.iconify)
                .map(|view| view.frame.shown);
            Some(current.unwrap_or_else(|| {
                match kind {
                    WarpKind::FromIcon => window
                        .icon
                        .map_or_else(|| shrunk(bounds), |icon| icon.to_f64()),
                    _ => bounds,
                }
            }))
        };
        self.set_warp(key, from, now, duration, kind);
    }

    fn set_warp(
        &mut self,
        key: WindowKey,
        from: Option<Rectangle<f64, Logical>>,
        start: Instant,
        duration: Duration,
        kind: WarpKind,
    ) {
        let Some(window) = self.window_by_key_mut(key) else {
            return;
        };
        window.warp = from.map(|from| Warp {
            from,
            start,
            duration,
            kind,
        });
        if window.warp.is_some() {
            self.queue_redraw_all();
        }
    }

    /// After a frame: end finished animations and keep frames coming while
    /// any are running.
    pub fn advance_warps(&mut self) {
        let now = Instant::now();
        let mut animating = false;
        for window in &mut self.windows {
            if window.warp.is_some() {
                // One more frame shows the window where it ends.
                animating = true;
                if window.warp_finished(now) {
                    window.warp = None;
                }
            }
        }
        if animating {
            self.queue_redraw_all();
        }
    }
}

/// Maps one axis of a warped window: positions alongside the content
/// stretch with it, positions before or after it keep their distance to
/// its nearest edge.
#[derive(Debug, Clone, Copy)]
struct Axis {
    /// The content's edges as drawn.
    drawn: (f64, f64),
    /// The content's edges as shown.
    shown: (f64, f64),
}

impl Axis {
    fn map(&self, x: f64) -> f64 {
        let (d0, d1) = self.drawn;
        let (s0, s1) = self.shown;
        if x <= d0 || d1 <= d0 {
            x + (s0 - d0)
        } else if x >= d1 {
            x + (s1 - d1)
        } else {
            s0 + (x - d0) * (s1 - s0) / (d1 - d0)
        }
    }
}

/// An element drawn somewhere else than it would be, possibly only a piece
/// of it.
#[derive(Debug)]
pub struct WarpedElement<E> {
    /// Shared by the pieces of one element.
    element: Rc<E>,
    id: Id,
    src: Rectangle<f64, Buffer>,
    geometry: Rectangle<i32, Physical>,
}

impl<E: Element> WarpedElement<E> {
    /// All of `element`, drawn into `geometry`.
    pub fn at(element: E, geometry: Rectangle<i32, Physical>) -> Self {
        WarpedElement {
            id: element.id().clone(),
            src: element.src(),
            element: Rc::new(element),
            geometry,
        }
    }
}

impl<E: Element> Element for WarpedElement<E> {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.element.current_commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.src
    }

    fn transform(&self) -> Transform {
        self.element.transform()
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }

    fn alpha(&self) -> f32 {
        self.element.alpha()
    }

    fn kind(&self) -> Kind {
        self.element.kind()
    }
}

impl<R: Renderer, E: RenderElement<R>> RenderElement<R> for WarpedElement<E> {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), R::Error> {
        self.element.draw(frame, src, dst, damage, opaque_regions)
    }

    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        // Stretched: not something to scan out as it is.
        None
    }
}

/// Where the content's edges are in one frame, in physical coordinates of
/// the output being rendered.
pub struct FrameMapping {
    x: Axis,
    y: Axis,
}

impl FrameMapping {
    pub fn new(
        frame: &WarpFrame,
        output_loc: Point<f64, Logical>,
        scale: Scale<f64>,
    ) -> FrameMapping {
        let to_phys = |rect: Rectangle<f64, Logical>| {
            let start = (rect.loc - output_loc).to_physical(scale);
            let end = (rect.loc + rect.size - output_loc).to_physical(scale);
            ((start.x, end.x), (start.y, end.y))
        };
        let (drawn_x, drawn_y) = to_phys(frame.drawn);
        let (shown_x, shown_y) = to_phys(frame.shown);
        FrameMapping {
            x: Axis {
                drawn: drawn_x,
                shown: shown_x,
            },
            y: Axis {
                drawn: drawn_y,
                shown: shown_y,
            },
        }
    }

    /// `element` the way it shows in this frame: `fit` decides how it
    /// follows the content. May take several pieces.
    pub fn place<E: Element>(
        &self,
        element: E,
        fit: Fit,
        scale: Scale<f64>,
    ) -> Vec<WarpedElement<E>> {
        let geometry = element.geometry(scale);
        let src = element.src();
        if geometry.is_empty() {
            return Vec::new();
        }
        let x0 = geometry.loc.x as f64;
        let y0 = geometry.loc.y as f64;
        let x1 = x0 + geometry.size.w as f64;
        let y1 = y0 + geometry.size.h as f64;

        if fit == Fit::Move {
            let offset = Point::<f64, Physical>::from((
                self.x.shown.0 - self.x.drawn.0,
                self.y.shown.0 - self.y.drawn.0,
            ))
            .to_i32_round();
            let id = element.id().clone();
            return vec![WarpedElement {
                element: Rc::new(element),
                id,
                src,
                geometry: Rectangle::new(geometry.loc + offset, geometry.size),
            }];
        }

        // Where to cut, along each axis.
        let cuts = |axis: &Axis, start: f64, end: f64| -> Vec<f64> {
            let mut cuts = vec![start];
            if fit == Fit::Slice {
                for edge in [axis.drawn.0.round(), axis.drawn.1.round()] {
                    if edge > start && edge < end && Some(&edge) != cuts.last() {
                        cuts.push(edge);
                    }
                }
            }
            cuts.push(end);
            cuts
        };
        // Pieces can only be cut from untransformed buffers.
        let (xs, ys) = if element.transform() == Transform::Normal {
            (cuts(&self.x, x0, x1), cuts(&self.y, y0, y1))
        } else {
            (vec![x0, x1], vec![y0, y1])
        };
        let pieces = (xs.len() - 1) * (ys.len() - 1);
        let element = Rc::new(element);
        let mut placed = Vec::with_capacity(pieces);
        for row in ys.windows(2) {
            for column in xs.windows(2) {
                let (a, b) = (column[0], column[1]);
                let (c, d) = (row[0], row[1]);
                let piece_src = Rectangle::<f64, Buffer>::new(
                    (
                        src.loc.x + (a - x0) / (x1 - x0) * src.size.w,
                        src.loc.y + (c - y0) / (y1 - y0) * src.size.h,
                    )
                        .into(),
                    (
                        (b - a) / (x1 - x0) * src.size.w,
                        (d - c) / (y1 - y0) * src.size.h,
                    )
                        .into(),
                );
                let left = self.x.map(a).round() as i32;
                let right = self.x.map(b).round() as i32;
                let top = self.y.map(c).round() as i32;
                let bottom = self.y.map(d).round() as i32;
                if right <= left || bottom <= top {
                    continue;
                }
                // Pieces of one element are told apart by the damage
                // tracker by their ids.
                let id = if pieces == 1 {
                    element.id().clone()
                } else {
                    Id::new()
                };
                placed.push(WarpedElement {
                    element: element.clone(),
                    id,
                    src: piece_src,
                    geometry: Rectangle::new(
                        (left, top).into(),
                        (right - left, bottom - top).into(),
                    ),
                });
            }
        }
        placed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rectangle<f64, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    fn solid(x: i32, y: i32, w: i32, h: i32) -> SolidColorRenderElement {
        let buffer = SolidColorBuffer::new((w, h), [1.0, 0.0, 0.0, 1.0]);
        SolidColorRenderElement::from_buffer(
            &buffer,
            Point::<i32, Physical>::from((x, y)),
            Scale::from(1.0),
            1.0,
            Kind::Unspecified,
        )
    }

    /// A window's content at (10, 20) 80x70 shown at (10, 20) 180x170.
    fn grown() -> FrameMapping {
        let frame = WarpFrame {
            drawn: rect(10.0, 20.0, 80.0, 70.0),
            shown: rect(10.0, 20.0, 180.0, 170.0),
        };
        FrameMapping::new(&frame, (0.0, 0.0).into(), Scale::from(1.0))
    }

    fn geometries<E>(placed: &[WarpedElement<E>]) -> Vec<(i32, i32, i32, i32)> {
        placed
            .iter()
            .map(|e| {
                let g = e.geometry;
                (g.loc.x, g.loc.y, g.size.w, g.size.h)
            })
            .collect()
    }

    #[test]
    fn easing_starts_and_ends_in_place() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert!(ease(0.5) > 0.5);
    }

    #[test]
    fn axis_stretches_inside_and_keeps_distances_outside() {
        let axis = Axis {
            drawn: (10.0, 90.0),
            shown: (110.0, 270.0),
        };
        assert_eq!(axis.map(0.0), 100.0);
        assert_eq!(axis.map(10.0), 110.0);
        assert_eq!(axis.map(50.0), 190.0);
        assert_eq!(axis.map(90.0), 270.0);
        assert_eq!(axis.map(95.0), 275.0);
    }

    #[test]
    fn axis_of_empty_content_moves() {
        let axis = Axis {
            drawn: (10.0, 10.0),
            shown: (20.0, 30.0),
        };
        assert_eq!(axis.map(10.0), 20.0);
    }

    #[test]
    fn slices_keep_the_frame_around_the_content() {
        let placed = grown().place(solid(0, 0, 100, 100), Fit::Slice, Scale::from(1.0));
        assert_eq!(
            geometries(&placed),
            vec![
                (0, 0, 10, 20),
                (10, 0, 180, 20),
                (190, 0, 10, 20),
                (0, 20, 10, 170),
                (10, 20, 180, 170),
                (190, 20, 10, 170),
                (0, 190, 10, 10),
                (10, 190, 180, 10),
                (190, 190, 10, 10),
            ]
        );
        // Every piece draws its own part of the buffer.
        let src = solid(0, 0, 100, 100).src();
        let top_middle = placed[1].src;
        assert_eq!(top_middle.loc.x, src.size.w * 0.1);
        assert_eq!(top_middle.size.w, src.size.w * 0.8);
        assert_eq!(top_middle.size.h, src.size.h * 0.2);
        // Their ids differ, so damage tracking tells them apart.
        let ids: std::collections::HashSet<_> = placed.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids.len(), 9);
    }

    #[test]
    fn a_titlebar_above_the_content_only_stretches_sideways() {
        let placed = grown().place(solid(5, 0, 90, 20), Fit::Slice, Scale::from(1.0));
        assert_eq!(
            geometries(&placed),
            vec![(5, 0, 5, 20), (10, 0, 180, 20), (190, 0, 5, 20)]
        );
    }

    #[test]
    fn content_stretches_as_a_whole() {
        let element = solid(10, 20, 80, 70);
        let id = element.id().clone();
        let placed = grown().place(element, Fit::Stretch, Scale::from(1.0));
        assert_eq!(geometries(&placed), vec![(10, 20, 180, 170)]);
        assert_eq!(placed[0].id, id);
    }

    #[test]
    fn popups_move_with_the_corner() {
        let frame = WarpFrame {
            drawn: rect(10.0, 20.0, 80.0, 70.0),
            shown: rect(0.0, 0.0, 180.0, 170.0),
        };
        let mapping = FrameMapping::new(&frame, (0.0, 0.0).into(), Scale::from(1.0));
        let placed = mapping.place(solid(50, 50, 30, 40), Fit::Move, Scale::from(1.0));
        assert_eq!(geometries(&placed), vec![(40, 30, 30, 40)]);
    }

    #[test]
    fn mapping_is_relative_to_the_output_and_scaled() {
        let frame = WarpFrame {
            drawn: rect(1010.0, 20.0, 80.0, 70.0),
            shown: rect(1010.0, 20.0, 180.0, 170.0),
        };
        let mapping = FrameMapping::new(&frame, (1000.0, 0.0).into(), Scale::from(2.0));
        let placed = mapping.place(solid(20, 40, 160, 140), Fit::Stretch, Scale::from(2.0));
        assert_eq!(geometries(&placed), vec![(20, 40, 360, 340)]);
    }
}
