//! `Pdfium::Page#text_objects`: the text objects of each page with their normalized
//! bounds, for `DocumentMetadatas.build_text_runs`.
//!
//! This calls `PDFium` directly: `PdfPageTextObject::text` loads the page's text page for
//! every object, where Rails loads it once per page.
#![allow(
    unsafe_code,
    reason = "PDFium FFI calls on handles this module opens and closes"
)]

use std::cmp::Ordering;

use pdfium_render::prelude::{
    FPDF_PAGEOBJ_TEXT, Pdfium, PdfiumLibraryBindings, PdfiumLibraryBindingsAccessor,
};

use crate::PdfError;

/// `Pdfium::TextObject`: bounds are fractions of the page size, `y` from the top.
#[derive(Clone, Debug, PartialEq)]
pub struct TextObject {
    pub content: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub font_size: FontSize,
}

/// `font_size`: the object's size, or Ruby's Integer `8` for a size of 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FontSize {
    Float(f64),
    Eight,
}

impl TextObject {
    fn endy(&self) -> f64 {
        self.y + self.h
    }
}

/// The text objects of every page, sorted into reading order as Rails sorts them.
pub fn text_objects(pdfium: &Pdfium, bytes: &[u8]) -> Result<Vec<Vec<TextObject>>, PdfError> {
    let bindings = pdfium.bindings();
    // SAFETY: the document, pages and text pages are closed before returning, and the
    // byte slice outlives the document.
    unsafe {
        let document = bindings.FPDF_LoadMemDocument64(bytes, None);
        if document.is_null() {
            return Err(PdfError::Unavailable(
                "Failed to load document from memory".into(),
            ));
        }
        let count = bindings.FPDF_GetPageCount(document);
        let mut pages = Vec::new();
        for index in 0..count {
            let page = bindings.FPDF_LoadPage(document, index);
            if page.is_null() {
                bindings.FPDF_CloseDocument(document);
                return Err(PdfError::Unavailable(format!(
                    "Failed to load page {index}"
                )));
            }
            let objects = page_text_objects(bindings, page);
            bindings.FPDF_ClosePage(page);
            pages.push(objects);
        }
        bindings.FPDF_CloseDocument(document);
        Ok(pages)
    }
}

unsafe fn page_text_objects(
    bindings: &dyn PdfiumLibraryBindings,
    page: pdfium_render::prelude::FPDF_PAGE,
) -> Vec<TextObject> {
    let mut objects = Vec::new();
    // SAFETY: `page` is a loaded page; the text page is closed below.
    unsafe {
        let count = bindings.FPDFPage_CountObjects(page);
        if count == 0 {
            return objects;
        }
        let width = f64::from(bindings.FPDF_GetPageWidthF(page));
        let height = f64::from(bindings.FPDF_GetPageHeightF(page));
        let text_page = bindings.FPDFText_LoadPage(page);
        if text_page.is_null() {
            return objects;
        }
        for i in 0..count {
            let object = bindings.FPDFPage_GetObject(page, i);
            if object.is_null()
                || bindings.FPDFPageObj_GetType(object) != FPDF_PAGEOBJ_TEXT.cast_signed()
            {
                continue;
            }
            let needed = bindings.FPDFTextObj_GetText(object, text_page, std::ptr::null_mut(), 0);
            if needed < 4 {
                continue;
            }
            let mut buffer = vec![0_u16; usize::try_from(needed).unwrap_or(0).div_ceil(2)];
            let written =
                bindings.FPDFTextObj_GetText(object, text_page, buffer.as_mut_ptr(), needed);
            if written < 4 {
                continue;
            }
            // `read_bytes(written - 2).force_encoding('UTF-16LE').encode('UTF-8')`
            let units =
                &buffer[..(usize::try_from(written - 2).unwrap_or(0) / 2).min(buffer.len())];
            let content = String::from_utf16_lossy(units);
            if content.is_empty() {
                continue;
            }
            let (mut left, mut bottom, mut right, mut top) = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
            if bindings.FPDFPageObj_GetBounds(
                object,
                &raw mut left,
                &raw mut bottom,
                &raw mut right,
                &raw mut top,
            ) == 0
            {
                continue;
            }
            let (left, bottom, right, top) = (
                f64::from(left),
                f64::from(bottom),
                f64::from(right),
                f64::from(top),
            );
            let obj_width = right - left;
            let obj_height = top - bottom;
            if obj_width <= 0.0 || obj_height <= 0.0 {
                continue;
            }
            let mut size = 0.0_f32;
            let font_size = if bindings.FPDFTextObj_GetFontSize(object, &raw mut size) == 0 {
                obj_height
            } else {
                f64::from(size)
            };
            #[allow(clippy::float_cmp, reason = "Ruby's `font_size == 1`")]
            let font_size = if font_size == 1.0 {
                FontSize::Eight
            } else {
                FontSize::Float(font_size)
            };
            objects.push(TextObject {
                content,
                x: left / width,
                y: (height - top) / height,
                w: obj_width / width,
                h: obj_height / height,
                font_size,
            });
        }
        bindings.FPDFText_ClosePage(text_page);
        let y_threshold = 4.0 / width;
        merge_sort(&mut objects, &|a: &TextObject, b: &TextObject| {
            if (a.endy() - b.endy()).abs() < y_threshold {
                a.x.partial_cmp(&b.x).unwrap_or(Ordering::Equal)
            } else {
                a.endy().partial_cmp(&b.endy()).unwrap_or(Ordering::Equal)
            }
        });
    }
    objects
}

/// A stable merge sort that accepts a comparison that is not a total order (the line
/// threshold above is not transitive), where `slice::sort_by` may panic.
fn merge_sort<T: Clone>(items: &mut [T], cmp: &dyn Fn(&T, &T) -> Ordering) {
    if items.len() < 2 {
        return;
    }
    let mid = items.len() / 2;
    merge_sort(&mut items[..mid], cmp);
    merge_sort(&mut items[mid..], cmp);
    let mut merged = Vec::with_capacity(items.len());
    let (mut i, mut j) = (0, mid);
    while i < mid && j < items.len() {
        if cmp(&items[j], &items[i]) == Ordering::Less {
            merged.push(items[j].clone());
            j += 1;
        } else {
            merged.push(items[i].clone());
            i += 1;
        }
    }
    merged.extend_from_slice(&items[i..mid]);
    merged.extend_from_slice(&items[j..]);
    items.clone_from_slice(&merged);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_sort_is_stable() {
        let mut items = vec![(1, 'a'), (0, 'b'), (1, 'c'), (0, 'd')];
        merge_sort(&mut items, &|a: &(i32, char), b: &(i32, char)| {
            a.0.cmp(&b.0)
        });
        assert_eq!(items, vec![(0, 'b'), (0, 'd'), (1, 'a'), (1, 'c')]);
    }
}
