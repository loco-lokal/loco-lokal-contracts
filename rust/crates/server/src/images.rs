//! `ImageUtils`: loading uploaded signature images and rejecting empty or broken ones.

use image::{DynamicImage, ImageFormat};

/// `ImageUtils.load_vips` for the formats the signing form sends.
#[must_use]
pub fn load(data: &[u8], content_type: &str) -> Option<DynamicImage> {
    let format = match content_type {
        "image/png" | "image/apng" | "image/vnd.mozilla.apng" => ImageFormat::Png,
        "image/jpeg" | "image/jpg" | "image/pjpeg" => ImageFormat::Jpeg,
        "image/x-icon" | "image/vnd.microsoft.icon" => ImageFormat::Ico,
        "image/bmp" | "image/x-bmp" | "image/x-ms-bmp" => ImageFormat::Bmp,
        _ => image::guess_format(data).ok()?,
    };
    image::load_from_memory_with_format(data, format).ok()
}

/// Pixels as band values, in the image's own band layout.
fn pixel_rows(image: &DynamicImage) -> (usize, Vec<Vec<u16>>) {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let (bands, samples): (usize, Vec<u16>) = match image {
        DynamicImage::ImageLuma8(i) => (1, i.as_raw().iter().map(|&v| u16::from(v)).collect()),
        DynamicImage::ImageLumaA8(i) => (2, i.as_raw().iter().map(|&v| u16::from(v)).collect()),
        DynamicImage::ImageRgb8(i) => (3, i.as_raw().iter().map(|&v| u16::from(v)).collect()),
        DynamicImage::ImageRgba8(i) => (4, i.as_raw().iter().map(|&v| u16::from(v)).collect()),
        DynamicImage::ImageLuma16(i) => (1, i.as_raw().clone()),
        DynamicImage::ImageLumaA16(i) => (2, i.as_raw().clone()),
        DynamicImage::ImageRgb16(i) => (3, i.as_raw().clone()),
        DynamicImage::ImageRgba16(i) => (4, i.as_raw().clone()),
        other => (
            4,
            other
                .to_rgba8()
                .as_raw()
                .iter()
                .map(|&v| u16::from(v))
                .collect(),
        ),
    };
    let rows = samples
        .chunks(width * bands)
        .take(height)
        .map(<[u16]>::to_vec)
        .collect();
    (bands, rows)
}

/// `ImageUtils.blank?`: every band is all white or all black.
#[must_use]
pub fn is_blank(image: &DynamicImage) -> bool {
    let (bands, rows) = pixel_rows(image);
    let mut min = vec![u16::MAX; bands];
    let mut max = vec![0_u16; bands];
    for row in &rows {
        for pixel in row.chunks(bands) {
            for (band, &v) in pixel.iter().enumerate() {
                min[band] = min[band].min(v);
                max[band] = max[band].max(v);
            }
        }
    }
    let all = |values: &[u16], target: u16| values.iter().all(|&v| v == target);
    (all(&min, 255) && all(&max, 255)) || (all(&min, 0) && all(&max, 0))
}

/// `ImageUtils.error?`: the first two rows of the left quarter repeat with a shift of
/// three pixels and no two neighbors match (a garbled canvas export).
#[must_use]
pub fn is_error(image: &DynamicImage) -> bool {
    let (bands, rows) = pixel_rows(image);
    let quarter = image.width() as usize / 4;
    if rows.len() < 2 || quarter == 0 {
        return false;
    }
    let pixels = |row: &[u16]| -> Vec<Vec<u16>> {
        row.chunks(bands)
            .take(quarter)
            .map(<[u16]>::to_vec)
            .collect()
    };
    let first_row = pixels(&rows[0]);
    let second_row = pixels(&rows[1]);
    let shifted = first_row.get(3..).unwrap_or_default();
    let head = &second_row[..second_row.len().saturating_sub(3)];
    shifted == head && first_row.windows(2).all(|w| w[0] != w[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_blank_images() {
        let white =
            DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 4, image::Rgb([255; 3])));
        assert!(is_blank(&white));
        let mut drawn = image::RgbImage::from_pixel(4, 4, image::Rgb([255; 3]));
        drawn.put_pixel(1, 1, image::Rgb([0, 0, 0]));
        assert!(!is_blank(&DynamicImage::ImageRgb8(drawn)));
    }
}
