//! The libvips operations `Templates::ImageToFields` runs on the page image before the
//! model sees it, ported from libvips 8.18 (the version in the Rails image) so that the
//! input tensor is the same: `find_trim`, `crop`, `resize` with the linear kernel
//! (`reducev`/`reduceh` with their integer `shrinkv`/`shrinkh` step, and `affine` with
//! the bilinear interpolator for enlarging), `embed` and the division by 255.
//!
//! All of them work on 8-bit images with libvips' fixed-point arithmetic, which its
//! vector (Highway) paths share, so the results are exact.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::manual_midpoint,
    clippy::many_single_char_names,
    clippy::needless_range_loop,
    reason = "ports of libvips' C arithmetic and loops, with its conversions and rounding"
)]

/// `VIPS_TRANSFORM_SCALE`: sub-pixel positions in the reduce masks.
const TRANSFORM_SCALE: i32 = 1 << 6;
/// `VIPS_INTERPOLATE_SHIFT`: bits of the fixed-point coefficients.
const INTERPOLATE_SHIFT: i32 = 12;
const INTERPOLATE_SCALE: i32 = 1 << INTERPOLATE_SHIFT;
/// `vips_resize`'s default `gap`.
const GAP: f64 = 2.0;

/// A 3-band 8-bit image (libvips' `uchar` sRGB), rows top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl Rgb {
    /// `extract_band(0, n: 3)` of an RGBA image.
    #[must_use]
    pub fn from_rgba(width: usize, height: usize, rgba: &[u8]) -> Self {
        let data = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .take(width * height)
            .flat_map(|px| [px[0], px[1], px[2]])
            .collect();
        Self {
            width,
            height,
            data,
        }
    }

    #[must_use]
    pub fn from_rgb(width: usize, height: usize, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data,
        }
    }

    fn at(&self, x: usize, y: usize) -> &[u8] {
        let i = (y * self.width + x) * 3;
        &self.data[i..i + 3]
    }

    /// `crop(left, top, width, height)` (`extract_area`) of an area inside the image.
    #[must_use]
    pub fn crop(&self, left: usize, top: usize, width: usize, height: usize) -> Self {
        let mut data = Vec::with_capacity(width * height * 3);
        for y in top..top + height {
            let start = (y * self.width + left) * 3;
            data.extend_from_slice(&self.data[start..start + width * 3]);
        }
        Self {
            width,
            height,
            data,
        }
    }

    /// `embed(x, y, width, height, background: [255, 255, 255])`: the image at `(x, y)`
    /// on a white canvas, cut to the canvas.
    #[must_use]
    pub fn embed_white(&self, x: usize, y: usize, width: usize, height: usize) -> Self {
        let mut data = vec![255u8; width * height * 3];
        for row in 0..self.height {
            let ty = y + row;
            if ty >= height {
                break;
            }
            let n = self.width.min(width.saturating_sub(x));
            if n == 0 {
                break;
            }
            let src = row * self.width * 3;
            let dst = (ty * width + x) * 3;
            data[dst..dst + n * 3].copy_from_slice(&self.data[src..src + n * 3]);
        }
        Self {
            width,
            height,
            data,
        }
    }
}

/// Why a preview can't be read as `ImageUtils.load_vips` reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PngError {
    /// Not a PNG libvips decodes (Rails fails on it).
    Invalid,
    /// A grayscale or 16-bit PNG: libvips keeps its bands and format, which the rest of
    /// `ImageToFields` doesn't handle the same way (not ported).
    Unsupported,
}

/// `ImageUtils.load_vips` of an 8-bit color PNG (palette images expanded, as libpng
/// does), without its alpha band.
pub fn load_png(data: &[u8]) -> Result<Rgb, PngError> {
    let decoded = ::image::load_from_memory_with_format(data, ::image::ImageFormat::Png)
        .map_err(|_| PngError::Invalid)?;
    let (width, height) = (decoded.width() as usize, decoded.height() as usize);
    match decoded {
        ::image::DynamicImage::ImageRgb8(rgb) => Ok(Rgb::from_rgb(width, height, rgb.into_raw())),
        ::image::DynamicImage::ImageRgba8(rgba) => Ok(Rgb::from_rgba(width, height, rgba.as_raw())),
        _ => Err(PngError::Unsupported),
    }
}

/// `find_trim(threshold: 10, background: [255, 255, 255])`: `[left, top, width, height]`
/// of the area that differs from white. libvips takes the 3x3 median of each band
/// (edges extended), marks pixels where a band differs from 255 by more than the
/// threshold, and searches the column and row profiles from each side.
#[must_use]
pub fn find_trim(image: &Rgb, threshold: f64) -> [i64; 4] {
    let (w, h) = (image.width, image.height);
    let mut columns = vec![false; w];
    let mut rows = vec![false; h];
    // The median of 9 values is below `limit` when at least 5 of them are.
    let below = |v: u8| (f64::from(v) - 255.0).abs() > threshold;
    for y in 0..h {
        let ys = [y.saturating_sub(1), y, (y + 1).min(h - 1)];
        for x in 0..w {
            let xs = [x.saturating_sub(1), x, (x + 1).min(w - 1)];
            let differs = (0..3).any(|band| {
                let mut count = 0;
                for &yy in &ys {
                    for &xx in &xs {
                        if below(image.at(xx, yy)[band]) {
                            count += 1;
                        }
                    }
                }
                count >= 5
            });
            if differs {
                columns[x] = true;
                rows[y] = true;
            }
        }
    }
    let first = |marks: &[bool]| marks.iter().position(|&m| m).unwrap_or(marks.len()) as i64;
    let last = |marks: &[bool]| marks.iter().rev().position(|&m| m).unwrap_or(marks.len()) as i64;
    let (left, right) = (first(&columns), last(&columns));
    let (top, bottom) = (first(&rows), last(&rows));
    [
        left,
        top,
        ((w as i64 - right) - left).max(0),
        ((h as i64 - bottom) - top).max(0),
    ]
}

/// `VIPS_ROUND_UINT`.
fn round_uint(value: f64) -> usize {
    (value + 0.5) as usize
}

/// `vips_reduce_get_points` for the linear kernel.
fn linear_points(shrink: f64) -> usize {
    2 * shrink.round_ties_even() as usize + 1
}

/// `vips_reduce_make_mask` for the linear kernel, as the 12-bit fixed-point table.
fn linear_masks(n_points: usize, shrink: f64) -> Vec<Vec<i32>> {
    (0..=TRANSFORM_SCALE)
        .map(|i| {
            let x = f64::from(i as f32 / TRANSFORM_SCALE as f32);
            let half = x + n_points as f64 / 2.0 - 1.0;
            let mut c: Vec<f64> = (0..n_points)
                .map(|i| {
                    let xp = ((i as f64 - half) / shrink).abs();
                    if xp < 1.0 { 1.0 - xp } else { 0.0 }
                })
                .collect();
            let sum: f64 = c.iter().sum();
            for v in &mut c {
                *v /= sum;
            }
            c.iter()
                .map(|v| i32::from((v * f64::from(INTERPOLATE_SCALE)) as i16))
                .collect()
        })
        .collect()
}

/// The mask for an input position: `sx = X * 128`, `tx = ((sx & 127) + 1) >> 1`.
fn mask_index(position: f64) -> usize {
    let s = (position * f64::from(TRANSFORM_SCALE * 2)) as i32;
    (((s & (TRANSFORM_SCALE * 2 - 1)) + 1) >> 1) as usize
}

fn fixed_round(sum: i32) -> u8 {
    ((sum + (INTERPOLATE_SCALE >> 1)) >> INTERPOLATE_SHIFT).clamp(0, 255) as u8
}

/// `vips_shrinkv(in, shrink, ceil: true)`: averages of `shrink` rows, the last ones
/// repeating the bottom row.
fn shrinkv(image: &Rgb, shrink: usize) -> Rgb {
    let height = image.height.div_ceil(shrink);
    let row = image.width * 3;
    let multiplier = ((1u64 << 32) / (256 * shrink as u64)) as u32;
    let amend = (shrink / 2) as u32;
    let mut data = Vec::with_capacity(row * height);
    for y in 0..height {
        for i in 0..row {
            let sum: u32 = (0..shrink)
                .map(|k| u32::from(image.data[(y * shrink + k).min(image.height - 1) * row + i]))
                .sum();
            data.push((sum.wrapping_add(amend).wrapping_mul(multiplier) >> 24) as u8);
        }
    }
    Rgb {
        width: image.width,
        height,
        data,
    }
}

/// `vips_shrinkh(in, shrink, ceil: true)`.
fn shrinkh(image: &Rgb, shrink: usize) -> Rgb {
    let width = image.width.div_ceil(shrink);
    let multiplier = ((1u64 << 32) / (256 * shrink as u64)) as u32;
    let amend = (shrink / 2) as u32;
    let mut data = Vec::with_capacity(width * image.height * 3);
    for y in 0..image.height {
        for x in 0..width {
            for band in 0..3 {
                let sum: u32 = (0..shrink)
                    .map(|k| u32::from(image.at((x * shrink + k).min(image.width - 1), y)[band]))
                    .sum();
                data.push((sum.wrapping_add(amend).wrapping_mul(multiplier) >> 24) as u8);
            }
        }
    }
    Rgb {
        width,
        height: image.height,
        data,
    }
}

/// Where libvips starts the regions it asks a resize for. `write_to_memory` fills the
/// canvas the resized image is embedded in tile by tile: full-width strips 16 rows high
/// (`FATSTRIP`), or 128 × 128 tiles once `shrinkv` (`SMALLTILE`) is in the pipeline.
/// `embed` asks for each tile's overlap with the image, and the reducers step their
/// sampling position (`X += hshrink`, `Y += vshrink`) from the start of each region, so
/// the rounding of those sums depends on where the regions start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regions {
    pub tile_width: usize,
    pub tile_height: usize,
    /// The image's offset in the canvas.
    pub left: usize,
    pub top: usize,
}

impl Regions {
    /// A single region: the whole image, as when it is written to memory on its own.
    #[must_use]
    pub fn whole(width: usize) -> Self {
        Self {
            tile_width: width,
            tile_height: usize::MAX,
            left: 0,
            top: 0,
        }
    }

    fn starts_column(&self, x: usize) -> bool {
        x == 0 || (x + self.left).is_multiple_of(self.tile_width)
    }

    fn starts_row(&self, y: usize) -> bool {
        y == 0 || (y + self.top).is_multiple_of(self.tile_height)
    }
}

/// `vips_reducev(in, vshrink, kernel: :linear, gap: 2.0)`, and whether its integer
/// `shrinkv` step runs.
fn reducev_shrinks(height: usize, vshrink: f64) -> (usize, usize) {
    let out = round_uint(height as f64 / vshrink);
    let int_shrink = ((height as f64 / out as f64 / GAP).floor() as usize).max(1);
    (out, int_shrink)
}

fn reducev(image: &Rgb, vshrink: f64, regions: Regions) -> Rgb {
    let (height, int_shrink) = reducev_shrinks(image.height, vshrink);
    let mut extra = height as f64 * vshrink - image.height as f64;
    let mut residual = vshrink;
    let shrunk;
    let mut image = image;
    if int_shrink > 1 {
        shrunk = shrinkv(image, int_shrink);
        image = &shrunk;
        extra /= int_shrink as f64;
        residual /= int_shrink as f64;
    }
    if residual == 1.0 {
        return image.clone();
    }
    let n = linear_points(residual);
    let masks = linear_masks(n, residual);
    let offset = (1.0 + extra) / 2.0 - 1.0;
    let pad = (n as f64 / 2.0).ceil() as i64 - 1;
    let row = image.width * 3;
    let source_row = |y: i64| (y - pad).clamp(0, image.height as i64 - 1) as usize * row;

    let mut data = Vec::with_capacity(row * height);
    let mut position = 0.0;
    for y in 0..height {
        if regions.starts_row(y) {
            position = (y as f64 + 0.5) * residual - 0.5 - offset;
        } else {
            position += residual;
        }
        let top = i64::from(position as i32);
        let mask = &masks[mask_index(position)];
        for i in 0..row {
            let sum: i32 = mask
                .iter()
                .enumerate()
                .map(|(k, c)| c * i32::from(image.data[source_row(top + k as i64) + i]))
                .sum();
            data.push(fixed_round(sum));
        }
    }
    Rgb {
        width: image.width,
        height,
        data,
    }
}

/// `vips_reduceh(in, hshrink, kernel: :linear, gap: 2.0)`.
fn reduceh(image: &Rgb, hshrink: f64, regions: Regions) -> Rgb {
    let width = round_uint(image.width as f64 / hshrink);
    let mut extra = width as f64 * hshrink - image.width as f64;
    let mut residual = hshrink;
    let int_shrink = ((image.width as f64 / width as f64 / GAP).floor() as usize).max(1);
    let shrunk;
    let mut image = image;
    if int_shrink > 1 {
        shrunk = shrinkh(image, int_shrink);
        image = &shrunk;
        residual /= int_shrink as f64;
        extra /= int_shrink as f64;
    }
    if residual == 1.0 {
        return image.clone();
    }
    let n = linear_points(residual);
    let masks = linear_masks(n, residual);
    let offset = (1.0 + extra) / 2.0 - 1.0;
    let pad = (n as f64 / 2.0).ceil() as i64 - 1;
    let source_x = |x: i64| (x - pad).clamp(0, image.width as i64 - 1) as usize;

    let mut data = Vec::with_capacity(width * image.height * 3);
    for y in 0..image.height {
        let mut position = 0.0;
        for x in 0..width {
            if regions.starts_column(x) {
                position = (x as f64 + 0.5) * residual - 0.5 - offset;
            } else {
                position += residual;
            }
            let left = i64::from(position as i32);
            let mask = &masks[mask_index(position)];
            for band in 0..3 {
                let sum: i32 = mask
                    .iter()
                    .enumerate()
                    .map(|(k, c)| c * i32::from(image.at(source_x(left + k as i64), y)[band]))
                    .sum();
                data.push(fixed_round(sum));
            }
        }
    }
    Rgb {
        width,
        height: image.height,
        data,
    }
}

/// `VIPS_ROUND_INT`.
fn round_int(value: f64) -> i64 {
    if value > 0.0 {
        (value + 0.5) as i64
    } else {
        (value - 0.5) as i64
    }
}

/// `vips_affine(in, [a, 0, 0, d], interpolate: bilinear, idx: 0.5, idy: 0.5, extend:
/// copy)`, as `vips_resize` enlarges.
fn affine(image: &Rgb, a: f64, d: f64, regions: Regions) -> Rgb {
    let det = a * d;
    let tmp = 1.0 / det;
    let (ia, id) = (tmp * d, tmp * a);
    let width = round_int(a * image.width as f64).max(0) as usize;
    let height = round_int(d * image.height as f64).max(0) as usize;
    // The input is embedded one pixel in (edges extended), and `idx -= 1`.
    let (idx, idy): (f64, f64) = (0.5 - 1.0, 0.5 - 1.0);
    let (w, h) = (image.width as i64, image.height as i64);
    let source = |x: i64, y: i64| {
        image.at(
            (x - 1).clamp(0, w - 1) as usize,
            (y - 1).clamp(0, h - 1) as usize,
        )
    };

    let mut data = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        let iy = id * y as f64 - idy;
        let mut ix = 0.0;
        for x in 0..width {
            if regions.starts_column(x) {
                ix = ia * x as f64 - idx;
            } else {
                ix += ia;
            }
            let (fx, fy) = (ix.floor() as i64, iy.floor() as i64);
            if (0..=w).contains(&fx) && (0..=h).contains(&fy) {
                let (x0, y0) = (i64::from(ix as i32), i64::from(iy as i32));
                let xf = ((ix - x0 as f64) * f64::from(INTERPOLATE_SCALE)) as i32;
                let yf = ((iy - y0 as f64) * f64::from(INTERPOLATE_SCALE)) as i32;
                let yd = INTERPOLATE_SCALE - yf;
                let c4 = (yf * xf) >> INTERPOLATE_SHIFT;
                let c2 = (yd * xf) >> INTERPOLATE_SHIFT;
                let c3 = yf - c4;
                let c1 = yd - c2;
                let (p1, p2) = (source(x0, y0), source(x0 + 1, y0));
                let (p3, p4) = (source(x0, y0 + 1), source(x0 + 1, y0 + 1));
                for band in 0..3 {
                    let v = c1 * i32::from(p1[band])
                        + c2 * i32::from(p2[band])
                        + c3 * i32::from(p3[band])
                        + c4 * i32::from(p4[band])
                        + (INTERPOLATE_SCALE >> 1);
                    data.push((v >> INTERPOLATE_SHIFT) as u8);
                }
            } else {
                data.extend_from_slice(&[0, 0, 0]);
            }
        }
    }
    Rgb {
        width,
        height,
        data,
    }
}

/// `image.resize(scale, vscale: scale, kernel: :linear)`, computed in `regions`.
#[must_use]
pub fn resize(image: &Rgb, scale: f64, regions: Regions) -> Rgb {
    let hscale = scale.max(1.0 / image.width as f64);
    let vscale = scale.max(1.0 / image.height as f64);
    let mut out = image.clone();
    if vscale < 1.0 {
        out = reducev(&out, 1.0 / vscale, regions);
    }
    if hscale < 1.0 {
        out = reduceh(&out, 1.0 / hscale, regions);
    }
    if hscale > 1.0 || vscale > 1.0 {
        out = affine(
            &out,
            if hscale > 1.0 { hscale } else { 1.0 },
            if vscale > 1.0 { vscale } else { 1.0 },
            regions,
        );
    }
    out
}

/// The regions of `resize(image, scale)` embedded at `(left, top)` in a `canvas` wide
/// image that is written to memory.
#[must_use]
pub fn embedded_regions(
    image: &Rgb,
    scale: f64,
    canvas: usize,
    left: usize,
    top: usize,
) -> Regions {
    let vscale = scale.max(1.0 / image.height as f64);
    let small_tiles = vscale < 1.0 && reducev_shrinks(image.height, 1.0 / vscale).1 > 1;
    let (tile_width, tile_height) = if small_tiles {
        (128, 128)
    } else {
        (canvas, 16)
    };
    Regions {
        tile_width,
        tile_height,
        left,
        top,
    }
}

/// `image / 255.0` written to memory (`linear(1 / 255.0, 0)`, single precision),
/// transposed to the model's `[3, height, width]` layout.
#[must_use]
pub fn to_planar_f32(image: &Rgb) -> Vec<f32> {
    let a = (1.0f64 / 255.0) as f32;
    let plane = image.width * image.height;
    let mut out = vec![0f32; plane * 3];
    for (i, px) in image.data.as_chunks::<3>().0.iter().enumerate() {
        for band in 0..3 {
            out[band * plane + i] = a * f32::from(px[band]) + 0.0;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: usize, height: usize) -> Rgb {
        let data = (0..width * height)
            .flat_map(|i| {
                let (x, y) = (i % width, i / width);
                [
                    (x * 7 % 256) as u8,
                    (y * 5 % 256) as u8,
                    ((x + y) % 256) as u8,
                ]
            })
            .collect();
        Rgb::from_rgb(width, height, data)
    }

    #[test]
    fn trims_to_content() {
        let mut image = Rgb::from_rgb(40, 30, vec![255; 40 * 30 * 3]);
        for y in 10..15 {
            for x in 5..20 {
                let i = (y * 40 + x) * 3;
                image.data[i..i + 3].copy_from_slice(&[0, 0, 0]);
            }
        }
        // A single dark pixel is noise: the median removes it.
        image.data[(25 * 40 + 35) * 3] = 0;
        assert_eq!(find_trim(&image, 10.0), [5, 10, 15, 5]);
        let blank = Rgb::from_rgb(8, 6, vec![255; 8 * 6 * 3]);
        assert_eq!(find_trim(&blank, 10.0), [8, 6, 0, 0]);
    }

    #[test]
    fn resizes_to_vips_sizes() {
        let image = gradient(816, 1056);
        let ratio = 704.0 / 1056.0;
        let out = resize(&image, ratio, Regions::whole(816));
        assert_eq!((out.width, out.height), (544, 704));
        let small = gradient(300, 200);
        let up = resize(&small, 704.0 / 300.0, Regions::whole(704));
        assert_eq!((up.width, up.height), (704, 469));
        let tall = gradient(700, 2400);
        let down = resize(&tall, 704.0 / 2400.0, Regions::whole(205));
        assert_eq!((down.width, down.height), (205, 704));
    }

    #[test]
    fn keeps_flat_images_flat() {
        let image = Rgb::from_rgb(50, 70, vec![255; 50 * 70 * 3]);
        for scale in [0.3, 0.7, 1.6, 3.2] {
            let out = resize(&image, scale, Regions::whole(200));
            assert!(out.data.iter().all(|&v| v == 255), "scale {scale}");
        }
    }
}
