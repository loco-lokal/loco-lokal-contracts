//! The `PDFium` work of `Templates::ModifyDocuments#build_document`: pages imported from
//! the source documents into a new one, then redacted (`Pdfium::Page#redact`) and turned.
//!
//! Rails' `PDFium` build registers the imported form fields (`FPDF_ImportAcroForm`) and
//! drops the objects a redaction leaves unreferenced (`FPDF_RemoveOrphanObjects`); the
//! public builds have neither, so [`crate::acroform`] does both on the saved file.
#![allow(
    unsafe_code,
    reason = "PDFium FFI calls on handles this module opens and closes"
)]

use pdfium_render::prelude::{
    FPDF_FILEACCESS, FPDF_PAGE, FPDF_PAGEOBJECT, FPDF_WCHAR, FS_MATRIX, Pdfium,
    PdfiumLibraryBindingsAccessor,
};

use crate::pages::{Document, Page};

const FPDF_PAGEOBJ_TEXT: i32 = 1;
const FPDF_PAGEOBJ_IMAGE: i32 = 3;
const FPDF_BITMAP_GRAY: i32 = 1;
const FPDF_BITMAP_BGR: i32 = 2;
const FPDF_BITMAP_BGRX: i32 = 3;
const FPDF_BITMAP_BGRA: i32 = 4;
const FPDF_FILLMODE_ALTERNATE: i32 = 1;
/// `Pdfium::BLANK_TEXT_CODEPOINTS`.
const BLANK_TEXT_CODEPOINTS: [u32; 6] = [0x00, 0x09, 0x0A, 0x0D, 0x20, 0xA0];

/// A redaction rectangle in page fractions, from the top left.
#[derive(Clone, Debug, PartialEq)]
pub struct Redaction {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// `color: 'white'` removes the content without drawing a black box.
    pub white: bool,
}

/// What happens to one page of the new document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageOps {
    /// Degrees: 0, 90, 180 or 270.
    pub rotate: i32,
    pub redact: Vec<Redaction>,
}

/// Pages of one source, imported in one call: `pages` as `FPDF_ImportPages` takes them
/// (`"1,3"`), `length` of them.
#[derive(Clone, Debug)]
pub struct Run {
    pub source: usize,
    pub pages: String,
    pub length: i32,
}

/// The layout of a `PDFium` bitmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitmapFormat {
    Gray,
    Bgr,
    Bgrx,
    Bgra,
}

/// `extract_image_bitmap`: an image object's pixels, rows packed.
#[derive(Clone, Debug)]
pub struct ImageBitmap {
    pub data: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub bands: usize,
    pub format: BitmapFormat,
}

/// `image_pixel_rects`: a redaction in the image's pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub left: i64,
    pub top: i64,
    pub width: i64,
    pub height: i64,
    pub white: bool,
}

/// Writes the redacted image of an image object as a JPEG (`encode_redacted_image_jpeg`).
pub type EncodeRedacted<'a> = &'a dyn Fn(&ImageBitmap, &[PixelRect]) -> Vec<u8>;

/// `source.page_size(page)`: `FPDF_GetPageSizeByIndexF` of a page of `bytes`.
pub fn page_size(pdfium: &Pdfium, bytes: &[u8], page: i32) -> Result<Option<(f32, f32)>, String> {
    let doc = Document::open(pdfium.bindings(), bytes)?;
    Ok(doc.page_size(page))
}

/// `build_document`'s `PDFium` part: a new document with the `runs` of `sources`, then
/// `ops` applied to its pages (`apply_pdf_page_ops`; `None` for pages that came from
/// images, which are turned and redacted before). Returns the saved file.
pub fn build(
    pdfium: &Pdfium,
    sources: &[&[u8]],
    runs: &[Run],
    ops: &[Option<PageOps>],
    encode: EncodeRedacted<'_>,
) -> Result<Vec<u8>, String> {
    let bindings = pdfium.bindings();
    let imported = {
        let dest = Document::create(bindings)?;
        let mut opened: Vec<Option<Document<'_>>> = sources.iter().map(|_| None).collect();
        let mut index = 0;
        for run in runs {
            let slot = opened
                .get_mut(run.source)
                .ok_or_else(|| format!("no source {}", run.source))?;
            if slot.is_none() {
                *slot = Some(Document::open(bindings, sources[run.source])?);
            }
            let source = slot.as_ref().ok_or("source not opened")?;
            dest.import_pages(source, &run.pages, index)?;
            index += run.length;
        }
        dest.save()?
    };
    let imports: Vec<crate::acroform::Import<'_>> = runs
        .iter()
        .map(|run| crate::acroform::Import {
            source: sources[run.source],
            pages: &run.pages,
            length: run.length,
        })
        .collect();
    let data = crate::acroform::import(imported, &imports)?;

    if ops
        .iter()
        .flatten()
        .all(|op| op.rotate % 360 == 0 && op.redact.is_empty())
    {
        return Ok(data);
    }
    let (saved, redacted) = {
        let doc = Document::open(bindings, &data)?;
        let mut redacted = false;
        for (index, op) in ops.iter().enumerate() {
            let Some(op) = op else { continue };
            let rotate = op.rotate.rem_euclid(360);
            if rotate == 0 && op.redact.is_empty() {
                continue;
            }
            let mut page = doc.page(i64::try_from(index).unwrap_or(i64::MAX))?;
            if !op.redact.is_empty() {
                redact(&mut page, &op.redact, encode)?;
                redacted = true;
            }
            if rotate != 0 {
                let rotation = page.rotation();
                page.set_rotation((rotation + rotate / 90) % 4);
                page.rotate();
            }
        }
        (doc.save()?, redacted)
    };
    crate::acroform::after_flatten(saved, redacted)
}

/// `Pdfium::Page#redact(rects, &image_processor)`.
fn redact(
    page: &mut Page<'_>,
    rects: &[Redaction],
    encode: EncodeRedacted<'_>,
) -> Result<(), String> {
    page.flatten()?;
    page.rotate();

    let (width, height) = (page.width(), page.height());
    let bounds: Vec<([f64; 4], bool)> = rects
        .iter()
        .map(|rect| {
            let left = rect.x * width;
            let top = height - rect.y * height;
            (
                [left, top - rect.h * height, left + rect.w * width, top],
                rect.white,
            )
        })
        .collect();
    let boxes: Vec<[f64; 4]> = bounds.iter().map(|(b, _)| *b).collect();

    page.unwrap_form_objects_in(Some(&boxes))?;
    remove_redacted_chars(page, &boxes)?;
    redact_image_objects(page, &bounds, encode)?;
    draw_redaction_rects(page, &bounds);

    // SAFETY: a loaded page.
    if unsafe { page.b().FPDFPage_GenerateContent(page.ptr) } == 0 {
        return Err("Failed to generate page content".into());
    }
    remove_blank_text_objects(page)
}

/// `remove_page_object`.
fn remove_page_object(page: &Page<'_>, object: FPDF_PAGEOBJECT) -> Result<(), String> {
    // SAFETY: an object of the loaded page, destroyed once it is removed.
    unsafe {
        if page.b().FPDFPage_RemoveObject(page.ptr, object) == 0 {
            return Err("Failed to remove page object".into());
        }
        page.b().FPDFPageObj_Destroy(object);
    }
    Ok(())
}

struct Char {
    codepoint: u32,
    origin_x: f64,
    origin_y: f64,
    redacted: bool,
}

/// `remove_redacted_chars`: text objects with a character whose box center is in a
/// redaction are removed; their other characters are written again one by one.
fn remove_redacted_chars(page: &Page<'_>, rects: &[[f64; 4]]) -> Result<(), String> {
    let b = page.b();
    let mut objects: Vec<(FPDF_PAGEOBJECT, Vec<Char>)> = Vec::new();
    // SAFETY: a loaded page; the text page is closed below.
    unsafe {
        let text_page = b.FPDFText_LoadPage(page.ptr);
        if text_page.is_null() {
            return Err("Failed to load text page".into());
        }
        let count = b.FPDFText_CountChars(text_page);
        let mut index = 0;
        while index < count {
            let object = b.FPDFText_GetTextObject(text_page, index);
            let mut codepoint = b.FPDFText_GetUnicode(text_page, index);
            let box_index = index;
            if (0xD800..=0xDBFF).contains(&codepoint) && index + 1 < count {
                let low = b.FPDFText_GetUnicode(text_page, index + 1);
                if (0xDC00..=0xDFFF).contains(&low) {
                    codepoint = 0x10000 + ((codepoint - 0xD800) << 10) + (low - 0xDC00);
                    index += 1;
                }
            }
            index += 1;
            if object.is_null() {
                continue;
            }
            let (mut left, mut right, mut bottom, mut top) = (0f64, 0f64, 0f64, 0f64);
            if b.FPDFText_GetCharBox(
                text_page,
                box_index,
                &raw mut left,
                &raw mut right,
                &raw mut bottom,
                &raw mut top,
            ) == 0
            {
                continue;
            }
            let center_x = f64::midpoint(left, right);
            let center_y = f64::midpoint(bottom, top);
            let (mut origin_x, mut origin_y) = (0f64, 0f64);
            b.FPDFText_GetCharOrigin(text_page, box_index, &raw mut origin_x, &raw mut origin_y);
            let redacted = rects
                .iter()
                .any(|&[l, bm, r, t]| (l..=r).contains(&center_x) && (bm..=t).contains(&center_y));
            let char = Char {
                codepoint,
                origin_x,
                origin_y,
                redacted,
            };
            match objects.iter_mut().find(|(o, _)| *o == object) {
                Some((_, chars)) => chars.push(char),
                None => objects.push((object, vec![char])),
            }
        }
        b.FPDFText_ClosePage(text_page);
    }

    for (object, chars) in &objects {
        if !chars.iter().any(|c| c.redacted) {
            continue;
        }
        if !chars.iter().all(|c| c.redacted) {
            rebuild_text_object_survivors(page, *object, chars)?;
        }
        remove_page_object(page, *object)?;
    }
    Ok(())
}

const IDENTITY: FS_MATRIX = FS_MATRIX {
    a: 1.0,
    b: 0.0,
    c: 0.0,
    d: 1.0,
    e: 0.0,
    f: 0.0,
};

/// `rebuild_text_object_survivors`: each character that is not redacted as its own
/// Helvetica text object at its origin, with the object's size and matrix.
fn rebuild_text_object_survivors(
    page: &Page<'_>,
    object: FPDF_PAGEOBJECT,
    chars: &[Char],
) -> Result<(), String> {
    let b = page.b();
    let font = page.doc.standard_font();
    // SAFETY: an object of the loaded page; new objects are inserted or destroyed.
    unsafe {
        let mut font_size = 0f32;
        if b.FPDFTextObj_GetFontSize(object, &raw mut font_size) == 0 {
            font_size = 12.0;
        }
        let mut matrix = IDENTITY;
        if b.FPDFPageObj_GetMatrix(object, &raw mut matrix) == 0 {
            matrix = IDENTITY;
        }
        for char in chars.iter().filter(|c| !c.redacted) {
            let new_object = b.FPDFPageObj_CreateTextObj(page.doc.doc, font, font_size);
            if new_object.is_null() {
                continue;
            }
            let Some(c) = char::from_u32(char.codepoint) else {
                b.FPDFPageObj_Destroy(new_object);
                return Err(format!("invalid codepoint 0x{:X} in UTF-8", char.codepoint));
            };
            let mut text: Vec<FPDF_WCHAR> = c.encode_utf16(&mut [0; 2]).to_vec();
            text.push(0);
            if b.FPDFText_SetText(new_object, text.as_ptr()) == 0 {
                b.FPDFPageObj_Destroy(new_object);
                continue;
            }
            #[allow(clippy::cast_possible_truncation, reason = "FS_MATRIX holds floats")]
            let placed = FS_MATRIX {
                e: char.origin_x as f32,
                f: char.origin_y as f32,
                ..matrix
            };
            b.FPDFPageObj_SetMatrix(new_object, &raw const placed);
            b.FPDFPage_InsertObject(page.ptr, new_object);
        }
    }
    Ok(())
}

/// `redact_image_objects`: image objects under a redaction get the JPEG `encode` writes of
/// their pixels with the redactions drawn in.
fn redact_image_objects(
    page: &Page<'_>,
    rects: &[([f64; 4], bool)],
    encode: EncodeRedacted<'_>,
) -> Result<(), String> {
    let b = page.b();
    // SAFETY: a loaded page.
    let count = unsafe { b.FPDFPage_CountObjects(page.ptr) };
    for index in 0..count {
        // SAFETY: an index below the page's object count.
        let object = unsafe { b.FPDFPage_GetObject(page.ptr, index) };
        if object.is_null() || page.object_type(object) != FPDF_PAGEOBJ_IMAGE {
            continue;
        }
        let Some([obj_left, obj_bottom, obj_right, obj_top]) = page.bounds(object) else {
            continue;
        };
        let overlapping: Vec<([f64; 4], bool)> = rects
            .iter()
            .filter(|([left, bottom, right, top], _)| {
                obj_left < *right && obj_right > *left && obj_bottom < *top && obj_top > *bottom
            })
            .copied()
            .collect();
        if overlapping.is_empty() {
            continue;
        }
        let mut matrix = IDENTITY;
        // SAFETY: an image object and a matrix to fill.
        if unsafe { b.FPDFPageObj_GetMatrix(object, &raw mut matrix) } == 0 {
            return Err("Failed to get image matrix".into());
        }
        let m = [matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f].map(f64::from);
        if (m[0] * m[3] - m[1] * m[2]).abs() < 1e-9 {
            continue;
        }
        let bitmap = extract_image_bitmap(page, object)?;
        let pixel_rects = image_pixel_rects(m, bitmap.width, bitmap.height, &overlapping);
        if pixel_rects.is_empty() {
            continue;
        }
        let jpeg = encode(&bitmap, &pixel_rects);
        load_image_jpeg(page, object, &jpeg)?;
    }
    Ok(())
}

/// `extract_image_bitmap`.
fn extract_image_bitmap(page: &Page<'_>, object: FPDF_PAGEOBJECT) -> Result<ImageBitmap, String> {
    let b = page.b();
    // SAFETY: an image object; the bitmap is destroyed before returning.
    unsafe {
        let bitmap = b.FPDFImageObj_GetBitmap(object);
        if bitmap.is_null() {
            return Err("Failed to get image bitmap".into());
        }
        let format = match b.FPDFBitmap_GetFormat(bitmap) {
            FPDF_BITMAP_GRAY => Some((BitmapFormat::Gray, 1)),
            FPDF_BITMAP_BGR => Some((BitmapFormat::Bgr, 3)),
            FPDF_BITMAP_BGRX => Some((BitmapFormat::Bgrx, 4)),
            FPDF_BITMAP_BGRA => Some((BitmapFormat::Bgra, 4)),
            _ => None,
        };
        let Some((format, bands)) = format else {
            b.FPDFBitmap_Destroy(bitmap);
            return Err("Unsupported image bitmap format".into());
        };
        let width = usize::try_from(b.FPDFBitmap_GetWidth(bitmap)).unwrap_or(0);
        let height = usize::try_from(b.FPDFBitmap_GetHeight(bitmap)).unwrap_or(0);
        let stride = usize::try_from(b.FPDFBitmap_GetStride(bitmap)).unwrap_or(0);
        let buffer = b.FPDFBitmap_GetBuffer(bitmap).cast::<u8>();
        let row_size = width * bands;
        let mut data = Vec::with_capacity(row_size * height);
        if !buffer.is_null() {
            for row in 0..height {
                let line = std::slice::from_raw_parts(buffer.add(row * stride), row_size);
                data.extend_from_slice(line);
            }
        }
        b.FPDFBitmap_Destroy(bitmap);
        Ok(ImageBitmap {
            data,
            width,
            height,
            bands,
            format,
        })
    }
}

/// `image_pixel_rects`: the redactions mapped through the inverse image matrix.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::many_single_char_names,
    reason = "pixel coordinates, as Ruby's Float#floor and #ceil; matrix names as in Rails"
)]
fn image_pixel_rects(
    matrix: [f64; 6],
    width: usize,
    height: usize,
    rects: &[([f64; 4], bool)],
) -> Vec<PixelRect> {
    let [a, b, c, d, e, f] = matrix;
    let det = a * d - b * c;
    let (w, h) = (width as f64, height as f64);
    let (wi, hi) = (
        i64::try_from(width).unwrap_or(i64::MAX),
        i64::try_from(height).unwrap_or(i64::MAX),
    );
    rects
        .iter()
        .filter_map(|&([left, bottom, right, top], white)| {
            let corners =
                [(left, bottom), (right, bottom), (left, top), (right, top)].map(|(x, y)| {
                    let u = (d * (x - e) - c * (y - f)) / det;
                    let v = (a * (y - f) - b * (x - e)) / det;
                    (u * w, (1.0 - v) * h)
                });
            let xs = corners.map(|(x, _)| x);
            let ys = corners.map(|(_, y)| y);
            let (x_min, x_max) = (
                xs.iter().copied().fold(f64::INFINITY, f64::min),
                xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            );
            let (y_min, y_max) = (
                ys.iter().copied().fold(f64::INFINITY, f64::min),
                ys.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            );
            if x_max <= 0.0 || x_min >= w || y_max <= 0.0 || y_min >= h {
                return None;
            }
            let px_left = (x_min.floor() as i64).clamp(0, wi - 1);
            let px_top = (y_min.floor() as i64).clamp(0, hi - 1);
            Some(PixelRect {
                left: px_left,
                top: px_top,
                width: (x_max.ceil() as i64 - px_left).clamp(1, wi - px_left),
                height: (y_max.ceil() as i64 - px_top).clamp(1, hi - px_top),
                white,
            })
        })
        .collect()
}

/// `load_image_jpeg`: `FPDFImageObj_LoadJpegFileInline` with the page.
fn load_image_jpeg(page: &Page<'_>, object: FPDF_PAGEOBJECT, jpeg: &[u8]) -> Result<(), String> {
    unsafe extern "C" fn get_block(
        param: *mut std::ffi::c_void,
        position: std::ffi::c_ulong,
        out: *mut std::ffi::c_uchar,
        size: std::ffi::c_ulong,
    ) -> std::ffi::c_int {
        // SAFETY: `param` is the JPEG slice of the call below; `out` has room for `size`.
        unsafe {
            let data = &*param.cast::<&[u8]>();
            let start = usize::try_from(position)
                .unwrap_or(usize::MAX)
                .min(data.len());
            let end = start
                .saturating_add(usize::try_from(size).unwrap_or(0))
                .min(data.len());
            std::ptr::copy_nonoverlapping(data[start..end].as_ptr(), out, end - start);
        }
        1
    }

    let mut data: &[u8] = jpeg;
    let mut access = FPDF_FILEACCESS {
        m_FileLen: std::ffi::c_ulong::try_from(jpeg.len()).unwrap_or(0),
        m_GetBlock: Some(get_block),
        m_Param: (&raw mut data).cast::<std::ffi::c_void>(),
    };
    let mut pages: [FPDF_PAGE; 1] = [page.ptr];
    // SAFETY: `access` and `data` live until the call returns.
    let ok = unsafe {
        page.b()
            .FPDFImageObj_LoadJpegFileInline(pages.as_mut_ptr(), 1, object, &raw mut access)
    };
    if ok == 0 {
        return Err("Failed to load redacted image".into());
    }
    Ok(())
}

/// `draw_redaction_rects`: a black rectangle for each redaction that is not white.
fn draw_redaction_rects(page: &Page<'_>, rects: &[([f64; 4], bool)]) {
    let b = page.b();
    for &([left, bottom, right, top], white) in rects {
        if white {
            continue;
        }
        // SAFETY: a loaded page; the new object is inserted into it.
        #[allow(clippy::cast_possible_truncation, reason = "FFI takes floats")]
        unsafe {
            let rect = b.FPDFPageObj_CreateNewRect(
                left as f32,
                bottom as f32,
                (right - left) as f32,
                (top - bottom) as f32,
            );
            if rect.is_null() {
                continue;
            }
            b.FPDFPageObj_SetFillColor(rect, 0, 0, 0, 255);
            b.FPDFPath_SetDrawMode(rect, FPDF_FILLMODE_ALTERNATE, 0);
            b.FPDFPage_InsertObject(page.ptr, rect);
        }
    }
}

/// `remove_blank_text_objects`: text objects left with only spaces and controls.
fn remove_blank_text_objects(page: &Page<'_>) -> Result<(), String> {
    let b = page.b();
    let mut blanks = Vec::new();
    // SAFETY: a loaded page; the text page is closed below.
    unsafe {
        let text_page = b.FPDFText_LoadPage(page.ptr);
        if text_page.is_null() {
            return Ok(());
        }
        let mut result = Ok(());
        for object in page.objects() {
            if page.object_type(object) != FPDF_PAGEOBJ_TEXT {
                continue;
            }
            let needed = b.FPDFTextObj_GetText(object, text_page, std::ptr::null_mut(), 0);
            if needed < 2 {
                continue;
            }
            let mut buffer: Vec<FPDF_WCHAR> = vec![0; usize::try_from(needed).unwrap_or(0) / 2 + 1];
            let written = b.FPDFTextObj_GetText(object, text_page, buffer.as_mut_ptr(), needed);
            if written < 2 {
                continue;
            }
            let units = (usize::try_from(written).unwrap_or(0) - 2) / 2;
            match String::from_utf16(&buffer[..units]) {
                Ok(content) => {
                    if content
                        .chars()
                        .all(|c| BLANK_TEXT_CODEPOINTS.contains(&u32::from(c)))
                    {
                        blanks.push(object);
                    }
                }
                Err(err) => {
                    result = Err(format!("invalid text in UTF-16LE: {err}"));
                    break;
                }
            }
        }
        b.FPDFText_ClosePage(text_page);
        result?;
    }
    if blanks.is_empty() {
        return Ok(());
    }
    for object in blanks {
        remove_page_object(page, object)?;
    }
    // SAFETY: a loaded page.
    unsafe { b.FPDFPage_GenerateContent(page.ptr) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_redactions_into_image_pixels() {
        // An image drawn 100 × 50 at (10, 20).
        let matrix = [100.0, 0.0, 0.0, 50.0, 10.0, 20.0];
        let rects = image_pixel_rects(
            matrix,
            200,
            100,
            &[
                ([10.0, 20.0, 60.0, 45.0], false),
                ([500.0, 500.0, 600.0, 600.0], true),
            ],
        );
        assert_eq!(
            rects,
            [PixelRect {
                left: 0,
                top: 50,
                width: 100,
                height: 50,
                white: false,
            }]
        );
    }
}
