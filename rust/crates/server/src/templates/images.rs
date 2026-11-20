//! The libvips operations of `Templates::ModifyDocuments` on images: redaction boxes
//! (`draw_rect` with `redaction_ink`), quarter turns, `flatten(background: 255)`, the
//! scanned-page check and the files written from them.

use std::io::Cursor;

use image::{DynamicImage, GenericImageView as _, ImageBuffer, ImageFormat, Pixel, Primitive};
use loco_pdf::modify::Redaction;

use super::previews::{self, PreviewError};

/// `ModifyDocuments::SCAN_WHITE_THRESHOLD` and `SCAN_WHITE_FRACTION`.
const SCAN_WHITE_THRESHOLD: usize = 220;
const SCAN_WHITE_FRACTION: f64 = 0.6;
/// `ProcessDocument`'s JPEG quality for rotated or redacted JPEG documents.
const JPEG_Q: u32 = 90;

/// `redaction_ink(bands, color)`: 255 for white, else 0, and 255 in a fourth band.
fn ink<S: Primitive>(bands: usize, white: bool) -> Vec<S> {
    let value = if white { 255u8 } else { 0 };
    (0..bands)
        .map(|band| {
            let v = if band == 3 { 255 } else { value };
            S::from(v).unwrap_or_else(S::zero)
        })
        .collect()
}

fn fill_rect<P: Pixel>(
    buffer: &mut ImageBuffer<P, Vec<P::Subpixel>>,
    white: bool,
    (left, top, width, height): (u32, u32, u32, u32),
) {
    let ink = ink::<P::Subpixel>(usize::from(P::CHANNEL_COUNT), white);
    for y in top..top + height {
        for x in left..left + width {
            buffer
                .get_pixel_mut(x, y)
                .channels_mut()
                .copy_from_slice(&ink);
        }
    }
}

/// `draw_image_redaction(image, rects)`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "pixel coordinates, as Ruby's Float#floor and #ceil, clamped to the image"
)]
pub fn draw_redaction(image: &mut DynamicImage, rects: &[Redaction]) {
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return;
    }
    let (w, h) = (i64::from(width), i64::from(height));
    for rect in rects {
        let left = ((rect.x * f64::from(width)).floor() as i64).clamp(0, w - 1);
        let top = ((rect.y * f64::from(height)).floor() as i64).clamp(0, h - 1);
        let rect_width = ((rect.w * f64::from(width)).ceil() as i64).clamp(1, w - left);
        let rect_height = ((rect.h * f64::from(height)).ceil() as i64).clamp(1, h - top);
        let area = (
            left as u32,
            top as u32,
            rect_width as u32,
            rect_height as u32,
        );
        match &mut *image {
            DynamicImage::ImageLuma8(b) => fill_rect(b, rect.white, area),
            DynamicImage::ImageLumaA8(b) => fill_rect(b, rect.white, area),
            DynamicImage::ImageRgb8(b) => fill_rect(b, rect.white, area),
            DynamicImage::ImageRgba8(b) => fill_rect(b, rect.white, area),
            DynamicImage::ImageLuma16(b) => fill_rect(b, rect.white, area),
            DynamicImage::ImageLumaA16(b) => fill_rect(b, rect.white, area),
            DynamicImage::ImageRgb16(b) => fill_rect(b, rect.white, area),
            DynamicImage::ImageRgba16(b) => fill_rect(b, rect.white, area),
            other => {
                let mut rgba = other.to_rgba8();
                fill_rect(&mut rgba, rect.white, area);
                *other = DynamicImage::ImageRgba8(rgba);
            }
        }
    }
}

/// `rotate_vips_image`: `rot90`, `rot180` and `rot270` turn clockwise.
#[must_use]
pub fn rotate(image: DynamicImage, degrees: i64) -> DynamicImage {
    match degrees {
        90 => image.rotate90(),
        180 => image.rotate180(),
        270 => image.rotate270(),
        _ => image,
    }
}

/// `flatten(background: 255)` of an 8-bit image with alpha:
/// `(p * alpha + 255 * (255 - alpha)) / 255.0`, truncated.
#[must_use]
pub fn flatten_white(image: DynamicImage) -> DynamicImage {
    let blend = |p: u8, alpha: u8| {
        let value = (u32::from(p) * u32::from(alpha) + 255 * u32::from(255 - alpha)) / 255;
        u8::try_from(value).unwrap_or(255)
    };
    match image {
        DynamicImage::ImageRgba8(rgba) => {
            let (width, height) = rgba.dimensions();
            let data = rgba
                .as_raw()
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [blend(p[0], p[3]), blend(p[1], p[3]), blend(p[2], p[3])])
                .collect();
            ImageBuffer::from_raw(width, height, data).map_or_else(
                || DynamicImage::ImageRgba8(rgba.clone()),
                DynamicImage::ImageRgb8,
            )
        }
        DynamicImage::ImageLumaA8(la) => {
            let (width, height) = la.dimensions();
            let data = la
                .as_raw()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| blend(p[0], p[1]))
                .collect();
            ImageBuffer::from_raw(width, height, data).map_or_else(
                || DynamicImage::ImageLumaA8(la.clone()),
                DynamicImage::ImageLuma8,
            )
        }
        other if other.color().has_alpha() => DynamicImage::ImageRgb8(other.to_rgb8()),
        other => other,
    }
}

/// `scanned_page_image?`: at least 60% of the pixels are light gray or white
/// (`colourspace('b-w')`, luminance in linear light).
#[must_use]
pub fn scanned_page(image: &DynamicImage) -> bool {
    fn to_linear(c: u8) -> f64 {
        let v = f64::from(c) / 255.0;
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }
    fn to_srgb(v: f64) -> f64 {
        if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    }
    let rgb = image.to_rgb8();
    let mut counts = [0u64; 256];
    for p in rgb.pixels() {
        let y = 0.2126 * to_linear(p[0]) + 0.7152 * to_linear(p[1]) + 0.0722 * to_linear(p[2]);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to 0..=255"
        )]
        let gray = (to_srgb(y) * 255.0).round().clamp(0.0, 255.0) as usize;
        counts[gray] += 1;
    }
    let total: u64 = counts.iter().sum();
    let light: u64 = counts[SCAN_WHITE_THRESHOLD..].iter().sum();
    #[allow(clippy::cast_precision_loss, reason = "pixel counts")]
    let enough = light as f64 >= total as f64 * SCAN_WHITE_FRACTION;
    enough
}

/// The palette PNG `build_pdf_data_from_image` embeds (`compression: 6, filter: 0,
/// bitdepth:, palette: true, Q: 95, dither: 0, strip: true`), of an 8-bit RGB image.
pub fn palette_png(image: &DynamicImage) -> Result<Vec<u8>, PreviewError> {
    let rgb = image.to_rgb8();
    let sums = previews::band_sums(rgb.as_raw(), 3, 3);
    let rgba = image.to_rgba8();
    previews::encode(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
        previews::bitdepth(&sums),
        1000,
        None,
    )
}

/// `write_to_buffer('.jpg', Q: 90, strip: true)` or `('.png', strip: true)`, with the
/// file's extension and content type (`build_image_document`).
pub fn write_document(
    image: &DynamicImage,
    jpeg: bool,
) -> Result<(Vec<u8>, &'static str, &'static str), PreviewError> {
    if jpeg {
        let (samples, bands) = match image {
            DynamicImage::ImageLuma8(gray) => (gray.as_raw().clone(), 1),
            other => (other.to_rgb8().into_raw(), 3),
        };
        let (width, height) = image.dimensions();
        let data = loco_pdf::jpeg::encode_baseline(
            &samples,
            width as usize,
            height as usize,
            bands,
            JPEG_Q,
        );
        return Ok((data, ".jpg", "image/jpeg"));
    }
    let mut data = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut data), ImageFormat::Png)
        .map_err(|e| PreviewError::Image(e.to_string()))?;
    Ok((data, ".png", "image/png"))
}

/// `encode_redacted_image_jpeg`: an image object's pixels as RGB (or gray), the
/// redactions drawn, written as `jpg` with `Q: 50, strip: true`.
#[must_use]
pub fn redacted_jpeg(
    bitmap: &loco_pdf::modify::ImageBitmap,
    rects: &[loco_pdf::modify::PixelRect],
) -> Vec<u8> {
    use loco_pdf::modify::BitmapFormat;

    let (bands, mut samples): (usize, Vec<u8>) = match bitmap.format {
        BitmapFormat::Gray => (1, bitmap.data.clone()),
        BitmapFormat::Bgr => (
            3,
            bitmap
                .data
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|p| [p[2], p[1], p[0]])
                .collect(),
        ),
        BitmapFormat::Bgrx => (
            3,
            bitmap
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[2], p[1], p[0]])
                .collect(),
        ),
        BitmapFormat::Bgra => {
            let blend = |p: u8, alpha: u8| {
                let value = (u32::from(p) * u32::from(alpha) + 255 * u32::from(255 - alpha)) / 255;
                u8::try_from(value).unwrap_or(255)
            };
            (
                3,
                bitmap
                    .data
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|p| [blend(p[2], p[3]), blend(p[1], p[3]), blend(p[0], p[3])])
                    .collect(),
            )
        }
    };
    let width = bitmap.width;
    for rect in rects {
        let value = if rect.white { 255 } else { 0 };
        let (left, top) = (
            usize::try_from(rect.left).unwrap_or(0),
            usize::try_from(rect.top).unwrap_or(0),
        );
        let (w, h) = (
            usize::try_from(rect.width).unwrap_or(0),
            usize::try_from(rect.height).unwrap_or(0),
        );
        for y in top..(top + h).min(bitmap.height) {
            for x in left..(left + w).min(width) {
                let start = (y * width + x) * bands;
                samples[start..start + bands].fill(value);
            }
        }
    }
    loco_pdf::jpeg::encode_baseline(&samples, width, bitmap.height, bands, 50)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_alpha_onto_white_like_libvips() {
        let rgba = ImageBuffer::from_raw(1, 1, vec![100, 0, 255, 128]).unwrap();
        let flat = flatten_white(DynamicImage::ImageRgba8(rgba)).to_rgb8();
        // (100 * 128 + 255 * 127) / 255 = 177.2, (0 + 32385) / 255 = 127, 255.
        assert_eq!(flat.get_pixel(0, 0).0, [177, 127, 255]);
    }

    #[test]
    fn draws_redactions_with_the_image_bands() {
        let mut image =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(10, 10, image::Rgba([9, 9, 9, 9])));
        draw_redaction(
            &mut image,
            &[Redaction {
                x: 0.15,
                y: 0.9,
                w: 0.21,
                h: 0.5,
                white: false,
            }],
        );
        let rgba = image.to_rgba8();
        // left 1, top 9, width ceil(2.1) = 3, height clamped to 1.
        assert_eq!(rgba.get_pixel(1, 9).0, [0, 0, 0, 255]);
        assert_eq!(rgba.get_pixel(3, 9).0, [0, 0, 0, 255]);
        assert_eq!(rgba.get_pixel(4, 9).0, [9, 9, 9, 9]);
        assert_eq!(rgba.get_pixel(1, 8).0, [9, 9, 9, 9]);
    }
}
