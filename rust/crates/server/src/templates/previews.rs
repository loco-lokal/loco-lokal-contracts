//! The `preview_images` PNGs of `Templates::ProcessDocument`, written as libvips writes
//! `write_to_buffer('.png', compression: 6, filter: 0, bitdepth:, palette: true, Q: 95,
//! dither: 0)`: quantized by libimagequant to a 2-, 4- or 8-bit palette, with the chunks
//! libvips adds through libpng.
//!
//! The pixels can differ slightly from Rails' (another `PDFium` build, and `image`'s Lanczos
//! filter instead of libvips' resize for uploaded images), and the compressed data does,
//! but sizes, bit depths, palettes sizes and metadata match.

use std::io::{Cursor, Write as _};

use flate2::{Compression, Crc, write::ZlibEncoder};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, imageops::FilterType};
use loco_pdf::upload::{Bitmap, MAX_WIDTH};

/// `Templates::ProcessDocument::Q`.
const Q: u8 = 95;
/// libvips' default `effort` (7) as libimagequant's speed (`11 - effort`).
const SPEED: i32 = 4;
/// libpng's `PNG_ZBUF_SIZE`: the largest IDAT chunk it writes.
const IDAT_SIZE: usize = 8192;

/// A PNG preview and its size (`metadata: { width:, height: }`).
#[derive(Clone, Debug)]
pub struct Preview {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    /// `ImageUtils::UnsupportedFormat`.
    #[error("unsupported image format {0}")]
    UnsupportedFormat(String),
    #[error("{0}")]
    Image(String),
}

/// `build_and_upload_blob(image, page_number)` for a rendered PDF page. libvips adds the
/// EXIF block it generates for every image (`strip` is not set) and the default
/// resolution of 1 pixel per mm.
pub fn page_png(bitmap: &Bitmap) -> Result<Preview, PreviewError> {
    let rgba = &bitmap.rgba;
    let sums = band_sums(rgba, 4, 3);
    let data = encode(
        rgba,
        bitmap.width,
        bitmap.height,
        bitdepth(&sums),
        1000,
        Some(&page_exif(bitmap.width, bitmap.height)),
    )?;
    Ok(Preview {
        data,
        width: bitmap.width,
        height: bitmap.height,
    })
}

/// `generate_preview_image`: the uploaded image, upright (`autorot`), scaled to
/// `MAX_WIDTH` wide, without metadata (`strip: true`) but with the source resolution.
pub fn image_png(data: &[u8], content_type: &str) -> Result<Preview, PreviewError> {
    let format = match content_type {
        "image/png" | "image/apng" | "image/vnd.mozilla.apng" => ImageFormat::Png,
        "image/jpeg" | "image/jpg" | "image/pjpeg" => ImageFormat::Jpeg,
        "image/x-icon" | "image/vnd.microsoft.icon" => ImageFormat::Ico,
        "image/bmp" | "image/x-bmp" | "image/x-ms-bmp" => ImageFormat::Bmp,
        other => return Err(PreviewError::UnsupportedFormat(other.to_owned())),
    };
    let image_error = |e: image::ImageError| PreviewError::Image(e.to_string());
    let mut decoder = ImageReader::with_format(Cursor::new(data), format)
        .into_decoder()
        .map_err(image_error)?;
    let orientation = decoder.orientation().map_err(image_error)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(image_error)?;
    image.apply_orientation(orientation);

    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return Err(PreviewError::Image("empty image".into()));
    }
    let target_width = u32::try_from(MAX_WIDTH).unwrap_or(1400);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a positive pixel count"
    )]
    let target_height =
        ((f64::from(height) * f64::from(target_width) / f64::from(width)).round() as u32).max(1);
    if (width, height) != (target_width, target_height) {
        let filter = if target_width < width {
            FilterType::Lanczos3
        } else {
            FilterType::CatmullRom
        };
        image = image.resize_exact(target_width, target_height, filter);
    }

    let bands = usize::from(image.color().channel_count());
    let sums = match &image {
        DynamicImage::ImageLuma16(i) => band_sums_u16(i.as_raw(), bands),
        DynamicImage::ImageLumaA16(i) => band_sums_u16(i.as_raw(), bands),
        DynamicImage::ImageRgb16(i) => band_sums_u16(i.as_raw(), bands),
        DynamicImage::ImageRgba16(i) => band_sums_u16(i.as_raw(), bands),
        other => band_sums(other.as_bytes(), bands, bands.min(3)),
    };
    let rgba = image.to_rgba8();
    let data = encode(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
        bitdepth(&sums),
        pixels_per_meter(data, format),
        None,
    )?;
    Ok(Preview {
        data,
        width: rgba.width(),
        height: rgba.height(),
    })
}

/// The sum of each of the first `count` bands (`image.stats` column 2).
fn band_sums(samples: &[u8], bands: usize, count: usize) -> Vec<u64> {
    let mut sums = vec![0_u64; count];
    for pixel in samples.chunks_exact(bands) {
        for (sum, &value) in sums.iter_mut().zip(pixel) {
            *sum += u64::from(value);
        }
    }
    sums
}

fn band_sums_u16(samples: &[u16], bands: usize) -> Vec<u64> {
    let mut sums = vec![0_u64; bands.min(3)];
    for pixel in samples.chunks_exact(bands) {
        for (sum, &value) in sums.iter_mut().zip(pixel) {
            *sum += u64::from(value);
        }
    }
    sums
}

/// `2**image.stats.to_a[1..3].pluck(2).uniq.size`: 2 bits for gray images, 8 for most
/// colored ones.
fn bitdepth(sums: &[u64]) -> u8 {
    let mut unique = sums.to_vec();
    unique.sort_unstable();
    unique.dedup();
    1 << unique.len().clamp(1, 3)
}

/// The `pHYs` value libvips writes: the loader's resolution (pixels per mm) × 1000. An
/// EXIF resolution overrides the file's own (`vips__exif_parse`).
fn pixels_per_meter(data: &[u8], format: ImageFormat) -> u32 {
    let exif = match format {
        ImageFormat::Png => png_chunk(data, *b"eXIf"),
        ImageFormat::Jpeg => jpeg_exif(data),
        _ => None,
    };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "`rint(xres * 1000)` of a positive resolution"
    )]
    if let Some(per_mm) = exif.and_then(exif_resolution) {
        return (per_mm * 1000.0).round() as u32;
    }
    match format {
        ImageFormat::Png => png_phys(data).unwrap_or(2834),
        ImageFormat::Jpeg => jfif_density(data).unwrap_or(2835),
        _ => 1000,
    }
}

fn png_chunk(data: &[u8], name: [u8; 4]) -> Option<&[u8]> {
    let mut pos = 8;
    while pos + 8 <= data.len() {
        let length =
            usize::try_from(u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?)).ok()?;
        if data[pos + 4..pos + 8] == name {
            return data.get(pos + 8..pos + 8 + length);
        }
        pos += 12 + length;
    }
    None
}

/// The TIFF data of a JPEG's `Exif` APP1 segment.
fn jpeg_exif(data: &[u8]) -> Option<&[u8]> {
    let mut pos = 2;
    while pos + 4 <= data.len() && data[pos] == 0xFF {
        let marker = data[pos + 1];
        if marker == 0xDA || marker == 0xD9 {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([data[pos + 2], data[pos + 3]]));
        let body = data.get(pos + 4..pos + 2 + length)?;
        if marker == 0xE1 && body.starts_with(b"Exif\0\0") {
            return Some(&body[6..]);
        }
        pos += 2 + length;
    }
    None
}

/// `XResolution` of IFD0 in pixels per mm (`ResolutionUnit` inches unless 3, cm).
fn exif_resolution(tiff: &[u8]) -> Option<f64> {
    let little = match tiff.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let b: [u8; 2] = tiff.get(at..at + 2)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let b: [u8; 4] = tiff.get(at..at + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    let ifd = usize::try_from(u32_at(4)?).ok()?;
    let (mut resolution, mut unit) = (None, 2);
    for i in 0..usize::from(u16_at(ifd)?) {
        let entry = ifd + 2 + i * 12;
        match u16_at(entry)? {
            0x011A => {
                let at = usize::try_from(u32_at(entry + 8)?).ok()?;
                let (num, den) = (u32_at(at)?, u32_at(at + 4)?);
                if den != 0 {
                    resolution = Some(f64::from(num) / f64::from(den));
                }
            }
            0x0128 => unit = u16_at(entry + 8)?,
            _ => {}
        }
    }
    let resolution = resolution.filter(|r| *r > 0.0)?;
    Some(if unit == 3 {
        resolution / 10.0
    } else {
        resolution / 25.4
    })
}

/// `pngload`: a `pHYs` in pixels per meter as is; one without a unit is read as pixels
/// per mm. Without one, libvips assumes 72 dpi, truncated to 2834 per meter.
fn png_phys(data: &[u8]) -> Option<u32> {
    let mut pos = 8;
    while pos + 8 <= data.len() {
        let length =
            usize::try_from(u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?)).ok()?;
        let kind = &data[pos + 4..pos + 8];
        if kind == b"pHYs" && length >= 9 {
            let body = data.get(pos + 8..pos + 17)?;
            let x = u32::from_be_bytes(body[0..4].try_into().ok()?);
            return Some(if body[8] == 1 {
                x
            } else {
                x.saturating_mul(1000)
            });
        }
        if kind == b"IDAT" || kind == b"IEND" {
            return None;
        }
        pos += 12 + length;
    }
    None
}

/// `jpegload`: the JFIF density in dots per inch or cm, else 72 dpi.
fn jfif_density(data: &[u8]) -> Option<u32> {
    let app0 = data.get(2..)?.windows(5).position(|w| w == b"JFIF\0")? + 2;
    let unit = *data.get(app0 + 7)?;
    let x = f64::from(u16::from_be_bytes([
        *data.get(app0 + 8)?,
        *data.get(app0 + 9)?,
    ]));
    let per_mm = match unit {
        1 => x / 25.4,
        2 => x / 10.0,
        _ => return None,
    };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "`rint(xres * 1000)` of a JFIF density"
    )]
    Some((per_mm * 1000.0).round() as u32)
}

/// The EXIF block libvips writes for an image without one: orientation 1, 25.4 dpi
/// (1 pixel per mm), and the pixel dimensions.
fn page_exif(width: u32, height: u32) -> Vec<u8> {
    const TEMPLATE: [u8; 180] = [
        0x49, 0x49, 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, 0x06, 0x00, 0x12, 0x01, 0x03, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x01, 0x05, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x56, 0x00, 0x00, 0x00, 0x1b, 0x01, 0x05, 0x00, 0x01, 0x00, 0x00, 0x00, 0x5e, 0x00, 0x00,
        0x00, 0x28, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x13, 0x02,
        0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x69, 0x87, 0x04, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x38, 0x63, 0x00, 0x00,
        0xe8, 0x03, 0x00, 0x00, 0x38, 0x63, 0x00, 0x00, 0xe8, 0x03, 0x00, 0x00, 0x06, 0x00, 0x00,
        0x90, 0x07, 0x00, 0x04, 0x00, 0x00, 0x00, 0x30, 0x32, 0x31, 0x30, 0x01, 0x91, 0x07, 0x00,
        0x04, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x00, 0x00, 0xa0, 0x07, 0x00, 0x04, 0x00, 0x00,
        0x00, 0x30, 0x31, 0x30, 0x30, 0x01, 0xa0, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0xff, 0xff,
        0x00, 0x00, 0x02, 0xa0, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03,
        0xa0, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let mut exif = TEMPLATE.to_vec();
    exif[160..164].copy_from_slice(&width.to_le_bytes());
    exif[172..176].copy_from_slice(&height.to_le_bytes());
    exif
}

/// Quantizes RGBA pixels and writes the palette PNG.
fn encode(
    rgba: &[u8],
    width: u32,
    height: u32,
    bitdepth: u8,
    pixels_per_meter: u32,
    exif: Option<&[u8]>,
) -> Result<Vec<u8>, PreviewError> {
    let quant_error = |e: imagequant::Error| PreviewError::Image(e.to_string());
    let (w, h) = (width as usize, height as usize);
    let pixels: Vec<imagequant::RGBA> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| imagequant::RGBA::new(p[0], p[1], p[2], p[3]))
        .collect();

    let mut attr = imagequant::new();
    attr.set_max_colors(1 << bitdepth).map_err(quant_error)?;
    attr.set_quality(0, Q).map_err(quant_error)?;
    attr.set_speed(SPEED).map_err(quant_error)?;
    let mut image = attr.new_image(pixels, w, h, 0.0).map_err(quant_error)?;
    let mut result = attr.quantize(&mut image).map_err(quant_error)?;
    result.set_dithering_level(0.0).map_err(quant_error)?;
    let (palette, indexes) = result.remapped(&mut image).map_err(quant_error)?;

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[bitdepth, 3, 0, 0, 0]);
    chunk(&mut png, *b"IHDR", &ihdr);
    let plte: Vec<u8> = palette.iter().flat_map(|c| [c.r, c.g, c.b]).collect();
    chunk(&mut png, *b"PLTE", &plte);
    if let Some(last) = palette.iter().rposition(|c| c.a != 255) {
        let trns: Vec<u8> = palette[..=last].iter().map(|c| c.a).collect();
        chunk(&mut png, *b"tRNS", &trns);
    }
    if let Some(exif) = exif {
        chunk(&mut png, *b"eXIf", exif);
    }
    let mut phys = Vec::with_capacity(9);
    phys.extend_from_slice(&pixels_per_meter.to_be_bytes());
    phys.extend_from_slice(&pixels_per_meter.to_be_bytes());
    phys.push(1);
    chunk(&mut png, *b"pHYs", &phys);

    // Filter type 0 (`filter: 0`) on every row, pixels packed most significant bit first.
    let per_byte = 8 / usize::from(bitdepth);
    let row_bytes = w.div_ceil(per_byte);
    let mut raw = Vec::with_capacity((row_bytes + 1) * h);
    for row in indexes.chunks(w.max(1)).take(h) {
        raw.push(0);
        for group in row.chunks(per_byte) {
            let mut byte = 0_u8;
            for (i, &index) in group.iter().enumerate() {
                byte |= index << (8 - usize::from(bitdepth) * (i + 1));
            }
            raw.push(byte);
        }
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(6));
    encoder
        .write_all(&raw)
        .map_err(|e| PreviewError::Image(e.to_string()))?;
    let compressed = encoder
        .finish()
        .map_err(|e| PreviewError::Image(e.to_string()))?;
    for part in compressed.chunks(IDAT_SIZE) {
        chunk(&mut png, *b"IDAT", part);
    }
    chunk(&mut png, *b"IEND", &[]);
    Ok(png)
}

fn chunk(png: &mut Vec<u8>, kind: [u8; 4], body: &[u8]) {
    png.extend_from_slice(&u32::try_from(body.len()).unwrap_or(u32::MAX).to_be_bytes());
    let mut crc = Crc::new();
    crc.update(&kind);
    crc.update(body);
    png.extend_from_slice(&kind);
    png.extend_from_slice(body);
    png.extend_from_slice(&crc.sum().to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(png: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut pos = 8;
        while pos + 8 <= png.len() {
            let length = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
            out.push((
                String::from_utf8_lossy(&png[pos + 4..pos + 8]).into_owned(),
                png[pos + 8..pos + 8 + length].to_vec(),
            ));
            pos += 12 + length;
        }
        out
    }

    #[test]
    fn writes_gray_pages_as_four_color_palettes() {
        let (width, height) = (40_u32, 30_u32);
        let mut rgba = vec![255_u8; (width * height * 4) as usize];
        for pixel in rgba.as_chunks_mut::<4>().0.iter_mut().take(300) {
            pixel[..3].copy_from_slice(&[20, 20, 20]);
        }
        let preview = page_png(&Bitmap {
            width,
            height,
            rgba,
        })
        .unwrap();
        let chunks = chunks(&preview.data);
        let names: Vec<&str> = chunks.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["IHDR", "PLTE", "eXIf", "pHYs", "IDAT", "IEND"]);
        // 2-bit palette, as `bitdepth = 2**1` for R = G = B.
        assert_eq!(chunks[0].1, [0, 0, 0, 40, 0, 0, 0, 30, 2, 3, 0, 0, 0]);
        assert!(chunks[1].1.len() <= 12);
        assert_eq!(chunks[3].1, [0, 0, 3, 0xe8, 0, 0, 3, 0xe8, 1]);
        let decoded = image::load_from_memory(&preview.data).unwrap().to_rgb8();
        assert_eq!(decoded.get_pixel(0, 0).0, [20, 20, 20]);
        assert_eq!(decoded.get_pixel(39, 29).0, [255, 255, 255]);
    }

    #[test]
    fn picks_bit_depths_like_rails() {
        assert_eq!(bitdepth(&[5, 5, 5]), 2);
        assert_eq!(bitdepth(&[5, 6, 5]), 4);
        assert_eq!(bitdepth(&[5, 6, 7]), 8);
        assert_eq!(bitdepth(&[5]), 2);
    }

    #[test]
    fn scales_images_to_the_preview_width() {
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            1000,
            333,
            image::Rgb([10, 200, 30]),
        ));
        let mut png = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        let preview = image_png(&png, "image/png").unwrap();
        assert_eq!((preview.width, preview.height), (1400, 466));
        let chunks = chunks(&preview.data);
        assert!(chunks.iter().all(|(n, _)| n != "eXIf"));
        assert_eq!(chunks[2].1, [0, 0, 0x0b, 0x12, 0, 0, 0x0b, 0x12, 1]);
        assert!(matches!(
            image_png(&png, "image/gif"),
            Err(PreviewError::UnsupportedFormat(_))
        ));
    }
}
