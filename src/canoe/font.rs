//! Fontconfig-backed font selection, rustybuzz text shaping and FreeType
//! rasterization.
//!
//! Text is split into clusters ([`segment`]), each cluster is assigned a
//! font (the configured one, a fallback that covers it, or a color emoji
//! font when it asks for emoji presentation), runs of clusters sharing a
//! font are shaped with rustybuzz, and the resulting glyphs are rasterized
//! by FreeType.

mod resample;
mod segment;

use fontconfig_sys as fc;
use freetype::freetype as ft;
use memmap2::Mmap;
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::ops::Range;
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

const DEFAULT_FONT_QUERY: &str = "sans";

/// Fontconfig's generic family for color emoji fonts.
const EMOJI_FONT_QUERY: &str = "emoji";

/// Upper bound for the number of cached font queries. In practice only a
/// couple are in use (titlebar font, icon label font); the limit just keeps
/// stale entries from piling up across config reloads.
const MAX_FONT_SETS: usize = 8;

/// Upper bound for the per-font-set glyph caches (advances and bitmaps).
/// Window titles use a small set of glyphs; when the caches grow past this,
/// they are simply dropped and refilled on demand.
const MAX_CACHED_GLYPHS: usize = 2048;

#[derive(Clone, PartialEq, Eq)]
struct FontMatch {
    path: PathBuf,
    index: i32,
}

/// A font file mapped into memory and opened both by FreeType (for
/// rasterization) and rustybuzz (for shaping), set to a specific pixel
/// size. Must be dropped before the `FT_Library` it was created from.
struct LoadedFont {
    source: FontMatch,
    face: ft::FT_Face,
    /// `None` for formats rustybuzz cannot parse (e.g. PCF bitmap fonts);
    /// text in those fonts is laid out character by character.
    shaper: Option<rustybuzz::Face<'static>>,
    /// Backing storage for `face` and `shaper`. Declared after `shaper` so
    /// it is dropped after it; `face` is released in `Drop`.
    _data: Mmap,
    size_px: u32,
    /// Whether the font has color glyphs (CBDT/sbix bitmaps, COLR layers).
    color: bool,
    /// Factor that brings glyphs rendered at the selected bitmap strike to
    /// `size_px`. Only differs from 1 for color fonts that cannot be scaled
    /// by FreeType; monochrome bitmap fonts are drawn at their native size.
    bitmap_scale: f32,
    /// Pixels per font unit, for converting shaper output.
    px_per_unit: f32,
}

impl LoadedFont {
    fn open(library: ft::FT_Library, matched: FontMatch, size_px: u32) -> Option<Self> {
        let file = File::open(&matched.path).ok()?;
        // SAFETY: font files are not expected to change while in use;
        // FreeType makes the same assumption when it maps them itself.
        let data = unsafe { Mmap::map(&file) }.ok()?;
        let mut face: ft::FT_Face = ptr::null_mut();
        let ok = unsafe {
            freetype::succeeded(ft::FT_New_Memory_Face(
                library,
                data.as_ptr(),
                data.len() as ft::FT_Long,
                matched.index as ft::FT_Long,
                &mut face,
            ))
        };
        if !ok || face.is_null() {
            return None;
        }
        // SAFETY: the mapping is owned by the returned `LoadedFont`, never
        // moves, and outlives `shaper` (see the field order).
        let bytes: &'static [u8] = unsafe { std::slice::from_raw_parts(data.as_ptr(), data.len()) };
        // The upper bits of a fontconfig index select a named instance of a
        // variable font, which FreeType handles; rustybuzz only wants the
        // face index within the collection.
        let shaper = rustybuzz::Face::from_slice(bytes, (matched.index & 0xFFFF) as u32);
        let color = unsafe { (*face).face_flags & ft::FT_FACE_FLAG_COLOR as ft::FT_Long != 0 };
        let mut font = LoadedFont {
            source: matched,
            face,
            shaper,
            _data: data,
            size_px: 0,
            color,
            bitmap_scale: 1.0,
            px_per_unit: 0.0,
        };
        font.set_size(size_px).then_some(font)
    }

    fn set_size(&mut self, size_px: u32) -> bool {
        if self.size_px == size_px {
            return true;
        }
        let face = self.face;
        let ok = unsafe { freetype::succeeded(ft::FT_Set_Pixel_Sizes(face, 0, size_px)) }
            || self.select_strike(size_px);
        if !ok {
            return false;
        }

        let (ppem, units_per_em) = unsafe {
            let size = (*face).size;
            let ppem = if size.is_null() {
                size_px as f32
            } else {
                (*size).metrics.y_ppem as f32
            };
            (ppem.max(1.0), (*face).units_per_EM as f32)
        };
        self.bitmap_scale = if self.color {
            size_px as f32 / ppem
        } else {
            1.0
        };
        self.px_per_unit = if units_per_em > 0.0 {
            ppem * self.bitmap_scale / units_per_em
        } else {
            0.0
        };
        self.size_px = size_px;
        true
    }

    /// Select one of the fixed bitmap sizes of a font that cannot be
    /// scaled (e.g. a color emoji font with a single 109px strike).
    fn select_strike(&self, size_px: u32) -> bool {
        let strikes: Vec<u32> = unsafe {
            let count = (*self.face).num_fixed_sizes.max(0) as usize;
            let sizes = (*self.face).available_sizes;
            if sizes.is_null() {
                return false;
            }
            (0..count)
                .map(|i| {
                    let size = &*sizes.add(i);
                    let ppem = (size.y_ppem + 32) >> 6;
                    if ppem > 0 {
                        ppem as u32
                    } else {
                        size.height.max(0) as u32
                    }
                })
                .collect()
        };
        let Some(index) = choose_strike(&strikes, size_px, self.color) else {
            return false;
        };
        unsafe { freetype::succeeded(ft::FT_Select_Size(self.face, index as ft::FT_Int)) }
    }

    fn has_glyph(&self, ch: char) -> bool {
        unsafe { ft::FT_Get_Char_Index(self.face, ch as ft::FT_ULong) != 0 }
    }
}

impl Drop for LoadedFont {
    fn drop(&mut self) {
        unsafe {
            ft::FT_Done_Face(self.face);
        }
    }
}

/// Pick the bitmap strike to use for `size_px`. Color fonts get scaled
/// afterwards, so the smallest strike at least as large as requested is
/// best (only ever scaling down); if none is large enough, the largest.
/// Monochrome bitmap fonts are drawn unscaled, so the closest size wins.
fn choose_strike(strikes: &[u32], size_px: u32, color: bool) -> Option<usize> {
    let indexed = strikes.iter().copied().enumerate();
    if color {
        indexed
            .clone()
            .filter(|&(_, ppem)| ppem >= size_px)
            .min_by_key(|&(_, ppem)| ppem)
            .or_else(|| indexed.max_by_key(|&(_, ppem)| ppem))
            .map(|(i, _)| i)
    } else {
        indexed
            .min_by_key(|&(_, ppem)| ppem.abs_diff(size_px))
            .map(|(i, _)| i)
    }
}

/// A rasterized glyph, with tightly packed top-down rows. Glyphs of color
/// fonts with a fixed strike are stored scaled to the requested size.
struct CachedGlyph {
    width: i32,
    rows: i32,
    pixel_mode: GlyphPixelMode,
    buffer: Vec<u8>,
    left: i32,
    top: i32,
}

/// A glyph as returned by the shaper, in font units.
#[derive(Default)]
struct ShapedGlyph {
    glyph_id: u32,
    advance: i32,
    /// How much the shaper changed the glyph's nominal advance.
    adjustment: i32,
    x_offset: i32,
    y_offset: i32,
}

/// Cache key for per-glyph data: font index in the set, glyph id, size.
type GlyphKey = (usize, u32, u32);

/// The primary font for a fontconfig query plus the fallback fonts that were
/// discovered for glyphs it lacks.
struct FontSet {
    query: String,
    /// The primary font first, then fallbacks in the order they were found.
    fonts: Vec<LoadedFont>,
    /// Characters for which no fallback font could be found. Remembered so
    /// that unrenderable characters don't trigger a fontconfig match on
    /// every draw; they are rendered with the primary font instead.
    missing: HashSet<char>,
    /// Characters for which no color emoji font could be found.
    no_emoji_font: HashSet<char>,
    /// Characters no monochrome font provides, and the (color) font
    /// fontconfig suggested for them instead.
    color_only: HashMap<char, usize>,
    /// Hinted advances in pixels.
    advances: HashMap<GlyphKey, f32>,
    bitmaps: HashMap<GlyphKey, CachedGlyph>,
}

impl FontSet {
    fn load(library: ft::FT_Library, query: &str, size_px: u32) -> Option<Self> {
        let primary = LoadedFont::open(library, match_font(query, None)?, size_px)?;
        Some(Self {
            query: query.to_string(),
            fonts: vec![primary],
            missing: HashSet::new(),
            no_emoji_font: HashSet::new(),
            color_only: HashMap::new(),
            advances: HashMap::new(),
            bitmaps: HashMap::new(),
        })
    }

    fn size_px(&self) -> u32 {
        self.fonts[0].size_px
    }

    fn set_size(&mut self, size_px: u32) -> bool {
        if !self.fonts[0].set_size(size_px) {
            return false;
        }
        // Fallback fonts that cannot be resized are dropped and will be
        // looked up again on demand. Cache keys hold font indices, so the
        // caches go with them.
        let before = self.fonts.len();
        let mut index = 0;
        self.fonts.retain_mut(|font| {
            index += 1;
            index == 1 || font.set_size(size_px)
        });
        if self.fonts.len() != before {
            self.color_only.clear();
            self.advances.clear();
            self.bitmaps.clear();
        }
        true
    }

    /// The font to draw `cluster` with.
    fn font_for_cluster(&mut self, library: ft::FT_Library, cluster: &str) -> usize {
        let Some(base) = cluster.chars().next() else {
            return 0;
        };
        if segment::wants_emoji(cluster) {
            if let Some(index) = self.emoji_font_for_char(library, base) {
                return index;
            }
        }
        self.font_for_char(library, base)
    }

    fn emoji_font_for_char(&mut self, library: ft::FT_Library, ch: char) -> Option<usize> {
        if let Some(index) = self
            .fonts
            .iter()
            .position(|font| font.color && font.has_glyph(ch))
        {
            return Some(index);
        }
        if self.no_emoji_font.contains(&ch) {
            return None;
        }
        let index = match_font(EMOJI_FONT_QUERY, Some(ch))
            .and_then(|matched| self.font_index(library, matched))
            .filter(|&i| self.fonts[i].color && self.fonts[i].has_glyph(ch));
        if index.is_none() {
            self.no_emoji_font.insert(ch);
        }
        index
    }

    fn font_for_char(&mut self, library: ft::FT_Library, ch: char) -> usize {
        // Text presentation: color fonts are only used for characters no
        // monochrome font has, otherwise an emoji font loaded for 😀 would
        // also take over ❤ or ☃ from the text font.
        if let Some(index) = self
            .fonts
            .iter()
            .position(|font| !font.color && font.has_glyph(ch))
        {
            return index;
        }
        if let Some(&index) = self.color_only.get(&ch) {
            return index;
        }

        // Characters no font can provide are drawn with the primary font,
        // which renders its .notdef glyph ("tofu") so they remain visible.
        // Control characters never go through fallback matching: fontconfig
        // would happily pick some random font that maps them.
        if ch.is_control() || self.missing.contains(&ch) {
            return 0;
        }
        let fallback = match_font(&self.query, Some(ch))
            .and_then(|matched| self.font_index(library, matched))
            .filter(|&i| self.fonts[i].has_glyph(ch));
        match fallback {
            Some(index) => {
                if self.fonts[index].color {
                    self.color_only.insert(ch, index);
                }
                index
            }
            None => {
                self.missing.insert(ch);
                0
            }
        }
    }

    /// Index of the font fontconfig matched, loading it if needed.
    fn font_index(&mut self, library: ft::FT_Library, matched: FontMatch) -> Option<usize> {
        if let Some(index) = self.fonts.iter().position(|font| font.source == matched) {
            return Some(index);
        }
        let font = LoadedFont::open(library, matched, self.size_px())?;
        self.fonts.push(font);
        Some(self.fonts.len() - 1)
    }

    fn trim_caches(&mut self) {
        if self.advances.len() > MAX_CACHED_GLYPHS {
            self.advances.clear();
        }
        if self.bitmaps.len() > MAX_CACHED_GLYPHS {
            self.bitmaps.clear();
        }
    }

    fn layout(&mut self, library: ft::FT_Library, text: &str) -> TextLayout {
        self.trim_caches();

        // Group clusters into runs sharing a font.
        let mut runs: Vec<(usize, Range<usize>)> = Vec::new();
        for cluster in segment::clusters(text) {
            let font = self.font_for_cluster(library, &text[cluster.clone()]);
            match runs.last_mut() {
                Some((last_font, range)) if *last_font == font => range.end = cluster.end,
                _ => runs.push((font, cluster)),
            }
        }

        let mut layout = TextLayout {
            glyphs: Vec::new(),
            width: 0.0,
        };
        for (font, range) in runs {
            self.shape_run(font, &text[range], &mut layout);
        }
        layout
    }

    fn shape_run(&mut self, font: usize, text: &str, layout: &mut TextLayout) {
        let mut glyphs: Vec<ShapedGlyph> = Vec::new();
        let px_per_unit = self.fonts[font].px_per_unit;
        match &self.fonts[font].shaper {
            Some(shaper) => {
                let mut buffer = rustybuzz::UnicodeBuffer::new();
                buffer.push_str(text);
                buffer.guess_segment_properties();
                let shaped = rustybuzz::shape(shaper, &[], buffer);
                for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
                    let nominal = shaper
                        .glyph_hor_advance(rustybuzz::ttf_parser::GlyphId(info.glyph_id as u16))
                        .unwrap_or(0) as i32;
                    glyphs.push(ShapedGlyph {
                        glyph_id: info.glyph_id,
                        advance: pos.x_advance,
                        adjustment: pos.x_advance - nominal,
                        x_offset: pos.x_offset,
                        y_offset: pos.y_offset,
                    });
                }
            }
            None => {
                let face = self.fonts[font].face;
                for ch in text.chars() {
                    let glyph_id = unsafe { ft::FT_Get_Char_Index(face, ch as ft::FT_ULong) };
                    glyphs.push(ShapedGlyph {
                        glyph_id,
                        ..ShapedGlyph::default()
                    });
                }
            }
        }

        for glyph in glyphs {
            // Shaped advances are unhinted, while FreeType renders hinted
            // glyphs. To keep the familiar hinted spacing, start from
            // FreeType's advance and only apply what the shaper changed
            // (kerning, ligatures, ...). Glyphs the shaper made zero-width
            // (marks, default-ignorables like ZWJ) stay zero-width.
            let advance = if glyph.advance == 0 && glyph.adjustment != 0 {
                0.0
            } else {
                self.hinted_advance(font, glyph.glyph_id) + glyph.adjustment as f32 * px_per_unit
            };
            layout.glyphs.push(PlacedGlyph {
                font,
                glyph_id: glyph.glyph_id,
                x: layout.width,
                x_offset: glyph.x_offset as f32 * px_per_unit,
                y_offset: glyph.y_offset as f32 * px_per_unit,
                advance,
            });
            layout.width += advance;
        }
    }

    fn hinted_advance(&mut self, font: usize, glyph_id: u32) -> f32 {
        let key = (font, glyph_id, self.size_px());
        if let Some(&advance) = self.advances.get(&key) {
            return advance;
        }
        let LoadedFont {
            face, bitmap_scale, ..
        } = self.fonts[font];
        let advance = unsafe {
            if freetype::succeeded(ft::FT_Load_Glyph(
                face,
                glyph_id,
                (ft::FT_LOAD_DEFAULT | ft::FT_LOAD_COLOR) as ft::FT_Int32,
            )) && !(*face).glyph.is_null()
            {
                (*(*face).glyph).advance.x as f32 / 64.0 * bitmap_scale
            } else {
                0.0
            }
        };
        self.advances.insert(key, advance);
        advance
    }

    /// Rasterizing is by far the most expensive part of drawing text, so
    /// every glyph is rendered once per size and then served from cache.
    fn glyph_bitmap(&mut self, glyph: &PlacedGlyph) -> Option<GlyphBitmap<'_>> {
        let key = (glyph.font, glyph.glyph_id, self.size_px());
        if let Entry::Vacant(entry) = self.bitmaps.entry(key) {
            let LoadedFont {
                face, bitmap_scale, ..
            } = self.fonts[glyph.font];
            // SAFETY: the slot bitmap is copied before the next glyph load.
            let bitmap = unsafe { render_glyph(face, glyph.glyph_id)? };
            entry.insert(if bitmap_scale == 1.0 {
                copy_bitmap(&bitmap)
            } else {
                scale_bitmap(&bitmap, bitmap_scale)
            });
        }
        let cached = self.bitmaps.get(&key)?;
        Some(GlyphBitmap {
            width: cached.width,
            rows: cached.rows,
            pitch: cached.width * cached.pixel_mode.bytes_per_pixel() as i32,
            pixel_mode: cached.pixel_mode,
            buffer: &cached.buffer,
            left: cached.left,
            top: cached.top,
        })
    }
}

/// Render `glyph_id` into `face`'s glyph slot and return the bitmap.
///
/// # Safety
///
/// `face` must be valid, and the returned bitmap must not be used after the
/// next glyph load on `face`.
unsafe fn render_glyph<'a>(face: ft::FT_Face, glyph_id: u32) -> Option<GlyphBitmap<'a>> {
    let ok = freetype::succeeded(ft::FT_Load_Glyph(
        face,
        glyph_id,
        (ft::FT_LOAD_RENDER | ft::FT_LOAD_COLOR) as ft::FT_Int32,
    ));
    if !ok {
        return None;
    }
    slot_bitmap(face)
}

/// The bitmap FreeType just rendered into `face`'s glyph slot.
///
/// # Safety
///
/// `face` must be valid, and the returned bitmap must not be used after the
/// next glyph load on `face`.
unsafe fn slot_bitmap<'a>(face: ft::FT_Face) -> Option<GlyphBitmap<'a>> {
    let glyph = (*face).glyph;
    if glyph.is_null() {
        return None;
    }
    let bitmap = &(*glyph).bitmap;
    let pitch = bitmap.pitch;
    let rows = bitmap.rows as i32;
    let width = bitmap.width as i32;
    let left = (*glyph).bitmap_left;
    let top = (*glyph).bitmap_top;
    let len = (pitch.unsigned_abs() as usize).saturating_mul(rows.max(0) as usize);
    if width == 0 || rows == 0 || len == 0 {
        return Some(GlyphBitmap {
            width: 0,
            rows: 0,
            pitch,
            pixel_mode: GlyphPixelMode::Gray,
            buffer: &[],
            left,
            top,
        });
    }
    let pixel_mode = match bitmap.pixel_mode {
        mode if mode == ft::FT_Pixel_Mode::FT_PIXEL_MODE_GRAY as u8 => GlyphPixelMode::Gray,
        mode if mode == ft::FT_Pixel_Mode::FT_PIXEL_MODE_BGRA as u8 => GlyphPixelMode::Bgra,
        _ => return None,
    };
    if bitmap.buffer.is_null() {
        return None;
    }
    Some(GlyphBitmap {
        width,
        rows,
        pitch,
        pixel_mode,
        buffer: std::slice::from_raw_parts(bitmap.buffer, len),
        left,
        top,
    })
}

/// Copy a glyph slot bitmap into a cache entry, packing the rows top-down.
fn copy_bitmap(bitmap: &GlyphBitmap<'_>) -> CachedGlyph {
    let row_len = bitmap.width.max(0) as usize * bitmap.pixel_mode.bytes_per_pixel();
    let rows = bitmap.rows.max(0) as usize;
    let abs_pitch = bitmap.pitch.unsigned_abs() as usize;
    let mut buffer = Vec::with_capacity(row_len * rows);
    for row in 0..rows {
        let src_row = if bitmap.pitch < 0 {
            rows - 1 - row
        } else {
            row
        };
        let start = src_row * abs_pitch;
        match bitmap.buffer.get(start..start + row_len) {
            Some(src) => buffer.extend_from_slice(src),
            None => buffer.resize(buffer.len() + row_len, 0),
        }
    }
    CachedGlyph {
        width: bitmap.width.max(0),
        rows: rows as i32,
        pixel_mode: bitmap.pixel_mode,
        buffer,
        left: bitmap.left,
        top: bitmap.top,
    }
}

fn scale_bitmap(bitmap: &GlyphBitmap<'_>, scale: f32) -> CachedGlyph {
    let left = (bitmap.left as f32 * scale).round() as i32;
    let top = (bitmap.top as f32 * scale).round() as i32;
    if bitmap.width <= 0 || bitmap.rows <= 0 {
        return CachedGlyph {
            width: 0,
            rows: 0,
            pixel_mode: bitmap.pixel_mode,
            buffer: Vec::new(),
            left,
            top,
        };
    }
    let width = ((bitmap.width as f32 * scale).round() as i32).max(1);
    let rows = ((bitmap.rows as f32 * scale).round() as i32).max(1);
    let buffer = resample::resample(
        &resample::Bitmap {
            data: bitmap.buffer,
            width: bitmap.width as usize,
            rows: bitmap.rows as usize,
            pitch: bitmap.pitch as isize,
            channels: bitmap.pixel_mode.bytes_per_pixel(),
        },
        width as usize,
        rows as usize,
    );
    CachedGlyph {
        width,
        rows,
        pixel_mode: bitmap.pixel_mode,
        buffer,
        left,
        top,
    }
}

struct FontCache {
    library: ft::FT_Library,
    sets: HashMap<String, FontSet>,
}

impl FontCache {
    fn new() -> Option<Self> {
        let mut library: ft::FT_Library = ptr::null_mut();
        let ok = unsafe { freetype::succeeded(ft::FT_Init_FreeType(&mut library)) };
        if !ok || library.is_null() {
            return None;
        }
        Some(Self {
            library,
            sets: HashMap::new(),
        })
    }

    fn font_set(&mut self, query: Option<&str>, size_px: u32) -> Option<&mut FontSet> {
        let query = query.unwrap_or(DEFAULT_FONT_QUERY);
        let size_px = size_px.max(1);
        if !self.sets.contains_key(query) {
            let set = FontSet::load(self.library, query, size_px)?;
            if self.sets.len() >= MAX_FONT_SETS {
                self.sets.clear();
            }
            self.sets.insert(query.to_string(), set);
        }
        let set = self.sets.get_mut(query)?;
        set.set_size(size_px).then_some(set)
    }
}

impl Drop for FontCache {
    fn drop(&mut self) {
        // All faces must be released before the library.
        self.sets.clear();
        if !self.library.is_null() {
            unsafe {
                ft::FT_Done_FreeType(self.library);
            }
        }
    }
}

thread_local! {
    static FONT_CACHE: RefCell<Option<FontCache>> = RefCell::new(FontCache::new());
}

pub fn with_face<R>(
    query: Option<&str>,
    size_px: u32,
    f: impl FnOnce(&mut FaceHandle<'_>) -> R,
) -> Option<R> {
    FONT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let cache = cache.as_mut()?;
        let library = cache.library;
        let set = cache.font_set(query, size_px)?;
        Some(f(&mut FaceHandle { library, set }))
    })
}

/// The fonts for one font query and size: the primary font plus the
/// fallbacks used for glyphs it doesn't provide.
pub struct FaceHandle<'a> {
    library: ft::FT_Library,
    set: &'a mut FontSet,
}

impl FaceHandle<'_> {
    pub fn line_metrics(&self) -> Option<LineMetrics> {
        let face = self.set.fonts[0].face;
        unsafe {
            let size = (*face).size;
            if size.is_null() {
                return None;
            }
            let metrics = (*size).metrics;
            Some(LineMetrics {
                ascender: (metrics.ascender >> 6) as i32,
                descender: (metrics.descender >> 6) as i32,
            })
        }
    }

    /// Shape `text` into positioned glyphs.
    pub fn layout(&mut self, text: &str) -> TextLayout {
        self.set.layout(self.library, text)
    }

    /// Rasterize a glyph from [`FaceHandle::layout`].
    pub fn glyph_bitmap(&mut self, glyph: &PlacedGlyph) -> Option<GlyphBitmap<'_>> {
        self.set.glyph_bitmap(glyph)
    }
}

pub struct LineMetrics {
    pub ascender: i32,
    pub descender: i32,
}

/// A glyph positioned on the baseline, in pixels from the start of the
/// text. `x` is the pen position; the offsets are where the shaper moved
/// the glyph from there (e.g. for marks). Positive `y_offset` is up.
pub struct PlacedGlyph {
    font: usize,
    glyph_id: u32,
    pub x: f32,
    pub x_offset: f32,
    pub y_offset: f32,
    pub advance: f32,
}

pub struct TextLayout {
    pub glyphs: Vec<PlacedGlyph>,
    /// Total advance width in pixels.
    pub width: f32,
}

/// Pixel format of a rasterized glyph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GlyphPixelMode {
    /// One coverage byte per pixel, to be tinted with the text color.
    Gray,
    /// Premultiplied BGRA color pixels (e.g. color emoji).
    Bgra,
}

impl GlyphPixelMode {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            GlyphPixelMode::Gray => 1,
            GlyphPixelMode::Bgra => 4,
        }
    }
}

pub struct GlyphBitmap<'a> {
    pub width: i32,
    pub rows: i32,
    pub pitch: i32,
    pub pixel_mode: GlyphPixelMode,
    pub buffer: &'a [u8],
    pub left: i32,
    pub top: i32,
}

/// Width of `text` in pixels, as drawn by [`super::render::Renderer::render_text`].
pub fn measure_text(query: Option<&str>, font_size: f32, text: &str) -> Option<f32> {
    let size_px = font_size.round().max(1.0) as u32;
    with_face(query, size_px, |face| face.layout(text).width.round())
}

/// Byte ranges of the clusters in `text`: the units text can be cut at
/// without breaking up emoji sequences or detaching combining marks.
pub fn clusters(text: &str) -> Vec<Range<usize>> {
    segment::clusters(text)
}

fn match_font(pattern: &str, ch: Option<char>) -> Option<FontMatch> {
    let path = Path::new(pattern);
    let is_file = path.is_file();
    if ch.is_none() && is_file {
        return Some(FontMatch {
            path: path.to_path_buf(),
            index: 0,
        });
    }
    // let fontconfig decide
    let pattern = if is_file { DEFAULT_FONT_QUERY } else { pattern };

    let config = fontconfig_config()?;
    let c_pattern = CString::new(pattern).ok()?;
    unsafe {
        let pat = fc::FcNameParse(c_pattern.as_ptr() as *const fc::FcChar8);
        if pat.is_null() {
            return None;
        }
        fc::FcConfigSubstitute(config, pat, fc::FcMatchPattern);
        fc::FcDefaultSubstitute(pat);
        if let Some(ch) = ch {
            let charset = fc::FcCharSetCreate();
            let added = !charset.is_null()
                && fc::FcCharSetAddChar(charset, ch as fc::FcChar32) != 0
                && fc::FcPatternAddCharSet(pat, fc::constants::FC_CHARSET.as_ptr(), charset) != 0;

            if !charset.is_null() {
                fc::FcCharSetDestroy(charset);
            }
            if !added {
                fc::FcPatternDestroy(pat);
                return None;
            }
        }
        let mut result = fc::FcResultMatch;
        let font = fc::FcFontMatch(config, pat, &mut result);
        fc::FcPatternDestroy(pat);
        if font.is_null() {
            return None;
        }

        let mut file_ptr: *mut fc::FcChar8 = ptr::null_mut();
        let file_result =
            fc::FcPatternGetString(font, fc::constants::FC_FILE.as_ptr(), 0, &mut file_ptr);
        if file_result != fc::FcResultMatch || file_ptr.is_null() {
            fc::FcPatternDestroy(font);
            return None;
        }

        let mut index: c_int = 0;
        if fc::FcPatternGetInteger(font, fc::constants::FC_INDEX.as_ptr(), 0, &mut index)
            != fc::FcResultMatch
        {
            index = 0;
        }

        let path = CStr::from_ptr(file_ptr as *const c_char)
            .to_string_lossy()
            .into_owned();
        fc::FcPatternDestroy(font);

        Some(FontMatch {
            path: PathBuf::from(path),
            index,
        })
    }
}

fn fontconfig_config() -> Option<*mut fc::FcConfig> {
    #[derive(Clone, Copy)]
    struct ConfigPtr(*mut fc::FcConfig);

    unsafe impl Send for ConfigPtr {}
    unsafe impl Sync for ConfigPtr {}

    static CONFIG: OnceLock<ConfigPtr> = OnceLock::new();
    let config = CONFIG.get_or_init(|| unsafe { ConfigPtr(fc::FcInitLoadConfigAndFonts()) });
    if config.0.is_null() {
        None
    } else {
        Some(config.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_strikes_prefer_scaling_down() {
        let strikes = [20, 32, 40, 64, 96, 160];
        assert_eq!(choose_strike(&strikes, 15, true), Some(0));
        assert_eq!(choose_strike(&strikes, 33, true), Some(2));
        assert_eq!(choose_strike(&strikes, 40, true), Some(2));
        assert_eq!(choose_strike(&strikes, 200, true), Some(5));
        assert_eq!(choose_strike(&[109], 15, true), Some(0));
        assert_eq!(choose_strike(&[], 15, true), None);
    }

    #[test]
    fn copied_bitmaps_are_packed_top_down() {
        // Two rows of two pixels with padding, stored bottom-up.
        let data = [3, 4, 0, 1, 2, 0];
        let bitmap = GlyphBitmap {
            width: 2,
            rows: 2,
            pitch: -3,
            pixel_mode: GlyphPixelMode::Gray,
            buffer: &data,
            left: 1,
            top: 2,
        };
        let copy = copy_bitmap(&bitmap);
        assert_eq!(copy.buffer, [1, 2, 3, 4]);
        assert_eq!((copy.width, copy.rows, copy.left, copy.top), (2, 2, 1, 2));
    }

    #[test]
    fn monochrome_strikes_pick_the_closest_size() {
        let strikes = [12, 14, 18, 24];
        assert_eq!(choose_strike(&strikes, 15, false), Some(1));
        assert_eq!(choose_strike(&strikes, 17, false), Some(2));
        assert_eq!(choose_strike(&strikes, 100, false), Some(3));
    }

    /// Shape `text` with the default font at `size_px`, returning the glyph
    /// count and whether any glyph is a color bitmap. `None` if no font
    /// could be loaded at all.
    fn shape(text: &str, size_px: u32) -> Option<(usize, bool, f32)> {
        with_face(None, size_px, |face| {
            let layout = face.layout(text);
            let mut color = false;
            for glyph in &layout.glyphs {
                if let Some(bitmap) = face.glyph_bitmap(glyph) {
                    color |= bitmap.pixel_mode == GlyphPixelMode::Bgra;
                }
            }
            (layout.glyphs.len(), color, layout.width)
        })
    }

    fn color_emoji_available() -> bool {
        matches!(shape("😀", 15), Some((_, true, _)))
    }

    #[test]
    fn emoji_presentation_uses_a_color_font() {
        if !color_emoji_available() {
            eprintln!("no color emoji font installed, skipping");
            return;
        }
        // Text presentation by default, emoji presentation when asked.
        assert!(!shape("❤", 15).unwrap().1);
        assert!(shape("❤\u{FE0F}", 15).unwrap().1);
        assert!(!shape("😀\u{FE0E}", 15).unwrap().1);
    }

    #[test]
    fn emoji_sequences_shape_to_single_glyphs() {
        if !color_emoji_available() {
            eprintln!("no color emoji font installed, skipping");
            return;
        }
        for sequence in ["👨\u{200D}👩\u{200D}👧", "👍🏽", "🇩🇪", "1\u{FE0F}\u{20E3}"]
        {
            let (glyphs, color, _) = shape(sequence, 15).unwrap();
            assert!(color, "{sequence:?} not drawn in color");
            // Default-ignorables may survive shaping as zero-width glyphs;
            // only one glyph may take up space.
            let visible = with_face(None, 15, |face| {
                face.layout(sequence)
                    .glyphs
                    .iter()
                    .filter(|g| g.advance > 0.0)
                    .count()
            })
            .unwrap();
            assert_eq!(visible, 1, "{sequence:?} shaped into {glyphs} glyphs");
        }
    }

    #[test]
    fn color_emoji_are_scaled_to_the_font_size() {
        if !color_emoji_available() {
            eprintln!("no color emoji font installed, skipping");
            return;
        }
        for size_px in [12, 15, 30] {
            let (_, _, width) = shape("😀", size_px).unwrap();
            let rows = with_face(None, size_px, |face| {
                let layout = face.layout("😀");
                face.glyph_bitmap(&layout.glyphs[0]).map(|b| b.rows)
            })
            .flatten()
            .unwrap();
            let size = size_px as f32;
            assert!(
                (size * 0.9..=size * 1.5).contains(&width),
                "advance {width} at {size_px}px"
            );
            assert!(
                (size * 0.9..=size * 1.4).contains(&(rows as f32)),
                "height {rows} at {size_px}px"
            );
        }
    }
}
