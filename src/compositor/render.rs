//! What an output shows, as render elements, topmost first.
//!
//! Stacking, bottom to top: the background layer (canoe's fill below
//! wallpaper clients), the bottom layer (canoe's desktop icons below bars),
//! windows with their shadows and frames, the top layer, a fullscreen
//! window, the overlay layer with canoe's menus and picker, and the cursor.
//! While the session is locked, only the lock surface is shown.
//!
//! Every element is placed at its logical position times the output scale,
//! *unrounded*: Smithay then covers device pixels `round(x·s)` to
//! `round((x + w)·s)`, the rule canoe's decorations are drawn for, so frames
//! meet window content exactly at fractional scales.

use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::utils::CropRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::backend::renderer::{ImportAll, ImportMem, Renderer};
use smithay::desktop::{layer_map_for_output, PopupManager};
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale};
use smithay::wayland::compositor::{with_states, with_surface_tree_downward, TraversalAction};
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use super::warp::{Fit, FrameMapping, WarpedElement};
use super::{LockState, State};
use crate::canoe;

smithay::backend::renderer::element::render_elements! {
    pub CanoeElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Cropped=CropRenderElement<WaylandSurfaceRenderElement<R>>,
    Memory=MemoryRenderBufferRenderElement<R>,
    Solid=SolidColorRenderElement,
    Warped=WarpedElement<CanoeElement<R>>,
}

/// Colors drawn where nothing else is.
pub const CLEAR_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Render elements for a surface tree whose origin is at `location`
/// (physical, unrounded). Like Smithay's `render_elements_from_surface_tree`,
/// but keeps the location fractional.
pub fn surface_tree_elements<R>(
    renderer: &mut R,
    surface: &WlSurface,
    location: Point<f64, Physical>,
    scale: Scale<f64>,
    alpha: f32,
    kind: Kind,
) -> Vec<WaylandSurfaceRenderElement<R>>
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
{
    let mut elements = Vec::new();
    with_surface_tree_downward(
        surface,
        location,
        |_, states, location| {
            let mut location = *location;
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else {
                return TraversalAction::SkipChildren;
            };
            match data.lock().unwrap().view() {
                Some(view) => {
                    location += view.offset.to_f64().to_physical(scale);
                    TraversalAction::DoChildren(location)
                }
                None => TraversalAction::SkipChildren,
            }
        },
        |surface, states, location| {
            let mut location = *location;
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else {
                return;
            };
            let Some(view) = data.lock().unwrap().view() else {
                return;
            };
            location += view.offset.to_f64().to_physical(scale);
            if let Ok(Some(element)) = WaylandSurfaceRenderElement::from_surface(
                renderer, surface, states, location, alpha, kind,
            ) {
                elements.push(element);
            }
        },
        |_, _, _| true,
    );
    // The downward traversal visits the topmost surface first already.
    elements
}

/// A surface tree plus its popups, topmost first.
pub(super) fn surface_with_popups<R>(
    renderer: &mut R,
    surface: &WlSurface,
    location: Point<f64, Physical>,
    scale: Scale<f64>,
    alpha: f32,
) -> (
    Vec<WaylandSurfaceRenderElement<R>>,
    Vec<WaylandSurfaceRenderElement<R>>,
)
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
{
    let mut popups = Vec::new();
    for (popup, offset) in PopupManager::popups_for_surface(surface) {
        let geometry = popup.geometry();
        let at = location + (offset - geometry.loc).to_f64().to_physical(scale);
        let mut elements = surface_tree_elements(
            renderer,
            popup.wl_surface(),
            at,
            scale,
            alpha,
            Kind::Unspecified,
        );
        // Later popups are on top.
        elements.append(&mut popups);
        popups = elements;
    }
    let main = surface_tree_elements(renderer, surface, location, scale, alpha, Kind::Unspecified);
    (main, popups)
}

/// One of the window manager's surfaces at global `origin`: one element,
/// or one for each piece of a buffer with a hole.
pub(super) fn canoe_surface<R>(
    renderer: &mut R,
    surface: &canoe::surface::Surface,
    origin: Point<f64, Logical>,
    output_loc: Point<f64, Logical>,
    scale: Scale<f64>,
    alpha: f32,
) -> Vec<CanoeElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Send + 'static,
{
    let Some(content) = surface.content() else {
        return Vec::new();
    };
    let location = (origin - output_loc).to_physical(scale);
    // The device pixels the surface covers.
    let size = content.logical_size;
    let start = location.to_i32_round::<i32>();
    let end = (location
        + Point::<f64, Logical>::from((size.0 as f64, size.1 as f64)).to_physical(scale))
    .to_i32_round::<i32>();
    if let Some(mut solid) = content.solid {
        if solid.color().a() == 0.0 {
            return Vec::new();
        }
        solid.resize(((end.x - start.x).max(0), (end.y - start.y).max(0)));
        return vec![CanoeElement::Solid(SolidColorRenderElement::from_buffer(
            &solid,
            start,
            Scale::from(1.0),
            alpha,
            Kind::Unspecified,
        ))];
    }
    let (bw, bh) = content.buffer_size;
    let alpha = (alpha < 1.0).then_some(alpha);
    let mut elements = Vec::with_capacity(content.pieces.len());
    for piece in &content.pieces {
        let rect = piece.rect;
        if rect.size == (bw, bh).into() {
            // The whole buffer, drawn at the surface's logical size: device
            // pixels map 1:1 where canoe drew for this scale; a single pixel
            // stretches.
            let src = Rectangle::from_size((bw as f64, bh as f64).into());
            elements.extend(
                MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    location,
                    &piece.memory,
                    alpha,
                    Some(src),
                    Some(size.into()),
                    Kind::Unspecified,
                )
                .ok()
                .map(CanoeElement::Memory),
            );
            continue;
        }
        // A piece covers the part of the surface's device pixels its own
        // pixels are of the buffer's, so pieces meet without a seam.
        let along = |from: i32, to: i32, at: i32, total: i32| {
            from + (at as f64 * (to - from) as f64 / total as f64).round() as i32
        };
        let x0 = along(start.x, end.x, rect.loc.x, bw);
        let x1 = along(start.x, end.x, rect.loc.x + rect.size.w, bw);
        let y0 = along(start.y, end.y, rect.loc.y, bh);
        let y1 = along(start.y, end.y, rect.loc.y + rect.size.h, bh);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            Point::<f64, Physical>::from((x0 as f64, y0 as f64)),
            &piece.memory,
            alpha,
            None,
            None,
            Kind::Unspecified,
        ) else {
            continue;
        };
        elements.push(CanoeElement::Warped(WarpedElement::at(
            CanoeElement::Memory(element),
            Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into()),
        )));
    }
    elements
}

/// Where a layer-type surface of the window manager's sits, globally.
fn canoe_layer_origin(
    surface: &canoe::surface::Surface,
    output_loc: Point<i32, Logical>,
    output_size: (i32, i32),
) -> Option<Point<f64, Logical>> {
    match surface.role() {
        canoe::surface::Role::Layer(placement) => {
            let (x, y, _, _) = placement.geometry(output_size);
            Some((output_loc + Point::<i32, Logical>::from((x, y))).to_f64())
        }
        _ => None,
    }
}

impl State {
    /// Everything `output` shows, topmost first.
    pub fn output_elements<R>(&mut self, renderer: &mut R, output: &Output) -> Vec<CanoeElement<R>>
    where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        self.apply_expired_positions();
        let Some(geometry) = self.output_geometry(output) else {
            return Vec::new();
        };
        let Some(managed) = self.managed_output(output) else {
            return Vec::new();
        };
        let (output_key, output_id) = (managed.key, managed.id);
        let output_loc = geometry.loc.to_f64();
        let output_size = (geometry.size.w, geometry.size.h);
        let fractional = output.current_scale().fractional_scale();
        let scale = Scale::from(fractional);
        let to_phys = |p: Point<f64, Logical>| (p - output_loc).to_physical(scale);

        // Built bottom to top, reversed at the end.
        let mut bottom_up: Vec<CanoeElement<R>> = Vec::new();
        let push_top_first = |list: &mut Vec<CanoeElement<R>>, elements: Vec<CanoeElement<R>>| {
            list.extend(elements.into_iter().rev());
        };

        if self.lock_state != LockState::Unlocked {
            if let Some(lock) = managed.lock_surface.as_ref() {
                let elements = surface_tree_elements(
                    renderer,
                    lock.wl_surface(),
                    to_phys(output_loc),
                    scale,
                    1.0,
                    Kind::Unspecified,
                );
                push_top_first(
                    &mut bottom_up,
                    elements.into_iter().map(Into::into).collect(),
                );
            }
            let mut elements = self.cursor_elements(renderer, output, geometry, scale);
            elements.extend(bottom_up.into_iter().rev());
            return elements;
        }

        let context = self.context.clone();
        let context = context.borrow();
        let canoe_output = context.outputs.get(&output_id).cloned();

        // Background layer: canoe's fill, then wallpaper clients.
        if let Some(out) = canoe_output.as_ref() {
            let out = out.borrow();
            if let Some(background) = out.desktop_background.as_ref() {
                if let Some(origin) =
                    canoe_layer_origin(&background.surface, geometry.loc, output_size)
                {
                    bottom_up.extend(canoe_surface(
                        renderer,
                        &background.surface,
                        origin,
                        output_loc,
                        scale,
                        1.0,
                    ));
                }
            }
        }
        self.push_layer(
            renderer,
            output,
            WlrLayer::Background,
            geometry,
            scale,
            &mut bottom_up,
        );

        // Bottom layer: canoe's desktop icons, then bars and panels.
        if let Some(out) = canoe_output.as_ref() {
            let out = out.borrow();
            if let Some(desktop) = out.desktop_surface.as_ref() {
                if let Some(origin) =
                    canoe_layer_origin(&desktop.surface, geometry.loc, output_size)
                {
                    bottom_up.extend(canoe_surface(
                        renderer,
                        &desktop.surface,
                        origin,
                        output_loc,
                        scale,
                        1.0,
                    ));
                }
            }
        }
        self.push_layer(
            renderer,
            output,
            WlrLayer::Bottom,
            geometry,
            scale,
            &mut bottom_up,
        );

        // Windows. One being animated is drawn where its animation is.
        let now = std::time::Instant::now();
        for window in &self.windows {
            if window.fullscreen.is_some() {
                continue;
            }
            // Hidden windows still show while they fly into their icons.
            let view = self.warp_view(window, now);
            let iconify = view.is_some_and(|view| view.iconify);
            if window.hidden && !iconify {
                continue;
            }
            let Some(w) = context.windows.get(&window.id) else {
                continue;
            };
            let w = w.borrow();
            // Windows show once they have a size and the window manager
            // placed them.
            if w.position_undefined || window.reported_size.is_none() {
                continue;
            }
            let pos = window.pos.to_f64();
            let alpha = view.map_or(1.0, |view| view.alpha);
            let mapping = view.map(|view| FrameMapping::new(&view.frame, output_loc, scale));
            // Flying to or from its icon, the window shrinks as a whole.
            let (frame_fit, popup_fit) = if iconify {
                (Fit::Stretch, None)
            } else {
                (Fit::Slice, Some(Fit::Move))
            };
            let fit = |elements: Vec<CanoeElement<R>>, fit: Fit| match &mapping {
                Some(mapping) => elements
                    .into_iter()
                    .flat_map(|e| mapping.place(e, fit, scale))
                    .map(CanoeElement::Warped)
                    .collect(),
                None => elements,
            };
            if let Some(shadow) = w.shadow.as_ref().filter(|_| !iconify) {
                if let canoe::surface::Role::Decoration { offset, .. } = shadow.surface.role() {
                    let origin = pos + Point::<i32, Logical>::from(offset).to_f64();
                    let shadow =
                        canoe_surface(renderer, &shadow.surface, origin, output_loc, scale, 1.0);
                    bottom_up.extend(fit(shadow, Fit::Slice));
                }
            }
            let surface_origin = window.surface_origin(None).to_f64();
            let (main, popups) = surface_with_popups(
                renderer,
                window.wl_surface(),
                to_phys(surface_origin),
                scale,
                alpha,
            );
            let clip = window.content_clip;
            let main: Vec<CanoeElement<R>> = if clip.is_enabled() {
                let crop = Rectangle::<f64, Logical>::new(
                    pos + Point::from((clip.x as f64, clip.y as f64)),
                    (clip.width as f64, clip.height as f64).into(),
                );
                let crop =
                    Rectangle::new(to_phys(crop.loc), crop.size.to_physical(scale)).to_i32_round();
                main.into_iter()
                    .filter_map(|e| CropRenderElement::from_element(e, scale, crop))
                    .map(CanoeElement::Cropped)
                    .collect()
            } else {
                main.into_iter().map(Into::into).collect()
            };
            push_top_first(&mut bottom_up, fit(main, Fit::Stretch));
            if window.ssd {
                if let Some(titlebar) = w.titlebar.as_ref() {
                    if let canoe::surface::Role::Decoration { offset, .. } = titlebar.surface.role()
                    {
                        let origin = pos + Point::<i32, Logical>::from(offset).to_f64();
                        let frame = canoe_surface(
                            renderer,
                            &titlebar.surface,
                            origin,
                            output_loc,
                            scale,
                            alpha,
                        );
                        bottom_up.extend(fit(frame, frame_fit));
                    }
                }
            }
            if let Some(popup_fit) = popup_fit {
                let popups = popups.into_iter().map(Into::into).collect();
                push_top_first(&mut bottom_up, fit(popups, popup_fit));
            }
        }

        self.push_layer(
            renderer,
            output,
            WlrLayer::Top,
            geometry,
            scale,
            &mut bottom_up,
        );

        // A fullscreen window, on black.
        if let Some(window) = self
            .windows
            .iter()
            .rev()
            .find(|w| !w.hidden && w.fullscreen == Some(output_key))
        {
            let backdrop = SolidColorBuffer::new(geometry.size, [0.0, 0.0, 0.0, 1.0]);
            let backdrop_id = self
                .backdrop_ids
                .entry(output_key)
                .or_insert(backdrop)
                .clone();
            bottom_up.push(CanoeElement::Solid(SolidColorRenderElement::from_buffer(
                &backdrop_id,
                Point::<i32, Physical>::from((0, 0)),
                scale,
                1.0,
                Kind::Unspecified,
            )));
            let origin = window.surface_origin(Some(geometry.loc)).to_f64();
            let (main, popups) =
                surface_with_popups(renderer, window.wl_surface(), to_phys(origin), scale, 1.0);
            push_top_first(&mut bottom_up, main.into_iter().map(Into::into).collect());
            push_top_first(&mut bottom_up, popups.into_iter().map(Into::into).collect());
        }

        // X11 menus and tooltips, above the windows they belong to.
        for (surface, loc) in self.x11_unmanaged_surfaces() {
            let elements = surface_tree_elements(
                renderer,
                &surface,
                to_phys(loc.to_f64()),
                scale,
                1.0,
                Kind::Unspecified,
            );
            push_top_first(
                &mut bottom_up,
                elements.into_iter().map(Into::into).collect(),
            );
        }

        self.push_layer(
            renderer,
            output,
            WlrLayer::Overlay,
            geometry,
            scale,
            &mut bottom_up,
        );

        // The window manager's overlays.
        if let Some(menu) = context.window_menu.as_ref() {
            if menu.output_id == output_id {
                if let Some(origin) = canoe_layer_origin(&menu.surface, geometry.loc, output_size) {
                    bottom_up.extend(canoe_surface(
                        renderer,
                        &menu.surface,
                        origin,
                        output_loc,
                        scale,
                        1.0,
                    ));
                }
            }
        }
        if let Some(picker) = context.share_picker.as_ref() {
            for surface in picker.surfaces.iter().filter(|s| s.output_id == output_id) {
                if let Some(origin) =
                    canoe_layer_origin(&surface.surface, geometry.loc, output_size)
                {
                    bottom_up.extend(canoe_surface(
                        renderer,
                        &surface.surface,
                        origin,
                        output_loc,
                        scale,
                        1.0,
                    ));
                }
            }
        }
        drop(context);

        let mut elements = self.cursor_elements(renderer, output, geometry, scale);
        elements.extend(bottom_up.into_iter().rev());
        elements
    }

    /// Client surfaces of one layer, bottom to top.
    fn push_layer<R>(
        &self,
        renderer: &mut R,
        output: &Output,
        layer: WlrLayer,
        geometry: Rectangle<i32, Logical>,
        scale: Scale<f64>,
        bottom_up: &mut Vec<CanoeElement<R>>,
    ) where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        let map = layer_map_for_output(output);
        // layers_on() yields bottom to top.
        for surface in map.layers_on(layer) {
            let Some(layer_geo) = map.layer_geometry(surface) else {
                continue;
            };
            let location = layer_geo.loc.to_f64().to_physical(scale);
            let (main, popups) =
                surface_with_popups(renderer, surface.wl_surface(), location, scale, 1.0);
            bottom_up.extend(main.into_iter().rev().map(Into::into));
            bottom_up.extend(popups.into_iter().rev().map(Into::into));
        }
        let _ = geometry;
    }

    /// The pointer's cursor, and under it the icon of a drag going on.
    fn cursor_elements<R>(
        &mut self,
        renderer: &mut R,
        output: &Output,
        geometry: Rectangle<i32, Logical>,
        scale: Scale<f64>,
    ) -> Vec<CanoeElement<R>>
    where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        let mut elements = self.pointer_image_elements(renderer, output, geometry, scale);
        elements.extend(self.dnd_icon_elements(renderer, geometry, scale));
        elements
    }

    /// The icon a client's drag carries, beside the pointer. Drawn on every
    /// output, so one straddling an edge shows on both sides, and even where
    /// the cursor is left out or drawn by the backend: it is the drag's
    /// content, not the cursor.
    fn dnd_icon_elements<R>(
        &self,
        renderer: &mut R,
        geometry: Rectangle<i32, Logical>,
        scale: Scale<f64>,
    ) -> Vec<CanoeElement<R>>
    where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        let icon = self.dnd.as_ref().and_then(|d| d.icon.as_ref());
        let Some(icon) = icon.filter(|i| i.surface.alive()) else {
            return Vec::new();
        };
        let local = self.pointer_location - geometry.loc.to_f64() + icon.offset.to_f64();
        surface_tree_elements(
            renderer,
            &icon.surface,
            local.to_physical(scale),
            scale,
            1.0,
            Kind::Unspecified,
        )
        .into_iter()
        .map(Into::into)
        .collect()
    }

    /// The pointer's cursor, if it is on `output`.
    fn pointer_image_elements<R>(
        &mut self,
        renderer: &mut R,
        output: &Output,
        geometry: Rectangle<i32, Logical>,
        scale: Scale<f64>,
    ) -> Vec<CanoeElement<R>>
    where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        if !geometry.to_f64().contains(self.pointer_location)
            || self.cursor_hidden_by_backend
            || self.suppress_cursor
        {
            return Vec::new();
        }
        let local = self.pointer_location - geometry.loc.to_f64();

        // The window manager decides the cursor over its own surfaces,
        // during pointer operations and where no client has the pointer.
        let pointer_on_client = self
            .seat
            .get_pointer()
            .and_then(|p| p.current_focus())
            .is_some();
        let wm_controls = self.pointer_op.is_some()
            || self.pointer_canoe_target != canoe::PointerTarget::None
            || (!pointer_on_client && self.lock_state == LockState::Unlocked);

        let name = if let Some(dnd) = &self.dnd {
            // A drag shows what a drop would do, wherever it goes.
            dnd.cursor_name()
        } else if wm_controls {
            match self.wm_cursor {
                Some(shape) => shape.xcursor_name(),
                None => return Vec::new(),
            }
        } else {
            match &self.client_cursor {
                CursorImageStatus::Hidden => return Vec::new(),
                CursorImageStatus::Named(icon) => icon.name(),
                CursorImageStatus::Surface(surface) => {
                    let hotspot = with_states(surface, |states| {
                        states
                            .data_map
                            .get::<CursorImageSurfaceData>()
                            .map(|data| data.lock().unwrap().hotspot)
                            .unwrap_or_default()
                    });
                    let location = (local - hotspot.to_f64()).to_physical(scale);
                    return surface_tree_elements(
                        renderer,
                        surface,
                        location,
                        scale,
                        1.0,
                        Kind::Cursor,
                    )
                    .into_iter()
                    .map(Into::into)
                    .collect();
                }
            }
        };

        let integer = output.current_scale().integer_scale().max(1) as u32;
        let Some(frame) = self.cursor.frame(name, integer, self.start_time.elapsed()) else {
            return Vec::new();
        };
        let size = self.cursor.size() as f64;
        let factor = size / (size * integer as f64);
        let hotspot = Point::<f64, Logical>::from((
            frame.hotspot.0 as f64 * factor,
            frame.hotspot.1 as f64 * factor,
        ));
        let logical_size = (
            (frame.size.0 as f64 * factor).round() as i32,
            (frame.size.1 as f64 * factor).round() as i32,
        );
        let location = (local - hotspot).to_physical(scale);
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &frame.buffer,
            None,
            Some(Rectangle::from_size(
                (frame.size.0 as f64, frame.size.1 as f64).into(),
            )),
            Some(logical_size.into()),
            Kind::Cursor,
        )
        .ok()
        .map(|e| vec![CanoeElement::Memory(e)])
        .unwrap_or_default()
    }
}
