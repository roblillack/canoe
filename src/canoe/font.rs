//! Fontconfig-backed font selection and FreeType rasterization helpers.

use fontconfig_sys as fc;
use freetype::freetype as ft;
use std::cell::RefCell;
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

struct LoadedFont {
    query: String,
    face: ft::FT_Face,
    size_px: u32,
}

pub(super) struct FontCache {
    library: ft::FT_Library,
    loaded: Option<LoadedFont>,
    fallbacks: Vec<LoadedFont>,
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
            loaded: None,
            fallbacks: Vec::new(),
        })
    }

    fn load_primary(&mut self, query: Option<&str>, size_px: u32) -> Option<()> {
        let query = query.unwrap_or(DEFAULT_FONT_QUERY);
        let size_px = size_px.max(1);
        let needs_reload = match &self.loaded {
            Some(loaded) => loaded.query != query,
            None => true,
        };

        if needs_reload {
            let matched = match_font(query, None)?;
            let loaded = self.load_font(matched, query, size_px)?;
            self.clear_fallbacks();
            self.clear_loaded();
            self.loaded = Some(loaded);
        } else {
            let loaded = self.loaded.as_ref()?;
            if loaded.size_px != size_px {
                if unsafe { !freetype::succeeded(ft::FT_Set_Pixel_Sizes(loaded.face, 0, size_px)) }
                {
                    return None;
                }
                self.clear_fallbacks();
                self.loaded.as_mut()?.size_px = size_px;
            }
        }

        Some(())
    }

    fn load_font(&self, matched: FontMatch, query: &str, size_px: u32) -> Option<LoadedFont> {
        let c_path = CString::new(matched.path.to_string_lossy().as_bytes()).ok()?;
        let mut face: ft::FT_Face = ptr::null_mut();
        let ok = unsafe {
            freetype::succeeded(ft::FT_New_Face(
                self.library,
                c_path.as_ptr(),
                matched.index as ft::FT_Long,
                &mut face,
            ))
        };
        if !ok || face.is_null() {
            return None;
        }
        let ok = unsafe { freetype::succeeded(ft::FT_Set_Pixel_Sizes(face, 0, size_px)) };
        if !ok {
            unsafe {
                ft::FT_Done_Face(face);
            }
            return None;
        }
        Some(LoadedFont {
            query: query.to_string(),
            face,
            size_px,
        })
    }

    fn clear_loaded(&mut self) {
        if let Some(loaded) = self.loaded.take() {
            unsafe {
                ft::FT_Done_Face(loaded.face);
            }
        }
    }

    fn clear_fallbacks(&mut self) {
        for fallback in self.fallbacks.drain(..) {
            unsafe {
                ft::FT_Done_Face(fallback.face);
            }
        }
    }
}

impl Drop for FontCache {
    fn drop(&mut self) {
        self.clear_fallbacks();
        self.clear_loaded();
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
    f: impl FnOnce(&mut FontCache) -> R,
) -> Option<R> {
    FONT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let cache = cache.as_mut()?;
        cache.load_primary(query, size_px)?;
        Some(f(cache))
    })
}

impl FontCache {
    pub fn line_metrics(&self) -> Option<LineMetrics> {
        let face = self.loaded.as_ref()?.face;
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
        let face = self.face_for_char(ch)?;
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

    fn face_for_char(&mut self, ch: char) -> Option<ft::FT_Face> {
        let has_glyph = |face: ft::FT_Face| {
            !face.is_null() && unsafe { ft::FT_Get_Char_Index(face, ch as ft::FT_ULong) != 0 }
        };
        let (primary, query, size_px) = {
            let loaded = self.loaded.as_ref()?;
            (loaded.face, loaded.query.clone(), loaded.size_px)
        };
        if has_glyph(primary) {
            return Some(primary);
        }

        for fallback in &self.fallbacks {
            if has_glyph(fallback.face) {
                return Some(fallback.face);
            }
        }

        let matched = match_font(&query, Some(ch))?;
        let fallback = self.load_font(matched, &query, size_px)?;
        if !has_glyph(fallback.face) {
            unsafe {
                ft::FT_Done_Face(fallback.face);
            }
            return None;
        }

        let face = fallback.face;
        self.fallbacks.push(fallback);
        Some(face)
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
