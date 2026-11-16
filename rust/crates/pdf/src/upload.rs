//! The `PDFium` work of a template upload (`Templates::CreateAttachments#handle_pdf_or_image`
//! and `Templates::ProcessDocument`): opening and decrypting the PDF, its external links
//! (`Templates::BuildPdfiumAnnotations`), its form fields (`Templates::FindPdfiumAcroFields`)
//! and the page bitmaps of `preview_images`.
//!
//! This calls `PDFium` directly, as `lib/pdfium.rb` does. Rails' `PDFium` build has
//! "raw" variants of a few form field functions (`FPDFAnnot_GetFormFieldValueRaw`,
//! `…ExportValueRaw`, `FPDFAnnot_IsCheckedRaw`) and `FPDFPage_FixFormFields`; the public
//! builds don't, so the standard functions are used.
#![allow(
    unsafe_code,
    reason = "PDFium FFI calls on handles this module opens and closes"
)]

use std::collections::HashMap;

use pdfium_render::prelude::{
    FPDF_ANNOTATION, FPDF_DOCUMENT, FPDF_FILEWRITE, FPDF_FORMFILLINFO, FPDF_FORMHANDLE,
    FPDF_PAGE, FPDF_WCHAR, FS_RECTF, Pdfium, PdfiumLibraryBindings,
    PdfiumLibraryBindingsAccessor,
};
use regex::Regex;
use serde_json::{Map, Value, json};

const FPDF_ANNOT: i32 = 0x01;
const FPDF_LCD_TEXT: i32 = 0x02;
const FPDF_NO_NATIVETEXT: i32 = 0x04;
const FPDF_REVERSE_BYTE_ORDER: i32 = 0x10;
const FPDF_ANNOT_LINK: i32 = 2;
const FPDF_ANNOT_WIDGET: i32 = 20;
const FPDF_ANNOT_FLAG_HIDDEN: i32 = 2;
const FPDF_ERR_PASSWORD: std::ffi::c_ulong = 4;
const FPDF_REMOVE_SECURITY: std::ffi::c_ulong = 3;
const FORMTYPE_NONE: i32 = 0;
const PDFACTION_URI: std::ffi::c_ulong = 3;
const FLATTEN_FAIL: i32 = 0;
const FLATTEN_SUCCESS: i32 = 1;
const FPDF_ANNOT_AACTION_KEY_STROKE: i32 = 12;
const FPDF_ANNOT_AACTION_FORMAT: i32 = 13;

const FPDF_FORMFIELD_PUSHBUTTON: i32 = 1;
const FPDF_FORMFIELD_CHECKBOX: i32 = 2;
const FPDF_FORMFIELD_RADIOBUTTON: i32 = 3;
const FPDF_FORMFIELD_COMBOBOX: i32 = 4;
const FPDF_FORMFIELD_LISTBOX: i32 = 5;
const FPDF_FORMFIELD_TEXTFIELD: i32 = 6;
const FPDF_FORMFIELD_SIGNATURE: i32 = 7;

const FPDF_FORMFLAG_REQUIRED: i32 = 1 << 1;
const FPDF_FORMFLAG_TEXT_PASSWORD: i32 = 1 << 13;
const FPDF_FORMFLAG_BUTTON_PUSH: i32 = 1 << 16;
const FPDF_FORMFLAG_TEXT_FILE_SELECT: i32 = 1 << 20;
const FPDF_FORMFLAG_TEXT_COMB: i32 = 1 << 24;
const FPDF_FORMFLAG_TEXT_RICH_TEXT: i32 = 1 << 25;
const TEXT_TYPE_FLAGS: i32 =
    FPDF_FORMFLAG_TEXT_FILE_SELECT | FPDF_FORMFLAG_TEXT_PASSWORD | FPDF_FORMFLAG_TEXT_RICH_TEXT;

/// `Pdfium::MAX_SIZE`.
const MAX_SIZE: i64 = 32_767;
/// `Templates::ProcessDocument::MAX_WIDTH`.
pub const MAX_WIDTH: i32 = 1400;
/// `Templates::CreateAttachments::ANNOTATIONS_SIZE_LIMIT`.
const ANNOTATIONS_SIZE_LIMIT: usize = 6 * 1024 * 1024;
/// `Templates::ProcessDocument::MAX_FLATTEN_FILE_SIZE`.
const MAX_FLATTEN_FILE_SIZE: usize = 20 * 1024 * 1024;
/// `Templates::ProcessDocument::GENERATE_PREVIEW_SIZE_LIMIT`.
const GENERATE_PREVIEW_SIZE_LIMIT: usize = 50 * 1024 * 1024;
/// `Templates::ProcessDocument::MAX_NUMBER_OF_PAGES_PROCESSED`.
const MAX_NUMBER_OF_PAGES_PROCESSED: usize = 15;
/// `Templates::BuildPdfiumAnnotations::LINKS_LIMIT`.
const LINKS_LIMIT: usize = 250;

/// Why a PDF could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// `Pdfium::PasswordError` (`Templates::CreateAttachments::PdfEncrypted`).
    #[error("PDF encrypted")]
    Password,
    #[error("{0}")]
    Pdfium(String),
}

/// A rendered page: RGBA rows, `width * 4` bytes each.
#[derive(Clone, Debug)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// What an upload of a PDF learns from `PDFium`.
#[derive(Debug)]
pub struct ProcessedPdf {
    /// The stored bytes: the upload, or its decrypted copy (`decrypt_document`).
    pub data: Vec<u8>,
    /// `metadata.pdf.annotations` (external links).
    pub annotations: Vec<Value>,
    /// `metadata.pdf.fields`, when fields were extracted.
    pub fields: Option<Vec<Value>>,
    pub number_of_pages: usize,
    /// The `preview_images` bitmaps, or the error that stopped rendering them.
    pub previews: Result<Vec<Bitmap>, String>,
}

/// `handle_pdf_or_image` and `ProcessDocument.call` for a PDF: decrypts it, reads its
/// links and (with `extract_fields`) its form fields, and renders the preview pages.
/// `attachment_uuid` goes into the field areas.
pub fn process(
    pdfium: &Pdfium,
    bytes: &[u8],
    password: Option<&str>,
    attachment_uuid: &str,
    extract_fields: bool,
) -> Result<ProcessedPdf, OpenError> {
    let mut doc = Document::open(pdfium.bindings(), bytes, password)?;

    let data = if doc.encrypted() {
        doc.save_without_security()
            .ok_or_else(|| OpenError::Pdfium("Failed to save document".into()))?
    } else {
        bytes.to_vec()
    };

    let annotations = if data.len() < ANNOTATIONS_SIZE_LIMIT {
        build_annotations(&mut doc)
    } else {
        Vec::new()
    };

    let fields = (extract_fields && data.len() < MAX_FLATTEN_FILE_SIZE)
        .then(|| find_acro_fields(&mut doc, &data, attachment_uuid));

    let number_of_pages = doc.page_count();
    let max_pages = if data.len() < GENERATE_PREVIEW_SIZE_LIMIT {
        MAX_NUMBER_OF_PAGES_PROCESSED
    } else {
        1
    };
    let last = number_of_pages
        .checked_sub(1)
        .map(|last| last.min(max_pages));
    let previews = match last {
        Some(last) => (0..=last).map(|index| doc.preview(index)).collect(),
        None => Ok(Vec::new()),
    };

    Ok(ProcessedPdf {
        data,
        annotations,
        fields,
        number_of_pages,
        previews,
    })
}

/// `Pdfium::Document`, with the pages it has loaded.
struct Document<'a> {
    bindings: &'a dyn PdfiumLibraryBindings,
    doc: FPDF_DOCUMENT,
    form: FPDF_FORMHANDLE,
    // Must outlive `form`.
    _form_info: Box<FPDF_FORMFILLINFO>,
    pages: HashMap<usize, Page>,
}

struct Page {
    ptr: FPDF_PAGE,
    view_loaded: bool,
    annotations: Option<Vec<Annotation>>,
}

#[derive(Clone, Debug)]
struct Annotation {
    page_index: usize,
    index: i32,
    subtype: i32,
    /// `[left, bottom, right, top]`, empty without a rect.
    bounds: Vec<f64>,
}

#[derive(Clone, Copy, Debug)]
struct Geometry {
    box_: [f64; 4],
    rotation: i32,
}

impl<'a> Document<'a> {
    fn open(
        bindings: &'a dyn PdfiumLibraryBindings,
        bytes: &'a [u8],
        password: Option<&str>,
    ) -> Result<Self, OpenError> {
        // SAFETY: `bytes` outlives the document (`process` closes it before returning);
        // the form fill info is boxed and freed after the form handle is closed.
        unsafe {
            let doc = bindings.FPDF_LoadMemDocument64(bytes, password);
            if doc.is_null() {
                let code = bindings.FPDF_GetLastError();
                return Err(if code == FPDF_ERR_PASSWORD {
                    OpenError::Password
                } else {
                    OpenError::Pdfium(format!(
                        "Failed to load document from memory (Code: {code})"
                    ))
                });
            }
            let mut form_info: Box<FPDF_FORMFILLINFO> = Box::new(std::mem::zeroed());
            form_info.version = 2;
            let form = bindings.FPDFDOC_InitFormFillEnvironment(doc, &raw mut *form_info);
            Ok(Self {
                bindings,
                doc,
                form,
                _form_info: form_info,
                pages: HashMap::new(),
            })
        }
    }

    fn page_count(&self) -> usize {
        // SAFETY: an open document.
        usize::try_from(unsafe { self.bindings.FPDF_GetPageCount(self.doc) }).unwrap_or(0)
    }

    fn encrypted(&self) -> bool {
        // SAFETY: an open document.
        unsafe { self.bindings.FPDF_GetSecurityHandlerRevision(self.doc) >= 0 }
    }

    fn is_form(&self) -> bool {
        // SAFETY: an open document.
        unsafe { self.bindings.FPDF_GetFormType(self.doc) != FORMTYPE_NONE }
    }

    /// `doc.save(io, flags: Pdfium::FPDF_REMOVE_SECURITY)`.
    fn save_without_security(&self) -> Option<Vec<u8>> {
        #[repr(C)]
        struct Writer {
            base: FPDF_FILEWRITE,
            out: Vec<u8>,
        }

        unsafe extern "C" fn write_block(
            this: *mut FPDF_FILEWRITE,
            data: *const std::ffi::c_void,
            size: std::ffi::c_ulong,
        ) -> std::ffi::c_int {
            // SAFETY: `this` is the `base` of a `Writer` (first field, `repr(C)`), and
            // `data` points to `size` bytes.
            unsafe {
                let writer = &mut *this.cast::<Writer>();
                let len = usize::try_from(size).unwrap_or(0);
                writer
                    .out
                    .extend_from_slice(std::slice::from_raw_parts(data.cast::<u8>(), len));
            }
            1
        }

        let mut writer = Writer {
            base: FPDF_FILEWRITE {
                version: 1,
                WriteBlock: Some(write_block),
            },
            out: Vec::new(),
        };
        // SAFETY: `writer` lives until `FPDF_SaveAsCopy` returns.
        let ok = unsafe {
            self.bindings.FPDF_SaveAsCopy(
                self.doc,
                (&raw mut writer).cast::<FPDF_FILEWRITE>(),
                FPDF_REMOVE_SECURITY,
            )
        };
        (ok != 0).then_some(writer.out)
    }

    /// `doc.annot_count(page_index)` (Rails counts without loading the page).
    fn annot_count(&mut self, page_index: usize) -> i32 {
        let Some(page) = self.page(page_index) else {
            return 0;
        };
        // SAFETY: a loaded page.
        unsafe { self.bindings.FPDFPage_GetAnnotCount(page) }
    }

    /// `doc.get_page(index)`: loaded once and kept until closed.
    fn page(&mut self, page_index: usize) -> Option<FPDF_PAGE> {
        if let Some(page) = self.pages.get(&page_index) {
            return Some(page.ptr);
        }
        let index = i32::try_from(page_index).ok()?;
        // SAFETY: an open document; the page is closed in `close_page` or `drop`.
        let ptr = unsafe { self.bindings.FPDF_LoadPage(self.doc, index) };
        if ptr.is_null() {
            return None;
        }
        self.pages.insert(
            page_index,
            Page {
                ptr,
                view_loaded: false,
                annotations: None,
            },
        );
        Some(ptr)
    }

    fn close_page(&mut self, page_index: usize) {
        if let Some(page) = self.pages.remove(&page_index) {
            self.release(&page);
        }
    }

    fn release(&self, page: &Page) {
        // SAFETY: a loaded page, closed once.
        unsafe {
            if !self.form.is_null() {
                self.bindings.FORM_OnBeforeClosePage(page.ptr, self.form);
            }
            self.bindings.FPDF_ClosePage(page.ptr);
        }
    }

    /// `page.box`: the bounding box, else US Letter.
    fn geometry(&self, page: FPDF_PAGE) -> Geometry {
        let mut rect = FS_RECTF {
            left: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
        };
        // SAFETY: a loaded page.
        let (found, rotation) = unsafe {
            (
                self.bindings.FPDF_GetPageBoundingBox(page, &raw mut rect) == 1,
                self.bindings.FPDFPage_GetRotation(page),
            )
        };
        let box_ = found
            .then(|| {
                let (x0, x1) = minmax(rect.left, rect.right);
                let (y0, y1) = minmax(rect.bottom, rect.top);
                (x1 - x0 > 0.0 && y1 - y0 > 0.0).then_some([x0, y0, x1, y1])
            })
            .flatten()
            .unwrap_or([0.0, 0.0, 612.0, 792.0]);
        Geometry { box_, rotation }
    }

    /// `page.annotations`: subtype and rect of each annotation.
    fn annotations(&mut self, page_index: usize) -> Vec<Annotation> {
        let Some(page) = self.page(page_index) else {
            return Vec::new();
        };
        if let Some(annotations) = self.pages.get(&page_index).and_then(|p| p.annotations.clone())
        {
            return annotations;
        }
        let mut annotations = Vec::new();
        // SAFETY: a loaded page; each annotation handle is closed after use.
        unsafe {
            for index in 0..self.bindings.FPDFPage_GetAnnotCount(page) {
                let annot = self.bindings.FPDFPage_GetAnnot(page, index);
                if annot.is_null() {
                    continue;
                }
                let subtype = self.bindings.FPDFAnnot_GetSubtype(annot);
                let mut rect = FS_RECTF {
                    left: 0.0,
                    top: 0.0,
                    right: 0.0,
                    bottom: 0.0,
                };
                let bounds = if self.bindings.FPDFAnnot_GetRect(annot, &raw mut rect) == 1 {
                    let (left, right) = minmax(rect.left, rect.right);
                    let (bottom, top) = minmax(rect.bottom, rect.top);
                    vec![left, bottom, right, top]
                } else {
                    Vec::new()
                };
                self.bindings.FPDFPage_CloseAnnot(annot);
                annotations.push(Annotation {
                    page_index,
                    index,
                    subtype,
                    bounds,
                });
            }
        }
        if let Some(p) = self.pages.get_mut(&page_index) {
            p.annotations = Some(annotations.clone());
        }
        annotations
    }

    /// `page.with_annotation(index) { |handle| … }`.
    fn with_annotation<R>(
        &mut self,
        page_index: usize,
        index: i32,
        f: impl FnOnce(&Handle<'_>) -> R,
    ) -> Option<R> {
        let page = self.page(page_index)?;
        // SAFETY: a loaded page; the handle is closed below.
        unsafe {
            let annot = self.bindings.FPDFPage_GetAnnot(page, index);
            if annot.is_null() {
                return None;
            }
            let result = f(&Handle {
                bindings: self.bindings,
                doc: self.doc,
                form: self.form,
                annot,
            });
            self.bindings.FPDFPage_CloseAnnot(annot);
            Some(result)
        }
    }

    /// One `preview_images` page: placeholder selects hidden, form fields flattened,
    /// rendered `MAX_WIDTH` wide. The page is closed afterwards, as in Rails.
    fn preview(&mut self, page_index: usize) -> Result<Bitmap, String> {
        let result = self.render_preview(page_index);
        self.close_page(page_index);
        result
    }

    fn render_preview(&mut self, page_index: usize) -> Result<Bitmap, String> {
        let flatten_pages = self.is_form();
        if self.page(page_index).is_none() {
            return Err(format!("Failed to load page {page_index}"));
        }
        self.hide_placeholder_widgets(page_index, !flatten_pages);
        if flatten_pages {
            self.flatten(page_index)?;
        }
        let page = self
            .page(page_index)
            .ok_or_else(|| format!("Failed to reload page {page_index}"))?;
        // SAFETY: a loaded page; the bitmap is destroyed below.
        unsafe {
            let width = f64::from(self.bindings.FPDF_GetPageWidthF(page));
            let height = f64::from(self.bindings.FPDF_GetPageHeightF(page));
            let scale = f64::from(MAX_WIDTH) / width;
            let render_width = i64::from(MAX_WIDTH).clamp(1, MAX_SIZE);
            #[allow(clippy::cast_possible_truncation, reason = "clamped to MAX_SIZE")]
            let render_height = ((height * scale).round() as i64).clamp(1, MAX_SIZE);
            let (w, h) = (
                i32::try_from(render_width).unwrap_or(1),
                i32::try_from(render_height).unwrap_or(1),
            );
            let bitmap = self.bindings.FPDFBitmap_Create(w, h, 1);
            if bitmap.is_null() {
                return Err("Failed to create bitmap (FPDFBitmap_Create returned NULL)".into());
            }
            let flags = FPDF_ANNOT | FPDF_LCD_TEXT | FPDF_NO_NATIVETEXT | FPDF_REVERSE_BYTE_ORDER;
            #[allow(clippy::let_unit_value, reason = "returns a BOOL in newer bindings")]
            let _ = self
                .bindings
                .FPDFBitmap_FillRect(bitmap, 0, 0, w, h, 0xFFFF_FFFF);
            self.bindings
                .FPDF_RenderPageBitmap(bitmap, page, 0, 0, w, h, 0, flags);
            if !self.form.is_null() {
                self.bindings
                    .FPDF_FFLDraw(self.form, bitmap, page, 0, 0, w, h, 0, flags);
            }
            let stride = usize::try_from(self.bindings.FPDFBitmap_GetStride(bitmap)).unwrap_or(0);
            let buffer = self.bindings.FPDFBitmap_GetBuffer(bitmap).cast::<u8>();
            let row = usize::try_from(w).unwrap_or(0) * 4;
            let rows = usize::try_from(h).unwrap_or(0);
            let mut rgba = Vec::with_capacity(row * rows);
            for y in 0..rows {
                rgba.extend_from_slice(std::slice::from_raw_parts(buffer.add(y * stride), row));
            }
            self.bindings.FPDFBitmap_Destroy(bitmap);
            Ok(Bitmap {
                width: u32::try_from(w).unwrap_or(1),
                height: u32::try_from(h).unwrap_or(1),
                rgba,
            })
        }
    }

    /// `ProcessDocument.hide_placeholder_widgets`: hides select boxes showing a
    /// "Select…" placeholder, and empty ones when `hide_empty`.
    fn hide_placeholder_widgets(&mut self, page_index: usize, hide_empty: bool) {
        for annotation in self.annotations(page_index) {
            if annotation.subtype != FPDF_ANNOT_WIDGET {
                continue;
            }
            self.with_annotation(page_index, annotation.index, |handle| {
                if handle.field_type() != FPDF_FORMFIELD_COMBOBOX {
                    return;
                }
                let value = handle.field_value().unwrap_or_default();
                if value.trim().is_empty() {
                    if !hide_empty {
                        return;
                    }
                } else if handle.option_labels().is_empty()
                    || !select_placeholder().is_match(&value)
                {
                    return;
                }
                handle.hide();
            });
        }
    }

    /// `Page#load_page_view`.
    fn load_page_view(&mut self, page_index: usize) {
        if self.form.is_null() {
            return;
        }
        if let Some(p) = self.pages.get_mut(&page_index)
            && !p.view_loaded
        {
            // SAFETY: a loaded page and an open form handle.
            unsafe { self.bindings.FORM_OnAfterLoadPage(p.ptr, self.form) };
            p.view_loaded = true;
        }
    }

    /// `Page#flatten(FLAT_NORMALDISPLAY)`, reloading the page when it changed.
    fn flatten(&mut self, page_index: usize) -> Result<(), String> {
        let page = self
            .page(page_index)
            .ok_or_else(|| format!("Failed to load page {page_index}"))?;
        let geometry = self.geometry(page);
        self.load_page_view(page_index);
        // SAFETY: a loaded page.
        let result = unsafe {
            #[allow(clippy::cast_possible_truncation, reason = "page boxes are f32")]
            let [left, bottom, right, top] = geometry.box_.map(|v| v as f32);
            let (mut l, mut b, mut r, mut t) = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
            if self.bindings.FPDFPage_GetMediaBox(
                page,
                &raw mut l,
                &raw mut b,
                &raw mut r,
                &raw mut t,
            ) == 0
            {
                self.bindings
                    .FPDFPage_SetMediaBox(page, left, bottom, right, top);
            }
            if self
                .bindings
                .FPDFPage_GetCropBox(page, &raw mut l, &raw mut b, &raw mut r, &raw mut t)
                == 0
            {
                self.bindings
                    .FPDFPage_SetCropBox(page, left, bottom, right, top);
            }
            self.bindings.FPDFPage_Flatten(page, 0)
        };
        if result == FLATTEN_FAIL {
            return Err(format!("Failed to flatten page {page_index}"));
        }
        if result == FLATTEN_SUCCESS {
            self.close_page(page_index);
            self.page(page_index)
                .ok_or_else(|| format!("Failed to reload page {page_index}"))?;
        }
        Ok(())
    }
}

impl Drop for Document<'_> {
    fn drop(&mut self) {
        let pages: Vec<Page> = self.pages.drain().map(|(_, page)| page).collect();
        for page in &pages {
            self.release(page);
        }
        // SAFETY: the document and form handle are closed once, after their pages.
        unsafe {
            if !self.form.is_null() {
                self.bindings.FPDFDOC_ExitFormFillEnvironment(self.form);
            }
            self.bindings.FPDF_CloseDocument(self.doc);
        }
    }
}

fn minmax(a: f32, b: f32) -> (f64, f64) {
    let (a, b) = (f64::from(a), f64::from(b));
    if a <= b { (a, b) } else { (b, a) }
}

/// `Pdfium::AnnotationHandle`.
struct Handle<'a> {
    bindings: &'a dyn PdfiumLibraryBindings,
    doc: FPDF_DOCUMENT,
    form: FPDF_FORMHANDLE,
    annot: FPDF_ANNOTATION,
}

impl Handle<'_> {
    fn field_type(&self) -> i32 {
        // SAFETY: an open annotation handle.
        unsafe {
            self.bindings
                .FPDFAnnot_GetFormFieldType(self.form, self.annot)
        }
    }

    fn field_flags(&self) -> i32 {
        // SAFETY: an open annotation handle.
        unsafe {
            self.bindings
                .FPDFAnnot_GetFormFieldFlags(self.form, self.annot)
        }
    }

    fn control_count(&self) -> i32 {
        // SAFETY: an open annotation handle.
        unsafe {
            self.bindings
                .FPDFAnnot_GetFormControlCount(self.form, self.annot)
        }
    }

    fn control_index(&self) -> i32 {
        // SAFETY: an open annotation handle.
        unsafe {
            self.bindings
                .FPDFAnnot_GetFormControlIndex(self.form, self.annot)
        }
    }

    fn checked(&self) -> bool {
        // SAFETY: an open annotation handle.
        unsafe { self.bindings.FPDFAnnot_IsChecked(self.form, self.annot) == 1 }
    }

    fn field_name(&self) -> Option<String> {
        // SAFETY: an open annotation handle and a buffer of the requested size.
        read_wide(|buf, len| unsafe {
            self.bindings
                .FPDFAnnot_GetFormFieldName(self.form, self.annot, buf, len)
        })
    }

    fn alternate_name(&self) -> Option<String> {
        // SAFETY: as above.
        read_wide(|buf, len| unsafe {
            self.bindings
                .FPDFAnnot_GetFormFieldAlternateName(self.form, self.annot, buf, len)
        })
    }

    fn field_value(&self) -> Option<String> {
        // SAFETY: as above.
        read_wide(|buf, len| unsafe {
            self.bindings
                .FPDFAnnot_GetFormFieldValue(self.form, self.annot, buf, len)
        })
    }

    fn export_value(&self) -> Option<String> {
        // SAFETY: as above.
        read_wide(|buf, len| unsafe {
            self.bindings
                .FPDFAnnot_GetFormFieldExportValue(self.form, self.annot, buf, len)
        })
    }

    fn option_labels(&self) -> Vec<String> {
        // SAFETY: as above.
        let count = unsafe {
            self.bindings
                .FPDFAnnot_GetOptionCount(self.form, self.annot)
        };
        (0..count.max(0))
            .map(|index| {
                // SAFETY: as above.
                read_wide(|buf, len| unsafe {
                    self.bindings
                        .FPDFAnnot_GetOptionLabel(self.form, self.annot, index, buf, len)
                })
                .unwrap_or_default()
            })
            .collect()
    }

    fn action_javascript(&self, event: i32) -> Option<String> {
        // SAFETY: as above.
        read_wide(|buf, len| unsafe {
            self.bindings.FPDFAnnot_GetFormAdditionalActionJavaScript(
                self.form, self.annot, event, buf, len,
            )
        })
    }

    fn string_value(&self, key: &str) -> Option<String> {
        // SAFETY: as above.
        read_wide(|buf, len| unsafe {
            self.bindings
                .FPDFAnnot_GetStringValue(self.annot, key, buf, len)
        })
    }

    fn number_value(&self, key: &str) -> Option<f64> {
        let mut value = 0.0_f32;
        // SAFETY: an open annotation handle.
        let ok = unsafe {
            self.bindings
                .FPDFAnnot_GetNumberValue(self.annot, key, &raw mut value)
        };
        (ok == 1).then_some(f64::from(value))
    }

    fn appearance(&self) -> Option<String> {
        // SAFETY: as above.
        read_wide(|buf, len| unsafe { self.bindings.FPDFAnnot_GetAP(self.annot, 0, buf, len) })
    }

    fn link_uri(&self) -> Option<String> {
        // SAFETY: an open annotation handle; the link and action belong to it.
        unsafe {
            let link = self.bindings.FPDFAnnot_GetLink(self.annot);
            if link.is_null() {
                return None;
            }
            let action = self.bindings.FPDFLink_GetAction(link);
            if action.is_null() || self.bindings.FPDFAction_GetType(action) != PDFACTION_URI
            {
                return None;
            }
            let length = self
                .bindings
                .FPDFAction_GetURIPath(self.doc, action, std::ptr::null_mut(), 0);
            if length <= 1 {
                return None;
            }
            let mut buffer = vec![0_u8; usize::try_from(length).unwrap_or(0)];
            let length = self.bindings.FPDFAction_GetURIPath(
                self.doc,
                action,
                buffer.as_mut_ptr().cast(),
                length,
            );
            if length <= 1 {
                return None;
            }
            buffer.truncate(usize::try_from(length - 1).unwrap_or(0));
            // `force_encoding('UTF-8').scrub`
            Some(String::from_utf8_lossy(&buffer).into_owned())
        }
    }

    fn hide(&self) {
        // SAFETY: an open annotation handle.
        unsafe {
            let flags = self.bindings.FPDFAnnot_GetFlags(self.annot);
            self.bindings
                .FPDFAnnot_SetFlags(self.annot, flags | FPDF_ANNOT_FLAG_HIDDEN);
        }
    }
}

/// `Pdfium.read_wide_string`: UTF-16LE without the terminator; invalid units dropped.
fn read_wide(mut call: impl FnMut(*mut FPDF_WCHAR, std::ffi::c_ulong) -> std::ffi::c_ulong) -> Option<String> {
    let length = call(std::ptr::null_mut(), 0);
    if length <= 2 {
        return None;
    }
    let mut buffer = vec![0_u16; usize::try_from(length).unwrap_or(0).div_ceil(2)];
    let length = call(buffer.as_mut_ptr(), length);
    if length <= 2 {
        return None;
    }
    let units = usize::try_from(length - 2).unwrap_or(0) / 2;
    Some(
        char::decode_utf16(buffer.into_iter().take(units))
            .filter_map(Result::ok)
            .collect(),
    )
}

/// `Pdfium.transform_rect`: `[x, y, w, h, page_width, page_height]`, `y` from the top of
/// the page as displayed.
#[must_use]
pub fn transform_rect(bounds: &[f64], box_: [f64; 4], rotation: i32) -> [f64; 6] {
    let [x0, y0, x1, y1] = [bounds[0], bounds[1], bounds[2], bounds[3]];
    let [bx0, by0, bx1, by1] = box_;
    let box_width = bx1 - bx0;
    let box_height = by1 - by0;
    let left = x0 - bx0;
    let right = x1 - bx0;
    let bottom = y0 - by0;
    let top = y1 - by0;
    match rotation {
        1 => [bottom, left, top - bottom, right - left, box_height, box_width],
        2 => [
            box_width - right,
            bottom,
            right - left,
            top - bottom,
            box_width,
            box_height,
        ],
        3 => [
            box_height - top,
            box_width - right,
            top - bottom,
            right - left,
            box_height,
            box_width,
        ],
        _ => [left, box_height - top, right - left, top - bottom, box_width, box_height],
    }
}

/// `Templates::BuildPdfiumAnnotations.call`: the `http(s)` links of every page.
fn build_annotations(doc: &mut Document<'_>) -> Vec<Value> {
    let mut annotations = Vec::new();
    for page_index in 0..doc.page_count() {
        if annotations.len() >= LINKS_LIMIT {
            break;
        }
        if doc.annot_count(page_index) == 0 {
            continue;
        }
        let Some(page) = doc.page(page_index) else {
            continue;
        };
        let geometry = doc.geometry(page);
        for annotation in doc.annotations(page_index) {
            if annotation.subtype != FPDF_ANNOT_LINK || annotation.bounds.is_empty() {
                continue;
            }
            // A closure: `Handle::link_uri` is not general over the handle's lifetime.
            #[allow(clippy::redundant_closure_for_method_calls)]
            let url = doc
                .with_annotation(page_index, annotation.index, |h| h.link_uri())
                .flatten();
            let Some(url) = url.filter(|u| !u.trim().is_empty()) else {
                continue;
            };
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                continue;
            }
            let [x, y, w, h, pw, ph] =
                transform_rect(&annotation.bounds, geometry.box_, geometry.rotation);
            annotations.push(json!({
                "type": "external_link",
                "value": url,
                "x": x / pw,
                "y": y / ph,
                "w": w / pw,
                "h": h / ph,
                "page": page_index,
            }));
        }
    }
    annotations
}

/// `Templates::FindPdfiumAcroFields::SELECT_PLACEHOLDER_REGEXP`.
fn select_placeholder() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // Ruby's `\b` is ASCII-only.
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?-u:\b)(Select|Choose|Wählen|Auswählen|Sélectionner|Choisir|Seleccionar|Elegir|Seleziona|Scegliere|Selecionar|Escolher)(?-u:\b)",
        )
        .expect("valid regex")
    })
}

fn date_format() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)[myd]{2,4}[-\\/ \t\r\n\x0B\x0C.][myd]{2,4}[-\\/ \t\r\n\x0B\x0C.][myd]{2,4}")
            .expect("valid regex")
    })
}

fn text_operator() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?-u:\b)T[jJ](?-u:\b)").expect("valid regex"))
}

/// `FIELD_NAME_REGEXP`: `\A(?=.*\p{L})[\p{L}\d\s-]+\z`.
fn plain_field_name(name: &str) -> bool {
    let first_line = name.split('\n').next().unwrap_or_default();
    !name.is_empty()
        && first_line.chars().any(char::is_alphabetic)
        && name.chars().all(|c| {
            c.is_alphabetic()
                || c.is_ascii_digit()
                || matches!(c, ' ' | '\t' | '\r' | '\n' | '\x0B' | '\x0C' | '-')
        })
}

fn blank(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim().is_empty())
}

fn presence(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldType {
    Unknown,
    Pushbutton,
    Checkbox,
    Radio,
    Combobox,
    Listbox,
    Text,
    Signature,
}

/// `Pdfium::Field`.
#[derive(Clone, Debug)]
struct Field {
    detached: bool,
    kind: FieldType,
    flags: i32,
    name: String,
    alternate_name: Option<String>,
    value: Option<String>,
    export_value: Option<String>,
    control_index: i32,
    control_count: i32,
    options: Vec<String>,
    checked: bool,
    max_len: Option<f64>,
    quadding: Option<f64>,
    format_js: Option<String>,
    keystroke_js: Option<String>,
    dict_type: Option<String>,
    partial_name: Option<String>,
}

impl Field {
    fn read(handle: &Handle<'_>) -> Self {
        let code = handle.field_type();
        let mut field = Self {
            detached: code < 0,
            kind: FieldType::Unknown,
            flags: 0,
            name: String::new(),
            alternate_name: None,
            value: None,
            export_value: None,
            control_index: 0,
            control_count: 1,
            options: Vec::new(),
            checked: false,
            max_len: None,
            quadding: None,
            format_js: None,
            keystroke_js: None,
            dict_type: handle.string_value("FT"),
            partial_name: handle.string_value("T"),
        };
        if field.detached {
            field.read_detached(handle);
        } else {
            field.read_form(handle, code);
        }
        field
    }

    fn read_form(&mut self, handle: &Handle<'_>, code: i32) {
        self.kind = match code {
            FPDF_FORMFIELD_PUSHBUTTON => FieldType::Pushbutton,
            FPDF_FORMFIELD_CHECKBOX => FieldType::Checkbox,
            FPDF_FORMFIELD_RADIOBUTTON => FieldType::Radio,
            FPDF_FORMFIELD_COMBOBOX => FieldType::Combobox,
            FPDF_FORMFIELD_LISTBOX => FieldType::Listbox,
            FPDF_FORMFIELD_TEXTFIELD => FieldType::Text,
            FPDF_FORMFIELD_SIGNATURE => FieldType::Signature,
            _ => FieldType::Unknown,
        };
        self.flags = handle.field_flags();
        self.name = handle.field_name().unwrap_or_default();
        self.alternate_name = handle.alternate_name();
        self.value = handle.field_value();
        self.control_index = handle.control_index();
        self.control_count = handle.control_count();
        match self.kind {
            FieldType::Checkbox | FieldType::Radio => {
                self.checked = handle.checked();
                self.export_value = handle.export_value();
                if self.control_index == 0 {
                    self.options = handle.option_labels();
                }
            }
            FieldType::Combobox | FieldType::Listbox => self.options = handle.option_labels(),
            FieldType::Text => {
                self.max_len = handle.number_value("MaxLen");
                self.quadding = handle.number_value("Q");
                self.format_js = handle.action_javascript(FPDF_ANNOT_AACTION_FORMAT);
                self.keystroke_js = handle.action_javascript(FPDF_ANNOT_AACTION_KEY_STROKE);
            }
            _ => {}
        }
    }

    fn read_detached(&mut self, handle: &Handle<'_>) {
        #[allow(clippy::cast_possible_truncation, reason = "`number_value('Ff').to_i`")]
        let flags = handle.number_value("Ff").map_or(0, |v| v as i32);
        self.flags = flags;
        self.value = handle.string_value("V");
        self.name = self.partial_name.clone().unwrap_or_default();
        self.alternate_name = handle.string_value("TU");
        self.kind = match self.dict_type.as_deref() {
            Some("Tx") => FieldType::Text,
            Some("Sig") => FieldType::Signature,
            Some("Btn") if flags & FPDF_FORMFLAG_BUTTON_PUSH != 0 => FieldType::Pushbutton,
            Some("Btn") => FieldType::Checkbox,
            _ => FieldType::Unknown,
        };
        if self.kind == FieldType::Text {
            self.max_len = handle.number_value("MaxLen");
            self.quadding = handle.number_value("Q");
        } else {
            self.checked = !blank(self.value.as_deref()) && self.value.as_deref() != Some("Off");
        }
    }

    fn required(&self) -> bool {
        self.flags & FPDF_FORMFLAG_REQUIRED != 0
    }

    fn comb(&self) -> bool {
        self.flags & FPDF_FORMFLAG_TEXT_COMB != 0 && self.flags & TEXT_TYPE_FLAGS == 0
    }

    fn own(&self) -> bool {
        !blank(self.dict_type.as_deref()) && !blank(self.partial_name.as_deref())
    }
}

struct Widget {
    annotation: Annotation,
    field: Field,
    /// Whether the normal appearance draws text (`renders_text?`), for selects.
    renders_text: bool,
}

/// `Templates::FindPdfiumAcroFields.call`.
fn find_acro_fields(doc: &mut Document<'_>, data: &[u8], attachment_uuid: &str) -> Vec<Value> {
    if !doc.is_form() && !contains(data, b"/Form") {
        return Vec::new();
    }
    let mut pages = HashMap::new();
    let mut widgets = Vec::new();
    for page_index in 0..doc.page_count() {
        if doc.annot_count(page_index) == 0 {
            continue;
        }
        let Some(page) = doc.page(page_index) else {
            continue;
        };
        pages.insert(page_index, doc.geometry(page));
        // The standard export value and checked state functions read the page's widgets,
        // which need the page view (Rails' build reads them raw).
        doc.load_page_view(page_index);
        for annotation in doc.annotations(page_index) {
            if annotation.subtype != FPDF_ANNOT_WIDGET || annotation.bounds.is_empty() {
                continue;
            }
            let Some((field, renders_text)) =
                doc.with_annotation(page_index, annotation.index, |handle| {
                    let field = Field::read(handle);
                    let renders_text = field.kind == FieldType::Combobox
                        && handle
                            .appearance()
                            .is_some_and(|ap| text_operator().is_match(&ap));
                    (field, renders_text)
                })
            else {
                continue;
            };
            if matches!(field.kind, FieldType::Unknown | FieldType::Pushbutton) {
                continue;
            }
            widgets.push(Widget {
                annotation,
                field,
                renders_text,
            });
        }
    }

    group_widgets(widgets)
        .into_iter()
        .filter_map(|group| build_field(group, &pages, attachment_uuid))
        .collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `group_widgets`: widgets of one field together, in order of first appearance.
fn group_widgets(widgets: Vec<Widget>) -> Vec<Vec<Widget>> {
    #[derive(PartialEq)]
    enum Key {
        Widget(usize, i32),
        Field(FieldType, String),
    }
    let mut keys: Vec<Key> = Vec::new();
    let mut groups: Vec<Vec<Widget>> = Vec::new();
    for widget in widgets {
        let field = &widget.field;
        let key = if (field.control_count > 1 && field.own())
            || field.detached
            || (field.name.trim().is_empty() && field.control_count <= 1)
        {
            Key::Widget(widget.annotation.page_index, widget.annotation.index)
        } else {
            Key::Field(field.kind, field.name.clone())
        };
        if let Some(position) = keys.iter().position(|k| *k == key) {
            groups[position].push(widget);
        } else {
            keys.push(key);
            groups.push(vec![widget]);
        }
    }
    groups
}

fn build_field(
    mut widgets: Vec<Widget>,
    pages: &HashMap<usize, Geometry>,
    attachment_uuid: &str,
) -> Option<Value> {
    // `sort_by.with_index { [control_index, index] }`: a stable sort.
    widgets.sort_by_key(|w| w.field.control_index);

    let mut areas: Vec<Map<String, Value>> = widgets
        .iter()
        .filter_map(|w| build_area(w, pages.get(&w.annotation.page_index)?, attachment_uuid))
        .collect();
    if areas.is_empty() {
        return None;
    }

    let mut properties = build_field_properties(&widgets);
    if properties.is_empty() {
        return None;
    }
    if properties.get("default_value").is_some_and(present) {
        return None;
    }

    if properties.get("type").and_then(Value::as_str) == Some("radio") {
        let options_len = properties
            .get("options")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if areas.len() != options_len {
            properties.insert(
                "options".into(),
                Value::Array(build_options(
                    &vec![OptionValue::Text(String::new()); areas.len()],
                    None,
                )),
            );
        }
        let options = properties["options"].as_array().cloned().unwrap_or_default();
        for (area, option) in areas.iter_mut().zip(&options) {
            area.insert("option_uuid".into(), option["uuid"].clone());
        }
    }

    let mut field = Map::new();
    field.insert("uuid".into(), uuid::Uuid::new_v4().to_string().into());
    field.insert("required".into(), widgets[0].field.required().into());
    field.insert("preferences".into(), json!({}));
    field.insert(
        "areas".into(),
        Value::Array(areas.into_iter().map(Value::Object).collect()),
    );
    for (key, value) in properties {
        field.insert(key, value);
    }
    Some(Value::Object(field))
}

/// `Object#present?` for JSON values.
fn present(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}

fn build_area(
    widget: &Widget,
    page: &Geometry,
    attachment_uuid: &str,
) -> Option<Map<String, Value>> {
    let [x, y, w, h, pw, ph] = transform_rect(&widget.annotation.bounds, page.box_, page.rotation);
    let (w, h) = (w / pw, h / ph);
    if w == 0.0 || h == 0.0 {
        return None;
    }
    let mut area = Map::new();
    area.insert("page".into(), widget.annotation.page_index.into());
    area.insert("x".into(), json!(x / pw));
    area.insert("y".into(), json!(y / ph));
    area.insert("w".into(), json!(w));
    area.insert("h".into(), json!(h));
    area.insert("attachment_uuid".into(), attachment_uuid.into());
    if let Some(max_len) = widget.field.max_len.filter(|m| widget.field.comb() && *m > 0.0) {
        area.insert("cell_w".into(), json!(w / max_len));
    }
    Some(area)
}

fn build_field_properties(widgets: &[Widget]) -> Map<String, Value> {
    let field = &widgets[0].field;
    let mut attrs = Map::new();
    let name = if plain_field_name(&field.name) {
        field.name.clone()
    } else {
        String::new()
    };
    attrs.insert("name".into(), name.into());
    if let Some(alternate) = field.alternate_name.as_deref()
        && !alternate.trim().is_empty()
        && alternate != field.name
        && alternate != "undefined"
    {
        attrs.insert("description".into(), alternate.into());
    }

    let properties = match field.kind {
        FieldType::Checkbox | FieldType::Radio => build_button_properties(attrs, widgets),
        FieldType::Combobox => build_select_properties(attrs, &widgets[0]),
        FieldType::Text => build_text_properties(attrs, field),
        FieldType::Signature => {
            let kind = if field.name.to_lowercase().contains("initials") {
                "initials"
            } else {
                "signature"
            };
            attrs.insert("type".into(), kind.into());
            attrs
        }
        _ => Map::new(),
    };
    // `.compact`
    properties.into_iter().filter(|(_, v)| !v.is_null()).collect()
}

fn build_button_properties(mut attrs: Map<String, Value>, widgets: &[Widget]) -> Map<String, Value> {
    let field = &widgets[0].field;
    let options = widgets
        .iter()
        .find(|w| !w.field.options.is_empty())
        .map(|w| w.field.options.clone())
        .unwrap_or_default();
    let checked = widgets.iter().find(|w| w.field.checked);
    let mut export_values: Vec<String> = Vec::new();
    for widget in widgets {
        if let Some(value) = presence(widget.field.export_value.clone())
            && !export_values.contains(&value)
        {
            export_values.push(value);
        }
    }

    if field.kind == FieldType::Radio && !options.is_empty() {
        let values: Vec<OptionValue> = options.iter().cloned().map(OptionValue::Text).collect();
        attrs.insert("type".into(), "radio".into());
        attrs.insert(
            "options".into(),
            Value::Array(build_options(&values, Some("radio"))),
        );
        let default = checked.and_then(|c| {
            usize::try_from(c.field.control_index)
                .ok()
                .and_then(|i| options.get(i).cloned())
        });
        attrs.insert("default_value".into(), default.map_or(Value::Null, Value::from));
    } else if field.control_count > 1 && export_values.len() > 1 {
        let values: Vec<OptionValue> = export_values
            .iter()
            .cloned()
            .map(OptionValue::Symbol)
            .collect();
        attrs.insert("type".into(), "radio".into());
        attrs.insert(
            "options".into(),
            Value::Array(build_options(&values, Some("radio"))),
        );
        let default = checked.and_then(|c| presence(c.field.export_value.clone()));
        attrs.insert("default_value".into(), default.map_or(Value::Null, Value::from));
    } else {
        attrs.insert("type".into(), "checkbox".into());
        attrs.insert("default_value".into(), checked.is_some().into());
    }
    attrs
}

fn build_select_properties(mut attrs: Map<String, Value>, widget: &Widget) -> Map<String, Value> {
    let field = &widget.field;
    if field.options.is_empty() {
        return Map::new();
    }
    let value = if widget.renders_text {
        presence(field.value.clone())
    } else {
        None
    };
    let values: Vec<OptionValue> = field.options.iter().cloned().map(OptionValue::Text).collect();
    attrs.insert("type".into(), "select".into());
    attrs.insert(
        "options".into(),
        Value::Array(build_options(&values, Some("select"))),
    );
    let default = value.filter(|v| !select_placeholder().is_match(v));
    attrs.insert("default_value".into(), default.map_or(Value::Null, Value::from));
    attrs
}

fn build_text_properties(mut attrs: Map<String, Value>, field: &Field) -> Map<String, Value> {
    #[allow(clippy::cast_possible_truncation, reason = "`quadding.to_i`")]
    let align = match field.quadding.map_or(0, |q| q as i64) {
        1 => "center",
        2 => "right",
        _ => "left",
    };
    let mut preferences = Map::new();
    preferences.insert("align".into(), align.into());
    let value = presence(field.value.clone()).map_or(Value::Null, Value::from);

    let kind = if field.comb() {
        "cells"
    } else if is_date(field) {
        let format = [field.format_js.as_deref(), field.keystroke_js.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|js| date_format().find(js).map(|m| m.as_str().to_uppercase()));
        if let Some(format) = format {
            preferences.insert("format".into(), format.into());
        }
        "date"
    } else {
        "text"
    };
    attrs.insert("preferences".into(), Value::Object(preferences));
    attrs.insert("type".into(), kind.into());
    attrs.insert("default_value".into(), value);
    attrs
}

fn is_date(field: &Field) -> bool {
    [field.format_js.as_deref(), field.keystroke_js.as_deref()]
        .into_iter()
        .flatten()
        .any(|js| js.contains("AFDate_"))
}

/// An option as Ruby sees it: a String, or a Symbol (export values).
#[derive(Clone, Debug)]
enum OptionValue {
    Text(String),
    Symbol(String),
}

/// `build_options`.
fn build_options(values: &[OptionValue], kind: Option<&str>) -> Vec<Value> {
    let texts: Vec<&str> = values
        .iter()
        .map(|v| match v {
            OptionValue::Text(s) | OptionValue::Symbol(s) => s.as_str(),
        })
        .collect();
    let mut unique = texts.clone();
    unique.dedup();
    unique.sort_unstable();
    unique.dedup();
    let skip_single_value = matches!(kind, Some("radio" | "multiple")) && unique.len() == 1;

    values
        .iter()
        .filter_map(|option| {
            let (text, is_number) = match option {
                OptionValue::Text(s) => (s.as_str(), false),
                OptionValue::Symbol(s) => {
                    (s.as_str(), !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
                }
            };
            if kind == Some("select") && select_placeholder().is_match(text) {
                return None;
            }
            let value = if is_number || skip_single_value {
                Value::String(String::new())
            } else if text.trim().is_empty() {
                Value::Null
            } else {
                Value::String(text.to_owned())
            };
            Some(json!({ "uuid": uuid::Uuid::new_v4().to_string(), "value": value }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_rects_like_rails() {
        let bounds = [100.0, 300.0, 300.0, 330.0];
        let box_ = [0.0, 0.0, 612.0, 792.0];
        assert_eq!(
            transform_rect(&bounds, box_, 0),
            [100.0, 462.0, 200.0, 30.0, 612.0, 792.0]
        );
        assert_eq!(
            transform_rect(&bounds, box_, 1),
            [300.0, 100.0, 30.0, 200.0, 792.0, 612.0]
        );
    }

    #[test]
    fn matches_field_names_like_ruby() {
        assert!(plain_field_name("Full Name"));
        assert!(plain_field_name("Notes 2"));
        assert!(plain_field_name("Straße-1"));
        assert!(!plain_field_name("123"));
        assert!(!plain_field_name("form[0].name"));
        assert!(!plain_field_name(""));
    }

    #[test]
    fn matches_select_placeholders() {
        assert!(select_placeholder().is_match("Select a country"));
        assert!(select_placeholder().is_match("-- bitte WÄHLEN --"));
        assert!(!select_placeholder().is_match("Selected"));
        assert!(!select_placeholder().is_match("Germany"));
    }

    #[test]
    fn builds_options_like_rails() {
        let options = build_options(
            &[
                OptionValue::Text("Select one".into()),
                OptionValue::Text("A".into()),
                OptionValue::Text(" ".into()),
            ],
            Some("select"),
        );
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["value"], "A");
        assert_eq!(options[1]["value"], Value::Null);

        let single = build_options(
            &[
                OptionValue::Text("On".into()),
                OptionValue::Text("On".into()),
            ],
            Some("radio"),
        );
        assert_eq!(single[0]["value"], "");

        let numbers = build_options(
            &[
                OptionValue::Symbol("1".into()),
                OptionValue::Symbol("Yes".into()),
            ],
            Some("radio"),
        );
        assert_eq!(numbers[0]["value"], "");
        assert_eq!(numbers[1]["value"], "Yes");
    }

    #[test]
    fn finds_date_formats() {
        let m = date_format()
            .find(r#"AFDate_FormatEx("mm/dd/yyyy");"#)
            .unwrap();
        assert_eq!(m.as_str().to_uppercase(), "MM/DD/YYYY");
    }
}
