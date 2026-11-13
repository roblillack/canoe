//! Surfaces of canoe's own: window decorations, the desktop, menus, and the
//! screen-sharing picker.
//!
//! These used to be Wayland surfaces canoe drew into shared memory and
//! committed to River. Now canoe composites them itself, so a [`Surface`]
//! is an in-process buffer with the same double-buffered life cycle — attach
//! pixels, damage, commit — and the compositor reads the committed state when
//! it renders or routes pointer input. Placement is given by a role: a
//! [`Decoration`] sits at an offset from its window, a [`LayerSurface`] is
//! anchored to an output like a wlr-layer-shell surface.

#![allow(dead_code)]

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::solid::SolidColorBuffer;
use smithay::utils::{Buffer as BufferCoords, Rectangle, Transform};

use super::render::Renderer;
use super::OutputId;

/// Pixels canoe drew: ARGB8888, premultiplied, little endian (the `wl_shm`
/// format), with rows `width * 4` bytes apart.
///
/// They are kept in the [`MemoryRenderBuffer`]s the compositor draws them
/// from, so canoe draws straight into what it shows, and committing a
/// surface only tells the compositor which parts changed. Clones refer to the
/// same pixels.
///
/// A buffer can leave out a hole that stays transparent: a window's frame
/// and shadow span the whole window but only draw around it, so they keep
/// just the pieces above, below, left and right of the window.
#[derive(Clone)]
pub struct Buffer {
    width: i32,
    height: i32,
    /// What is left out, within the buffer; never empty.
    hole: Option<Rectangle<i32, BufferCoords>>,
    pieces: Rc<[Piece]>,
}

/// Part of a [`Buffer`]'s pixels.
#[derive(Clone)]
pub struct Piece {
    /// Where the piece is, in buffer pixels.
    pub rect: Rectangle<i32, BufferCoords>,
    pub memory: MemoryRenderBuffer,
}

impl Piece {
    fn new(rect: Rectangle<i32, BufferCoords>) -> Self {
        Self {
            rect,
            memory: MemoryRenderBuffer::new(
                Fourcc::Argb8888,
                rect.size,
                1,
                Transform::Normal,
                None,
            ),
        }
    }
}

impl Buffer {
    /// A transparent buffer; `None` for an empty or absurd size.
    pub fn new(width: i32, height: i32) -> Option<Self> {
        Self::with_hole(width, height, None)
    }

    /// A transparent buffer that leaves out `hole`, as far as it is inside;
    /// `None` for an empty or absurd size.
    pub fn with_hole(
        width: i32,
        height: i32,
        hole: Option<Rectangle<i32, BufferCoords>>,
    ) -> Option<Self> {
        if width <= 0 || height <= 0 {
            return None;
        }
        let len = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if len > i32::MAX as usize {
            return None;
        }
        let full = Rectangle::<i32, BufferCoords>::from_size((width, height).into());
        let hole = hole
            .and_then(|hole| hole.intersection(full))
            .filter(|hole| !hole.is_empty());
        let pieces: Rc<[Piece]> = match hole {
            None => Rc::new([Piece::new(full)]),
            Some(hole) => {
                let (x0, y0) = (hole.loc.x, hole.loc.y);
                let (x1, y1) = (x0 + hole.size.w, y0 + hole.size.h);
                [
                    (0, 0, width, y0),
                    (0, y1, width, height - y1),
                    (0, y0, x0, y1 - y0),
                    (x1, y0, width - x1, y1 - y0),
                ]
                .into_iter()
                .filter(|&(_, _, w, h)| w > 0 && h > 0)
                .map(|(x, y, w, h)| Piece::new(Rectangle::new((x, y).into(), (w, h).into())))
                .collect()
            }
        };
        Some(Self {
            width,
            height,
            hole,
            pieces,
        })
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    /// The part left out, if any.
    pub fn hole(&self) -> Option<Rectangle<i32, BufferCoords>> {
        self.hole
    }

    pub fn pieces(&self) -> &[Piece] {
        &self.pieces
    }

    /// Hand the pixels of a buffer without a hole to `f`. What it changes
    /// shows once the parts it changed are committed as damage.
    pub fn draw<T>(&self, f: impl FnOnce(&mut [u8]) -> T) -> T {
        debug_assert!(self.hole.is_none(), "a buffer with a hole is painted");
        let mut memory = self.pieces[0].memory.clone();
        let mut context = memory.render();
        let mut result = None;
        let _ = context.draw(|pixels| {
            result = Some(f(pixels));
            Ok::<_, Infallible>(Vec::new())
        });
        result.expect("the closure is always called")
    }

    /// Paint the buffer: `f` draws into each piece in turn, in the whole
    /// buffer's coordinates, and each piece takes what falls on it.
    pub fn paint(&self, mut f: impl FnMut(&mut Renderer)) {
        let size = (self.width, self.height);
        for piece in self.pieces.iter() {
            let rect = piece.rect;
            let mut memory = piece.memory.clone();
            let mut context = memory.render();
            let _ = context.draw(|pixels| {
                let at = (rect.loc.x, rect.loc.y, rect.size.w, rect.size.h);
                if let Some(mut renderer) = Renderer::piece(pixels, at, size) {
                    f(&mut renderer);
                }
                Ok::<_, Infallible>(Vec::new())
            });
        }
    }

    /// The pixels, put together, with the hole transparent.
    #[cfg(test)]
    pub fn to_vec(&self) -> Vec<u8> {
        let stride = self.width as usize * 4;
        let mut pixels = vec![0; stride * self.height as usize];
        for piece in self.pieces.iter() {
            let rect = piece.rect;
            let mut memory = piece.memory.clone();
            let _ = memory.render().draw(|src| {
                let row_bytes = rect.size.w as usize * 4;
                for (row, src_row) in src.chunks_exact(row_bytes).enumerate() {
                    let at = (rect.loc.y as usize + row) * stride + rect.loc.x as usize * 4;
                    pixels[at..at + row_bytes].copy_from_slice(src_row);
                }
                Ok::<_, Infallible>(Vec::new())
            });
        }
        pixels
    }

    /// Tell the compositor these parts changed, for it to take them up.
    fn damage(&self, damage: &[Rectangle<i32, BufferCoords>]) {
        for piece in self.pieces.iter() {
            let rect = piece.rect;
            let damage: Vec<_> = damage
                .iter()
                .filter_map(|damage| damage.intersection(rect))
                .map(|damage| Rectangle::new(damage.loc - rect.loc, damage.size))
                .collect();
            if damage.is_empty() {
                continue;
            }
            let mut memory = piece.memory.clone();
            let mut context = memory.render();
            let _ = context.draw(|_| Ok::<_, Infallible>(damage));
        }
    }
}

/// A buffer that is redrawn in place, sized on demand. Stands in for the
/// double-buffered shm pools a Wayland client needs: canoe draws into the
/// pixels the compositor shows, so one buffer suffices.
#[derive(Default)]
pub struct PixelPool {
    buffer: Option<Buffer>,
    allocations: u32,
}

impl PixelPool {
    pub fn new(_name: &'static str) -> Self {
        Self::default()
    }

    /// How often a buffer had to be allocated.
    pub fn allocate_fresh_count(&self) -> u32 {
        self.allocations
    }

    /// Make the buffer `width`×`height`, leaving out `hole`. False if it
    /// can't be.
    pub fn prepare(
        &mut self,
        width: i32,
        height: i32,
        hole: Option<Rectangle<i32, BufferCoords>>,
    ) -> bool {
        let Some(wanted) = Buffer::with_hole(width, height, hole).map(|b| (b.hole, b)) else {
            return false;
        };
        let fits = self
            .buffer
            .as_ref()
            .is_some_and(|b| b.width() == width && b.height() == height && b.hole() == wanted.0);
        if !fits {
            self.buffer = Some(wanted.1);
            self.allocations += 1;
        }
        true
    }

    pub fn mark_attached(&self) {}

    /// Paint the prepared buffer, if there is one; see [`Buffer::paint`].
    pub fn paint(&self, f: impl FnMut(&mut Renderer)) -> bool {
        self.buffer.as_ref().map(|b| b.paint(f)).is_some()
    }

    pub fn current_buffer(&self) -> Option<&Buffer> {
        self.buffer.as_ref()
    }

    pub fn is_ready(&self) -> bool {
        self.buffer.is_some()
    }

    pub fn destroy(&mut self) {
        self.buffer = None;
    }
}

/// An area made of rectangles added and subtracted in order, in surface
/// coordinates (logical pixels).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Region {
    ops: Vec<(bool, (i32, i32, i32, i32))>,
}

impl Region {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, x: i32, y: i32, width: i32, height: i32) {
        self.ops.push((true, (x, y, width, height)));
    }

    pub fn subtract(&mut self, x: i32, y: i32, width: i32, height: i32) {
        self.ops.push((false, (x, y, width, height)));
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        let mut inside = false;
        for &(add, (rx, ry, rw, rh)) in &self.ops {
            if x >= rx && x < rx + rw && y >= ry && y < ry + rh {
                inside = add;
            }
        }
        inside
    }
}

/// The wlr-layer-shell layer a [`LayerSurface`] is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    Background,
    Bottom,
    Top,
    Overlay,
}

bitflags::bitflags! {
    /// Output edges a [`LayerSurface`] is anchored to.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Anchor: u32 {
        const Top = 1;
        const Bottom = 2;
        const Left = 4;
        const Right = 8;
    }
}

/// Whether a layer surface takes keyboard focus. Canoe's never do; key
/// presses meant for them arrive as bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardInteractivity {
    None,
}

/// How a surface is placed.
#[derive(Debug, Clone, PartialEq)]
pub enum Role {
    None,
    /// Drawn below (`above == false`) or above a window's content, `offset`
    /// logical pixels from the content's top left corner.
    Decoration {
        above: bool,
        offset: (i32, i32),
    },
    /// Anchored to an output.
    Layer(LayerPlacement),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayerPlacement {
    pub output_id: Option<OutputId>,
    pub layer: Layer,
    pub namespace: String,
    pub anchor: Anchor,
    /// top, right, bottom, left
    pub margin: (i32, i32, i32, i32),
    /// Requested size; zero stretches between opposite anchors.
    pub size: (u32, u32),
    pub exclusive_zone: i32,
    /// The size last configured, or `None` before the first configure.
    pub configured: Option<(i32, i32)>,
    /// Creation order, for stacking within a layer.
    pub serial: u64,
}

impl LayerPlacement {
    /// The surface's box within an output of `output_size`, in output-local
    /// logical coordinates, following the wlr-layer-shell rules.
    pub fn geometry(&self, output_size: (i32, i32)) -> (i32, i32, i32, i32) {
        let (ow, oh) = output_size;
        let (mt, mr, mb, ml) = self.margin;
        let stretch_x = self.anchor.contains(Anchor::Left | Anchor::Right);
        let stretch_y = self.anchor.contains(Anchor::Top | Anchor::Bottom);
        let width = if self.size.0 == 0 && stretch_x {
            (ow - ml - mr).max(0)
        } else {
            self.size.0 as i32
        };
        let height = if self.size.1 == 0 && stretch_y {
            (oh - mt - mb).max(0)
        } else {
            self.size.1 as i32
        };
        let x = if self.anchor.contains(Anchor::Left) {
            ml
        } else if self.anchor.contains(Anchor::Right) {
            ow - width - mr
        } else {
            (ow - width) / 2
        };
        let y = if self.anchor.contains(Anchor::Top) {
            mt
        } else if self.anchor.contains(Anchor::Bottom) {
            oh - height - mb
        } else {
            (oh - height) / 2
        };
        (x, y, width, height)
    }
}

static NEXT_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct State {
    // Pending state, applied by commit().
    pending_buffer: Option<Option<Buffer>>,
    pending_damage: Vec<Rectangle<i32, BufferCoords>>,
    pending_input_region: Option<Option<Region>>,
    pending_buffer_scale: i32,
    pending_destination: Option<(i32, i32)>,

    // Committed state, what the compositor shows.
    buffer: Option<Buffer>,
    /// A one-pixel buffer, kept as its color: it is drawn as a solid fill
    /// rather than stretched (which would blur its edges).
    solid: Option<SolidColorBuffer>,
    buffer_scale: i32,
    destination: Option<(i32, i32)>,
    input_region: Option<Region>,
    commits: u64,

    role: Role,
    destroyed: bool,
}

/// A surface of canoe's own. Clones refer to the same surface.
#[derive(Clone)]
pub struct Surface(Rc<RefCell<State>>);

impl PartialEq for Surface {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Surface({:p})", Rc::as_ptr(&self.0))
    }
}

impl Default for Surface {
    fn default() -> Self {
        Self::new()
    }
}

/// What the compositor needs to draw a committed surface.
pub struct SurfaceContent {
    /// The pixels, in one piece unless the buffer has a hole.
    pub pieces: Vec<Piece>,
    /// Set for a one-pixel buffer: its color, to fill the surface with.
    pub solid: Option<SolidColorBuffer>,
    /// Size in device pixels.
    pub buffer_size: (i32, i32),
    /// Size in logical pixels.
    pub logical_size: (i32, i32),
    pub commits: u64,
}

impl Surface {
    pub fn new() -> Self {
        Self(Rc::new(RefCell::new(State {
            pending_buffer: None,
            pending_damage: Vec::new(),
            pending_input_region: None,
            pending_buffer_scale: 1,
            pending_destination: None,
            buffer: None,
            solid: None,
            buffer_scale: 1,
            destination: None,
            input_region: None,
            commits: 0,
            role: Role::None,
            destroyed: false,
        })))
    }

    /// Attach `buffer` for the next commit; `None` unmaps the surface.
    pub fn attach(&self, buffer: Option<&Buffer>, _x: i32, _y: i32) {
        self.0.borrow_mut().pending_buffer = Some(buffer.cloned());
    }

    /// Mark part of the attached buffer as changed, in buffer pixels.
    pub fn damage_buffer(&self, x: i32, y: i32, width: i32, height: i32) {
        if width > 0 && height > 0 {
            self.0
                .borrow_mut()
                .pending_damage
                .push(Rectangle::new((x, y).into(), (width, height).into()));
        }
    }

    /// Where the surface takes pointer input; `None` means everywhere.
    pub fn set_input_region(&self, region: Option<&Region>) {
        self.0.borrow_mut().pending_input_region = Some(region.cloned());
    }

    pub fn set_buffer_scale(&self, scale: i32) {
        self.0.borrow_mut().pending_buffer_scale = scale.max(1);
    }

    /// Present the buffer at `width`×`height` logical pixels, whatever its
    /// size in device pixels (what `wp_viewport.set_destination` does).
    pub fn set_destination(&self, width: i32, height: i32) {
        self.0.borrow_mut().pending_destination = Some((width, height));
    }

    /// Apply the pending state.
    pub fn commit(&self) {
        let mut state = self.0.borrow_mut();
        if state.destroyed {
            return;
        }
        state.buffer_scale = state.pending_buffer_scale;
        if let Some(destination) = state.pending_destination.take() {
            state.destination = Some(destination);
        }
        if let Some(region) = state.pending_input_region.take() {
            state.input_region = region;
        }
        let damage = std::mem::take(&mut state.pending_damage);
        match state.pending_buffer.take() {
            Some(None) => {
                state.buffer = None;
                state.solid = None;
            }
            Some(Some(buffer)) => {
                let size = (buffer.width(), buffer.height());
                if size == (1, 1) {
                    // Premultiplied BGRA bytes.
                    let [b, g, r, a] =
                        buffer.draw(|pixels| [pixels[0], pixels[1], pixels[2], pixels[3]]);
                    let color = [
                        r as f32 / 255.0,
                        g as f32 / 255.0,
                        b as f32 / 255.0,
                        a as f32 / 255.0,
                    ];
                    match state.solid.as_mut() {
                        Some(solid) => solid.update((1, 1), color),
                        None => state.solid = Some(SolidColorBuffer::new((1, 1), color)),
                    }
                } else {
                    state.solid = None;
                }
                let full = Rectangle::from_size(size.into());
                let damage: Vec<_> = if damage.is_empty() {
                    vec![full]
                } else {
                    damage
                        .into_iter()
                        .filter_map(|rect| rect.intersection(full))
                        .collect()
                };
                buffer.damage(&damage);
                state.buffer = Some(buffer);
            }
            None => {}
        }
        state.commits += 1;
    }

    /// Stop showing the surface for good.
    pub fn destroy(&self) {
        let mut state = self.0.borrow_mut();
        state.destroyed = true;
        state.buffer = None;
    }

    /// The committed content, if the surface is mapped.
    pub fn content(&self) -> Option<SurfaceContent> {
        let state = self.0.borrow();
        let buffer = state.buffer.as_ref()?;
        let (bw, bh) = (buffer.width(), buffer.height());
        let logical_size = state.destination.unwrap_or((
            bw / state.buffer_scale.max(1),
            bh / state.buffer_scale.max(1),
        ));
        Some(SurfaceContent {
            pieces: buffer.pieces().to_vec(),
            solid: state.solid.clone(),
            buffer_size: (bw, bh),
            logical_size,
            commits: state.commits,
        })
    }

    pub fn is_mapped(&self) -> bool {
        self.0.borrow().buffer.is_some()
    }

    /// The committed size in logical pixels.
    pub fn logical_size(&self) -> Option<(i32, i32)> {
        self.content().map(|content| content.logical_size)
    }

    /// Whether a point in surface coordinates (logical pixels) takes input.
    pub fn accepts_input_at(&self, x: i32, y: i32) -> bool {
        let Some((width, height)) = self.logical_size() else {
            return false;
        };
        if x < 0 || y < 0 || x >= width || y >= height {
            return false;
        }
        match self.0.borrow().input_region {
            Some(ref region) => region.contains(x, y),
            None => true,
        }
    }

    pub fn role(&self) -> Role {
        self.0.borrow().role.clone()
    }

    fn set_role(&self, role: Role) {
        self.0.borrow_mut().role = role;
    }

    fn update_role(&self, f: impl FnOnce(&mut Role)) {
        f(&mut self.0.borrow_mut().role);
    }
}

/// A window decoration: a surface drawn with a window, below or above its
/// content.
pub struct Decoration {
    surface: Surface,
}

impl Decoration {
    pub fn above(surface: &Surface) -> Self {
        surface.set_role(Role::Decoration {
            above: true,
            offset: (0, 0),
        });
        Self {
            surface: surface.clone(),
        }
    }

    pub fn below(surface: &Surface) -> Self {
        surface.set_role(Role::Decoration {
            above: false,
            offset: (0, 0),
        });
        Self {
            surface: surface.clone(),
        }
    }

    /// Where the surface's corner sits relative to the window content's.
    pub fn set_offset(&self, x: i32, y: i32) {
        self.surface.update_role(|role| {
            if let Role::Decoration { offset, .. } = role {
                *offset = (x, y);
            }
        });
    }

    /// Decoration changes show with the window state they belong to, since
    /// the compositor only renders after a whole render sequence.
    pub fn sync_next_commit(&self) {}

    pub fn destroy(&self) {
        self.surface.destroy();
    }
}

/// A surface anchored to an output, like a wlr-layer-shell surface.
pub struct LayerSurface {
    surface: Surface,
}

impl LayerSurface {
    pub fn new(
        surface: &Surface,
        output_id: Option<OutputId>,
        layer: Layer,
        namespace: &str,
    ) -> Self {
        surface.set_role(Role::Layer(LayerPlacement {
            output_id,
            layer,
            namespace: namespace.to_string(),
            anchor: Anchor::empty(),
            margin: (0, 0, 0, 0),
            size: (0, 0),
            exclusive_zone: 0,
            configured: None,
            serial: NEXT_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }));
        Self {
            surface: surface.clone(),
        }
    }

    fn placement(&self, f: impl FnOnce(&mut LayerPlacement)) {
        self.surface.update_role(|role| {
            if let Role::Layer(placement) = role {
                f(placement);
            }
        });
    }

    pub fn set_anchor(&self, anchor: Anchor) {
        self.placement(|p| p.anchor = anchor);
    }

    pub fn set_margin(&self, top: i32, right: i32, bottom: i32, left: i32) {
        self.placement(|p| p.margin = (top, right, bottom, left));
    }

    pub fn set_size(&self, width: u32, height: u32) {
        self.placement(|p| p.size = (width, height));
    }

    pub fn set_exclusive_zone(&self, zone: i32) {
        self.placement(|p| p.exclusive_zone = zone);
    }

    pub fn set_keyboard_interactivity(&self, _interactivity: KeyboardInteractivity) {}

    pub fn ack_configure(&self, _serial: u32) {}

    /// Lay the surface out on an output of `output_size` logical pixels and
    /// return its size, the way a layer-shell `configure` event would.
    pub fn configure(&self, output_size: (i32, i32)) -> (i32, i32) {
        let mut size = (0, 0);
        self.placement(|p| {
            let (_, _, w, h) = p.geometry(output_size);
            p.configured = Some((w, h));
            size = (w, h);
        });
        size
    }

    pub fn output_id(&self) -> Option<OutputId> {
        match self.surface.role() {
            Role::Layer(p) => p.output_id,
            _ => None,
        }
    }

    pub fn destroy(&self) {
        self.surface.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_apply_operations_in_order() {
        let mut region = Region::new();
        region.add(0, 0, 100, 50);
        region.subtract(10, 10, 80, 30);
        assert!(region.contains(5, 5));
        assert!(!region.contains(50, 20));
        assert!(region.contains(50, 45));
        assert!(!region.contains(100, 0));
    }

    #[test]
    fn layer_geometry_follows_anchors_and_margins() {
        let mut p = LayerPlacement {
            output_id: None,
            layer: Layer::Overlay,
            namespace: String::new(),
            anchor: Anchor::Top | Anchor::Left,
            margin: (20, 0, 0, 30),
            size: (100, 50),
            exclusive_zone: -1,
            configured: None,
            serial: 0,
        };
        assert_eq!(p.geometry((800, 600)), (30, 20, 100, 50));
        p.anchor = Anchor::all();
        p.size = (0, 0);
        p.margin = (0, 0, 0, 0);
        assert_eq!(p.geometry((800, 600)), (0, 0, 800, 600));
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, BufferCoords> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn a_buffer_keeps_the_pieces_around_its_hole() {
        let pieces = |hole| {
            let buffer = Buffer::with_hole(10, 8, Some(hole)).unwrap();
            buffer.pieces().iter().map(|p| p.rect).collect::<Vec<_>>()
        };
        assert_eq!(
            pieces(rect(2, 3, 5, 2)),
            [
                rect(0, 0, 10, 3),
                rect(0, 5, 10, 3),
                rect(0, 3, 2, 2),
                rect(7, 3, 3, 2)
            ]
        );
        // A hole reaching past the edges is cut to the buffer.
        assert_eq!(pieces(rect(-4, 2, 20, 10)), [rect(0, 0, 10, 2)]);
        // One outside it leaves the buffer whole.
        assert_eq!(pieces(rect(10, 0, 4, 4)), [rect(0, 0, 10, 8)]);
        assert!(Buffer::with_hole(10, 8, Some(rect(10, 0, 4, 4)))
            .unwrap()
            .hole()
            .is_none());
    }

    #[test]
    fn painting_draws_into_every_piece() {
        let buffer = Buffer::with_hole(6, 4, Some(rect(1, 1, 4, 2))).unwrap();
        buffer.paint(|renderer| renderer.fill_rect(0, 0, 6, 4, 0xff112233));
        let pixels = buffer.to_vec();
        for y in 0..4 {
            for x in 0..6 {
                let at = (y * 6 + x) * 4;
                let px = u32::from_ne_bytes(pixels[at..at + 4].try_into().unwrap());
                let in_hole = (1..5).contains(&x) && (1..3).contains(&y);
                assert_eq!(px, if in_hole { 0 } else { 0xff112233 }, "({x}, {y})");
            }
        }
    }

    #[test]
    fn committed_surfaces_take_input_inside_their_region() {
        let surface = Surface::new();
        let buffer = Buffer::new(20, 10).unwrap();
        surface.attach(Some(&buffer), 0, 0);
        let mut region = Region::new();
        region.add(0, 0, 10, 10);
        surface.set_input_region(Some(&region));
        assert!(!surface.accepts_input_at(1, 1));
        surface.commit();
        assert!(surface.accepts_input_at(1, 1));
        assert!(!surface.accepts_input_at(15, 1));
        surface.attach(None, 0, 0);
        surface.commit();
        assert!(!surface.is_mapped());
    }
}
