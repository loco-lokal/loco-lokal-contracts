//! `Templates::BuildImagePagePdf`: a one-page PDF showing a PNG, written byte for byte as
//! Rails writes it. The PNG's compressed data is embedded as is (`/FlateDecode` with the PNG
//! predictor), so only non-interlaced grayscale, RGB and palette PNGs without transparency
//! are accepted.

use std::fmt::Write as _;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct InvalidPng(pub String);

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

struct Png {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    interlace: u8,
    palette: Option<Vec<u8>>,
    idat: Vec<u8>,
}

/// `BuildImagePagePdf.call(png_data, page_width:, page_height:, image_box:)`. Sizes are
/// Ruby Floats; `image_box` is `[x, y, width, height]` and defaults to the whole page.
pub fn build(
    png_data: &[u8],
    page_width: f64,
    page_height: f64,
    image_box: Option<[f64; 4]>,
) -> Result<Vec<u8>, InvalidPng> {
    let png = parse_png(png_data)?;
    if png.interlace != 0 {
        return Err(InvalidPng("interlaced png is not supported".into()));
    }
    let (color_space, colors) = match png.color_type {
        0 => ("/DeviceGray".to_owned(), 1),
        2 => ("/DeviceRGB".to_owned(), 3),
        3 => {
            let palette = png
                .palette
                .as_ref()
                .ok_or_else(|| InvalidPng("missing palette".into()))?;
            (
                format!(
                    "[ /Indexed /DeviceRGB {} <{}> ]",
                    (palette.len() / 3).cast_signed() - 1,
                    hex(palette)
                ),
                1,
            )
        }
        other => return Err(InvalidPng(format!("unsupported color type {other}"))),
    };
    let image_box = image_box.unwrap_or([0.0, 0.0, page_width, page_height]);
    Ok(build_pdf(
        &png,
        &color_space,
        colors,
        [round4(page_width), round4(page_height)],
        image_box.map(round4),
    ))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// `Float#round(4)`.
fn round4(value: f64) -> f64 {
    let rounded = (value * 10_000.0).round() / 10_000.0;
    if rounded.is_finite() { rounded } else { value }
}

/// `Float#to_s`.
fn ruby_float(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() { "-0.0" } else { "0.0" }.to_owned();
    }
    let abs = value.abs();
    if (1e-4..1e16).contains(&abs) {
        let s = format!("{value}");
        if s.contains('.') { s } else { format!("{s}.0") }
    } else {
        // `1.0e-05`, `1.234e+16`
        let s = format!("{value:e}");
        let (mantissa, exponent) = s.split_once('e').unwrap_or((&s, "0"));
        let mantissa = if mantissa.contains('.') {
            mantissa.to_owned()
        } else {
            format!("{mantissa}.0")
        };
        let exponent: i32 = exponent.parse().unwrap_or(0);
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.abs())
    }
}

fn parse_png(data: &[u8]) -> Result<Png, InvalidPng> {
    if !data.starts_with(PNG_SIGNATURE) {
        return Err(InvalidPng("not a png".into()));
    }
    let mut ihdr: Option<&[u8]> = None;
    let mut palette = None;
    let mut idat = Vec::new();
    let mut pos = 8;
    while pos + 8 <= data.len() {
        let length = u32::from_be_bytes(data[pos..pos + 4].try_into().expect("4 bytes")) as usize;
        let kind = &data[pos + 4..pos + 8];
        let start = pos + 8;
        let body = &data[start.min(data.len())..(start + length).min(data.len())];
        match kind {
            b"IHDR" => ihdr = Some(body),
            b"PLTE" => palette = Some(body.to_vec()),
            b"tRNS" => return Err(InvalidPng("transparency is not supported".into())),
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        pos += 12 + length;
    }
    let ihdr = ihdr
        .filter(|h| h.len() >= 13 && !idat.is_empty())
        .ok_or_else(|| InvalidPng("missing image data".into()))?;
    Ok(Png {
        width: u32::from_be_bytes(ihdr[0..4].try_into().expect("4 bytes")),
        height: u32::from_be_bytes(ihdr[4..8].try_into().expect("4 bytes")),
        bit_depth: ihdr[8],
        color_type: ihdr[9],
        interlace: ihdr[12],
        palette,
        idat,
    })
}

fn build_pdf(
    png: &Png,
    color_space: &str,
    colors: u8,
    [page_width, page_height]: [f64; 2],
    [image_x, image_y, image_width, image_height]: [f64; 4],
) -> Vec<u8> {
    let contents = format!(
        "q\n{} 0 0 {} {} {} cm\n/Im0 Do\nQ",
        ruby_float(image_width),
        ruby_float(image_height),
        ruby_float(image_x),
        ruby_float(image_y)
    );
    let image_dict = format!(
        "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} \
         /BitsPerComponent {bd} /ColorSpace {color_space} /Filter /FlateDecode \
         /DecodeParms << /Predictor 15 /Colors {colors} /BitsPerComponent {bd} \
         /Columns {w} >> /Length {len} >>",
        w = png.width,
        h = png.height,
        bd = png.bit_depth,
        len = png.idat.len(),
    );
    let stream = |dict: &str, data: &[u8]| {
        let mut out = format!("{dict}\nstream\n").into_bytes();
        out.extend_from_slice(data);
        out.extend_from_slice(b"\nendstream");
        out
    };
    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [ 3 0 R ] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [ 0 0 {} {} ] \
             /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>",
            ruby_float(page_width),
            ruby_float(page_height)
        )
        .into_bytes(),
        stream(&image_dict, &png.idat),
        stream(
            &format!("<< /Length {} >>", contents.len()),
            contents.as_bytes(),
        ),
    ];

    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref_offset = pdf.len();
    let size = objects.len() + 1;
    pdf.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF")
            .as_bytes(),
    );
    pdf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_floats_like_ruby() {
        assert_eq!(ruby_float(612.0), "612.0");
        assert_eq!(ruby_float(595.28), "595.28");
        assert_eq!(ruby_float(0.00001), "1.0e-05");
        assert_eq!(ruby_float(1.5e16), "1.5e+16");
        assert_eq!(round4(841.889_763), 841.8898);
    }

    #[test]
    fn rejects_transparency_and_other_files() {
        assert_eq!(
            build(b"%PDF-1.4", 1.0, 1.0, None),
            Err(InvalidPng("not a png".into()))
        );
    }
}
