//! Resampling of glyph bitmaps that come at a fixed size, such as the
//! bitmap strikes of color emoji fonts.

/// A bitmap whose rows are `pitch` bytes apart. A negative pitch means the
/// rows are stored bottom-up, as in FreeType.
pub struct Bitmap<'a> {
    pub data: &'a [u8],
    pub width: usize,
    pub rows: usize,
    pub pitch: isize,
    pub channels: usize,
}

impl Bitmap<'_> {
    fn sample(&self, x: usize, y: usize, channel: usize) -> u8 {
        let row = if self.pitch < 0 { self.rows - 1 - y } else { y };
        let idx = row * self.pitch.unsigned_abs() + x * self.channels + channel;
        self.data.get(idx).copied().unwrap_or(0)
    }
}

/// For each destination index along one axis, the source indices it covers
/// and how much of each, normalized to sum to 1.
fn weights(src_len: usize, dst_len: usize) -> Vec<Vec<(usize, f32)>> {
    let ratio = src_len as f32 / dst_len as f32;
    (0..dst_len)
        .map(|d| {
            let start = d as f32 * ratio;
            let end = ((d + 1) as f32 * ratio).min(src_len as f32);
            let mut taps = Vec::new();
            let mut s = start.floor() as usize;
            while (s as f32) < end && s < src_len {
                let lo = start.max(s as f32);
                let hi = end.min((s + 1) as f32);
                if hi > lo {
                    taps.push((s, hi - lo));
                }
                s += 1;
            }
            let total: f32 = taps.iter().map(|(_, w)| w).sum();
            if total > 0.0 {
                for (_, w) in &mut taps {
                    *w /= total;
                }
            }
            taps
        })
        .collect()
}

/// Resample `src` to `dst_width`×`dst_rows` by averaging the source area
/// covered by each destination pixel. Returns tightly packed rows (pitch =
/// `dst_width * channels`). Premultiplied color can be averaged channel by
/// channel, so this is correct for FreeType's BGRA bitmaps.
pub fn resample(src: &Bitmap<'_>, dst_width: usize, dst_rows: usize) -> Vec<u8> {
    let channels = src.channels;
    let mut out = vec![0u8; dst_width * dst_rows * channels];
    if src.width == 0 || src.rows == 0 || dst_width == 0 || dst_rows == 0 {
        return out;
    }

    let xw = weights(src.width, dst_width);
    let yw = weights(src.rows, dst_rows);
    let mut acc = vec![0f32; channels];
    for (dy, ytaps) in yw.iter().enumerate() {
        for (dx, xtaps) in xw.iter().enumerate() {
            acc.iter_mut().for_each(|a| *a = 0.0);
            for &(sy, wy) in ytaps {
                for &(sx, wx) in xtaps {
                    let w = wx * wy;
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += src.sample(sx, sy, c) as f32 * w;
                    }
                }
            }
            let base = (dy * dst_width + dx) * channels;
            for (c, a) in acc.iter().enumerate() {
                out[base + c] = a.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(data: &[u8], width: usize, rows: usize) -> Bitmap<'_> {
        Bitmap {
            data,
            width,
            rows,
            pitch: width as isize,
            channels: 1,
        }
    }

    #[test]
    fn halving_averages_blocks() {
        #[rustfmt::skip]
        let data = [
            0, 255, 10, 10,
            255, 0, 30, 30,
        ];
        let out = resample(&gray(&data, 4, 2), 2, 1);
        assert_eq!(out, [128, 20]);
    }

    #[test]
    fn non_integer_ratios_weight_partial_pixels() {
        // 3 → 2: the middle source pixel is split between both outputs.
        let data = [0, 90, 180];
        let out = resample(&gray(&data, 3, 1), 2, 1);
        assert_eq!(out, [30, 150]);
    }

    #[test]
    fn uniform_color_stays_uniform() {
        let px = [10u8, 20, 30, 200];
        let data: Vec<u8> = px.iter().copied().cycle().take(7 * 5 * 4).collect();
        let src = Bitmap {
            data: &data,
            width: 7,
            rows: 5,
            pitch: 28,
            channels: 4,
        };
        let out = resample(&src, 3, 2);
        assert_eq!(out.len(), 3 * 2 * 4);
        assert!(out.chunks(4).all(|c| c == px));
    }

    #[test]
    fn bottom_up_rows_are_flipped() {
        // Stored bottom-up: the first stored row is the bottom one.
        let data = [1, 2];
        let src = Bitmap {
            data: &data,
            width: 1,
            rows: 2,
            pitch: -1,
            channels: 1,
        };
        assert_eq!(resample(&src, 1, 2), [2, 1]);
    }
}
