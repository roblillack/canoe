//! Fontconfig-backed font selection and FreeType rasterization helpers.

use fontconfig_sys as fc;
use freetype::freetype as ft;
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

const DEFAULT_FONT_QUERY: &str = "sans";

struct FontMatch {
    path: PathBuf,
    index: i32,
}

/// A FreeType face set to a specific pixel size. Must be dropped before the
/// `FT_Library` it was created from.
struct LoadedFont {
    face: ft::FT_Face,
    size_px: u32,
}

impl LoadedFont {
    fn open(library: ft::FT_Library, matched: FontMatch, size_px: u32) -> Option<Self> {
        let c_path = CString::new(matched.path.to_string_lossy().as_bytes()).ok()?;
        let mut face: ft::FT_Face = ptr::null_mut();
        let ok = unsafe {
            freetype::succeeded(ft::FT_New_Face(
                library,
                c_path.as_ptr(),
                matched.index as ft::FT_Long,
                &mut face,
            ))
        };
        if !ok || face.is_null() {
            return None;
        }
        let mut font = LoadedFont { face, size_px: 0 };
        font.set_size(size_px).then_some(font)
    }

    fn set_size(&mut self, size_px: u32) -> bool {
        if self.size_px == size_px {
            return true;
        }
        let ok = unsafe { freetype::succeeded(ft::FT_Set_Pixel_Sizes(self.face, 0, size_px)) };
        if ok {
            self.size_px = size_px;
        }
        ok
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

/// The primary font for a fontconfig query plus the fallback fonts that were
/// discovered for glyphs it lacks.
struct FontSet {
    query: String,
    primary: LoadedFont,
    fallbacks: Vec<LoadedFont>,
    /// Characters for which no fallback font could be found. Remembered so
    /// that unrenderable characters don't trigger a fontconfig match on
    /// every draw; they are rendered with the primary face instead.
    missing: HashSet<char>,
}

impl FontSet {
    fn load(library: ft::FT_Library, query: &str, size_px: u32) -> Option<Self> {
        let primary = LoadedFont::open(library, match_font(query, None)?, size_px)?;
        Some(Self {
            query: query.to_string(),
            primary,
            fallbacks: Vec::new(),
            missing: HashSet::new(),
        })
    }

    fn set_size(&mut self, size_px: u32) -> bool {
        if !self.primary.set_size(size_px) {
            return false;
        }
        // Fallback faces that cannot be resized (e.g. bitmap-only fonts
        // without a matching strike) are dropped and will be looked up again
        // on demand.
        self.fallbacks
            .retain_mut(|fallback| fallback.set_size(size_px));
        true
    }

    fn face_for_char(&mut self, library: ft::FT_Library, ch: char) -> ft::FT_Face {
        if self.primary.has_glyph(ch) {
            return self.primary.face;
        }
        if let Some(fallback) = self.fallbacks.iter().find(|f| f.has_glyph(ch)) {
            return fallback.face;
        }

        // Characters no font can provide are drawn with the primary face,
        // which renders its .notdef glyph ("tofu") so they remain visible.
        // Control characters never go through fallback matching: fontconfig
        // would happily pick some random font that maps them.
        if ch.is_control() || self.missing.contains(&ch) {
            return self.primary.face;
        }
        match self.load_fallback(library, ch) {
            Some(face) => face,
            None => {
                self.missing.insert(ch);
                self.primary.face
            }
        }
    }

    fn load_fallback(&mut self, library: ft::FT_Library, ch: char) -> Option<ft::FT_Face> {
        let matched = match_font(&self.query, Some(ch))?;
        let fallback = LoadedFont::open(library, matched, self.primary.size_px)?;
        if !fallback.has_glyph(ch) {
            return None;
        }
        let face = fallback.face;
        self.fallbacks.push(fallback);
        Some(face)
    }
}

struct FontCache {
    library: ft::FT_Library,
    current: Option<FontSet>,
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
            current: None,
        })
    }

    fn font_set(&mut self, query: Option<&str>, size_px: u32) -> Option<&mut FontSet> {
        let query = query.unwrap_or(DEFAULT_FONT_QUERY);
        let size_px = size_px.max(1);
        let needs_reload = match &self.current {
            Some(set) => set.query != query,
            None => true,
        };
        if needs_reload {
            self.current = Some(FontSet::load(self.library, query, size_px)?);
        }
        let set = self.current.as_mut()?;
        set.set_size(size_px).then_some(set)
    }
}

impl Drop for FontCache {
    fn drop(&mut self) {
        // All faces must be released before the library.
        self.current = None;
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

/// The primary face for one font query and size, falling back to other
/// fonts for glyphs it doesn't provide.
pub struct FaceHandle<'a> {
    library: ft::FT_Library,
    set: &'a mut FontSet,
}

impl FaceHandle<'_> {
    pub fn line_metrics(&self) -> Option<LineMetrics> {
        let face = self.set.primary.face;
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

    pub fn load_char(&mut self, ch: char) -> Option<GlyphBitmap<'_>> {
        let face = self.set.face_for_char(self.library, ch);
        let ok = unsafe {
            freetype::succeeded(ft::FT_Load_Char(
                face,
                ch as ft::FT_ULong,
                ft::FT_LOAD_RENDER as ft::FT_Int32,
            ))
        };
        if !ok {
            return None;
        }

        unsafe {
            let glyph = (*face).glyph;
            if glyph.is_null() {
                return None;
            }
            let bitmap = &(*glyph).bitmap;
            let pitch = bitmap.pitch;
            let rows = bitmap.rows as i32;
            let width = bitmap.width as i32;
            let abs_pitch = pitch.unsigned_abs() as usize;
            let len = abs_pitch.saturating_mul(rows.max(0) as usize);
            if width == 0 || rows == 0 || len == 0 {
                return Some(GlyphBitmap {
                    width,
                    rows,
                    pitch,
                    buffer: &[],
                    left: (*glyph).bitmap_left,
                    top: (*glyph).bitmap_top,
                    advance: ((*glyph).advance.x >> 6) as i32,
                });
            }
            if bitmap.pixel_mode != ft::FT_Pixel_Mode::FT_PIXEL_MODE_GRAY as u8 {
                return None;
            }
            if bitmap.buffer.is_null() {
                return None;
            }
            let buffer = std::slice::from_raw_parts(bitmap.buffer, len);

            Some(GlyphBitmap {
                width,
                rows,
                pitch,
                buffer,
                left: (*glyph).bitmap_left,
                top: (*glyph).bitmap_top,
                advance: ((*glyph).advance.x >> 6) as i32,
            })
        }
    }
}

pub struct LineMetrics {
    pub ascender: i32,
    pub descender: i32,
}

pub struct GlyphBitmap<'a> {
    pub width: i32,
    pub rows: i32,
    pub pitch: i32,
    pub buffer: &'a [u8],
    pub left: i32,
    pub top: i32,
    pub advance: i32,
}

pub fn measure_text(query: Option<&str>, font_size: f32, text: &str) -> Option<f32> {
    let size_px = font_size.round().max(1.0) as u32;
    with_face(query, size_px, |face| {
        let mut width = 0i32;
        for ch in text.chars() {
            if let Some(glyph) = face.load_char(ch) {
                width += glyph.advance;
            }
        }
        width as f32
    })
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
