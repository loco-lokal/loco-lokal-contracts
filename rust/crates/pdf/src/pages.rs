//! Single pages of a stored PDF: the lazy preview image
//! (`Templates::ProcessDocument.generate_pdf_preview_from_io`) and the text and image
//! boxes the template builder snaps fields to (`Templates::ModifyDocuments.page_objects`).
//!
//! This calls `PDFium` directly, as `lib/pdfium.rb` does, so the same functions run in
//! the same order: the page is flattened, its form `XObject`s unwrapped and its rotation
//! applied before the objects are read.
#![allow(
    unsafe_code,
    reason = "PDFium FFI calls on handles this module opens and closes"
)]
#![allow(
    clippy::many_single_char_names,
    reason = "PDF box edges, as in lib/pdfium.rb"
)]

use std::cmp::Ordering;

use pdfium_render::prelude::{
    FPDF_DOCUMENT, FPDF_FORMFILLINFO, FPDF_FORMHANDLE, FPDF_PAGE, FPDF_PAGEOBJECT, FS_MATRIX,
    FS_RECTF, Pdfium, PdfiumLibraryBindings, PdfiumLibraryBindingsAccessor,
};

const FPDF_ANNOT: i32 = 0x01;
const FPDF_LCD_TEXT: i32 = 0x02;
const FPDF_NO_NATIVETEXT: i32 = 0x04;
const FPDF_REVERSE_BYTE_ORDER: i32 = 0x10;
const FPDF_PAGEOBJ_PATH: i32 = 2;
const FPDF_PAGEOBJ_IMAGE: i32 = 3;
const FPDF_PAGEOBJ_FORM: i32 = 5;
const FLAT_NORMALDISPLAY: i32 = 0;
const FLATTEN_FAIL: i32 = 0;
const FLATTEN_SUCCESS: i32 = 1;
/// `Pdfium::MAX_SIZE`.
const MAX_SIZE: i64 = 32_767;
/// `Templates::ProcessDocument::MAX_WIDTH`.
pub const MAX_WIDTH: i32 = 1400;

/// A rendered page: RGBA rows, `width * 4` bytes each.
#[derive(Clone, Debug)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// `Pdfium::TextNode`: one character, its loose box in page fractions.
#[derive(Clone, Debug, PartialEq)]
pub struct TextNode {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A coordinate of an image node. Ruby's `Float#clamp(0, 1)` returns the Integer bound
/// when it clamps, and Rails writes those as `0` and `1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Coord {
    Int(i64),
    Float(f64),
}

impl Coord {
    fn clamp01(value: f64) -> Self {
        if value < 0.0 {
            Self::Int(0)
        } else if value > 1.0 {
            Self::Int(1)
        } else {
            Self::Float(value)
        }
    }

    #[allow(clippy::cast_precision_loss, reason = "0 or 1")]
    fn as_f64(self) -> f64 {
        match self {
            Self::Int(i) => i as f64,
            Self::Float(f) => f,
        }
    }

    fn sub(self, other: Self) -> Self {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => Self::Int(a - b),
            _ => Self::Float(self.as_f64() - other.as_f64()),
        }
    }
}

/// `Pdfium::ImageNode`, in page fractions.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageNode {
    pub x: Coord,
    pub y: Coord,
    pub w: Coord,
    pub h: Coord,
}

/// `Templates::ModifyDocuments.page_objects`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageObjects {
    pub text_nodes: Vec<TextNode>,
    pub image_nodes: Vec<ImageNode>,
}

/// `Pdfium::LineNode`: a path object thin enough to be a line, in page fractions.
/// `tilt` is 0 for horizontal lines and 90 for vertical ones.
#[derive(Clone, Debug, PartialEq)]
pub struct LineNode {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub tilt: i32,
}

/// A page as `Templates::DetectFields` reads it: rendered `size` pixels along its longer
/// side, its size in points, and its characters and lines.
#[derive(Clone, Debug)]
pub struct DetectPage {
    pub bitmap: Bitmap,
    pub width: f64,
    pub height: f64,
    pub text_nodes: Vec<TextNode>,
    /// Sorted by bottom edge, then `x`, as `line_nodes` sorts them.
    pub line_nodes: Vec<LineNode>,
}

/// `prepare_page_image` (`render_to_bitmap(width: size)` for landscape pages, else
/// `height: size`), then `text_nodes` and `line_nodes` of the same page, which is not
/// flattened.
pub fn detect_page(
    pdfium: &Pdfium,
    bytes: &[u8],
    page_index: i64,
    size: f64,
) -> Result<DetectPage, String> {
    let doc = Document::open(pdfium.bindings(), bytes)?;
    let mut page = doc.page(page_index)?;
    let (width, height) = (page.width(), page.height());
    #[allow(clippy::cast_possible_truncation, reason = "clamped to MAX_SIZE")]
    let (w, h) = if width > height {
        (size as i64, (height * (size / width)).round() as i64)
    } else {
        ((width * (size / height)).round() as i64, size as i64)
    };
    let bitmap = page.render_size(w, h)?;
    Ok(DetectPage {
        bitmap,
        width,
        height,
        text_nodes: page.text_nodes()?,
        line_nodes: page.line_nodes(),
    })
}

/// `generate_pdf_preview_from_io`: the page rendered `MAX_WIDTH` wide (forms drawn, not
/// flattened). Errors carry `Pdfium::PdfiumError`'s message.
pub fn render_preview(pdfium: &Pdfium, bytes: &[u8], page_index: i64) -> Result<Bitmap, String> {
    let doc = Document::open(pdfium.bindings(), bytes)?;
    let mut page = doc.page(page_index)?;
    page.render(MAX_WIDTH)
}

/// `page_objects(attachment, page_number)`: flatten, unwrap form `XObject`s, apply the
/// rotation, then read the characters and images.
pub fn page_objects(pdfium: &Pdfium, bytes: &[u8], page_index: i64) -> Result<PageObjects, String> {
    let doc = Document::open(pdfium.bindings(), bytes)?;
    let mut page = doc.page(page_index)?;
    page.flatten()?;
    page.unwrap_form_objects()?;
    page.rotate();
    Ok(PageObjects {
        text_nodes: page.text_nodes()?,
        image_nodes: page.image_nodes(),
    })
}

/// `Pdfium::Document` with its form fill environment.
struct Document<'a> {
    bindings: &'a dyn PdfiumLibraryBindings,
    doc: FPDF_DOCUMENT,
    form: FPDF_FORMHANDLE,
    // Must outlive `form`.
    _form_info: Box<FPDF_FORMFILLINFO>,
}

impl<'a> Document<'a> {
    fn open(bindings: &'a dyn PdfiumLibraryBindings, bytes: &'a [u8]) -> Result<Self, String> {
        // SAFETY: `bytes` outlives the document, which is closed on drop; the form fill
        // info is boxed and freed after the form handle is closed.
        unsafe {
            let doc = bindings.FPDF_LoadMemDocument64(bytes, None);
            if doc.is_null() {
                let code = bindings.FPDF_GetLastError();
                return Err(format!(
                    "Failed to load document from memory (Code: {code})"
                ));
            }
            let mut form_info: Box<FPDF_FORMFILLINFO> = Box::new(std::mem::zeroed());
            form_info.version = 2;
            let form = bindings.FPDFDOC_InitFormFillEnvironment(doc, &raw mut *form_info);
            Ok(Self {
                bindings,
                doc,
                form,
                _form_info: form_info,
            })
        }
    }

    /// `doc.get_page(index)`.
    fn page(&self, page_index: i64) -> Result<Page<'_>, String> {
        // SAFETY: an open document.
        let count = i64::from(unsafe { self.bindings.FPDF_GetPageCount(self.doc) });
        let index = i32::try_from(page_index)
            .ok()
            .filter(|_| (0..count).contains(&page_index))
            .ok_or_else(|| format!("Page index {page_index} out of range (0..{})", count - 1))?;
        let mut page = Page {
            doc: self,
            index,
            ptr: std::ptr::null_mut(),
            view_loaded: false,
        };
        page.load()?;
        Ok(page)
    }
}

impl Drop for Document<'_> {
    fn drop(&mut self) {
        // SAFETY: handles this document opened; pages are closed before (they borrow it).
        unsafe {
            if !self.form.is_null() {
                self.bindings.FPDFDOC_ExitFormFillEnvironment(self.form);
            }
            self.bindings.FPDF_CloseDocument(self.doc);
        }
    }
}

type Getter =
    unsafe fn(&dyn PdfiumLibraryBindings, FPDF_PAGE, *mut f32, *mut f32, *mut f32, *mut f32) -> i32;
type Setter = unsafe fn(&dyn PdfiumLibraryBindings, FPDF_PAGE, f32, f32, f32, f32);

/// `Pdfium::Page`.
struct Page<'d> {
    doc: &'d Document<'d>,
    index: i32,
    ptr: FPDF_PAGE,
    view_loaded: bool,
}

impl Page<'_> {
    fn b(&self) -> &dyn PdfiumLibraryBindings {
        self.doc.bindings
    }

    fn load(&mut self) -> Result<(), String> {
        // SAFETY: an open document and an index in range.
        self.ptr = unsafe { self.b().FPDF_LoadPage(self.doc.doc, self.index) };
        if self.ptr.is_null() {
            return Err(format!("Failed to load page {}", self.index));
        }
        Ok(())
    }

    /// `reload`: closes the page view and the page, and loads the page again.
    fn reload(&mut self) -> Result<(), String> {
        // SAFETY: a loaded page; `close_page_view` calls `FORM_OnBeforeClosePage` even
        // when the view was never loaded, as Rails does.
        unsafe {
            if !self.doc.form.is_null() {
                self.b().FORM_OnBeforeClosePage(self.ptr, self.doc.form);
            }
            self.b().FPDF_ClosePage(self.ptr);
        }
        self.view_loaded = false;
        self.load()
    }

    fn width(&self) -> f64 {
        // SAFETY: a loaded page.
        f64::from(unsafe { self.b().FPDF_GetPageWidthF(self.ptr) })
    }

    fn height(&self) -> f64 {
        // SAFETY: a loaded page.
        f64::from(unsafe { self.b().FPDF_GetPageHeightF(self.ptr) })
    }

    /// `render_to_bitmap(width:)` with Rails' default flags and a white background.
    fn render(&mut self, width: i32) -> Result<Bitmap, String> {
        let scale = f64::from(width) / self.width();
        #[allow(clippy::cast_possible_truncation, reason = "clamped to MAX_SIZE")]
        let height = (self.height() * scale).round() as i64;
        self.render_size(i64::from(width), height)
    }

    /// `render_to_bitmap` at a computed size (each side clamped to `1..MAX_SIZE`).
    fn render_size(&mut self, width: i64, height: i64) -> Result<Bitmap, String> {
        let w = i32::try_from(width.clamp(1, MAX_SIZE)).unwrap_or(1);
        let h = i32::try_from(height.clamp(1, MAX_SIZE)).unwrap_or(1);
        let flags = FPDF_ANNOT | FPDF_LCD_TEXT | FPDF_NO_NATIVETEXT | FPDF_REVERSE_BYTE_ORDER;
        // SAFETY: a loaded page; the bitmap is destroyed below.
        unsafe {
            let bitmap = self.b().FPDFBitmap_Create(w, h, 1);
            if bitmap.is_null() {
                return Err("Failed to create bitmap (FPDFBitmap_Create returned NULL)".into());
            }
            #[allow(clippy::let_unit_value, reason = "returns a BOOL in newer bindings")]
            let _ = self
                .b()
                .FPDFBitmap_FillRect(bitmap, 0, 0, w, h, 0xFFFF_FFFF);
            self.b()
                .FPDF_RenderPageBitmap(bitmap, self.ptr, 0, 0, w, h, 0, flags);
            if !self.doc.form.is_null() {
                self.b()
                    .FPDF_FFLDraw(self.doc.form, bitmap, self.ptr, 0, 0, w, h, 0, flags);
            }
            let buffer = self.b().FPDFBitmap_GetBuffer(bitmap).cast::<u8>();
            let stride = usize::try_from(self.b().FPDFBitmap_GetStride(bitmap)).unwrap_or(0);
            let (wu, hu) = (
                usize::try_from(w).unwrap_or(0),
                usize::try_from(h).unwrap_or(0),
            );
            let mut rgba = Vec::with_capacity(wu * hu * 4);
            for row in 0..hu {
                let line = std::slice::from_raw_parts(buffer.add(row * stride), wu * 4);
                rgba.extend_from_slice(line);
            }
            self.b().FPDFBitmap_Destroy(bitmap);
            Ok(Bitmap {
                width: w.unsigned_abs(),
                height: h.unsigned_abs(),
                rgba,
            })
        }
    }

    /// `box`: the bounding box, else US Letter.
    fn bounding_box(&self) -> [f32; 4] {
        let mut rect = FS_RECTF {
            left: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
        };
        // SAFETY: a loaded page and a rect to fill.
        if unsafe { self.b().FPDF_GetPageBoundingBox(self.ptr, &raw mut rect) } != 0 {
            let (x0, x1) = (rect.left.min(rect.right), rect.left.max(rect.right));
            let (y0, y1) = (rect.bottom.min(rect.top), rect.bottom.max(rect.top));
            if x1 - x0 > 0.0 && y1 - y0 > 0.0 {
                return [x0, y0, x1, y1];
            }
        }
        [0.0, 0.0, 612.0, 792.0]
    }

    /// `flatten`: loads the page view, sets missing media and crop boxes, flattens the
    /// annotations, and reloads the page when anything was flattened.
    fn flatten(&mut self) -> Result<(), String> {
        if !self.view_loaded && !self.doc.form.is_null() {
            // SAFETY: a loaded page and the document's form handle.
            unsafe { self.b().FORM_OnAfterLoadPage(self.ptr, self.doc.form) };
            self.view_loaded = true;
        }
        let [left, bottom, right, top] = self.bounding_box();
        let (mut l, mut b, mut r, mut t) = (0f32, 0f32, 0f32, 0f32);
        // SAFETY: a loaded page and floats to fill.
        let result = unsafe {
            if self
                .b()
                .FPDFPage_GetMediaBox(self.ptr, &raw mut l, &raw mut b, &raw mut r, &raw mut t)
                == 0
            {
                self.b()
                    .FPDFPage_SetMediaBox(self.ptr, left, bottom, right, top);
            }
            if self
                .b()
                .FPDFPage_GetCropBox(self.ptr, &raw mut l, &raw mut b, &raw mut r, &raw mut t)
                == 0
            {
                self.b()
                    .FPDFPage_SetCropBox(self.ptr, left, bottom, right, top);
            }
            self.b().FPDFPage_Flatten(self.ptr, FLAT_NORMALDISPLAY)
        };
        if result == FLATTEN_FAIL {
            return Err(format!("Failed to flatten page {}", self.index));
        }
        if result == FLATTEN_SUCCESS {
            self.reload()?;
        }
        Ok(())
    }

    fn objects(&self) -> Vec<FPDF_PAGEOBJECT> {
        // SAFETY: a loaded page.
        unsafe {
            let count = self.b().FPDFPage_CountObjects(self.ptr);
            (0..count)
                .map(|i| self.b().FPDFPage_GetObject(self.ptr, i))
                .filter(|o| !o.is_null())
                .collect()
        }
    }

    fn object_type(&self, object: FPDF_PAGEOBJECT) -> i32 {
        // SAFETY: an object of the loaded page.
        unsafe { self.b().FPDFPageObj_GetType(object) }
    }

    /// `unwrap_form_objects`: moves the objects of each form `XObject` onto the page, with
    /// the form's matrix applied, until none are left.
    fn unwrap_form_objects(&mut self) -> Result<(), String> {
        let mut unwrapped = false;
        while let Some(form) = self
            .objects()
            .into_iter()
            .find(|&o| self.object_type(o) == FPDF_PAGEOBJ_FORM)
        {
            unwrapped = true;
            let mut matrix = FS_MATRIX {
                a: 1.0,
                b: 0.0,
                c: 0.0,
                d: 1.0,
                e: 0.0,
                f: 0.0,
            };
            // SAFETY: a form object of the loaded page; children are moved to the page,
            // then the emptied form is removed and destroyed.
            unsafe {
                if self.b().FPDFPageObj_GetMatrix(form, &raw mut matrix) == 0 {
                    matrix = FS_MATRIX {
                        a: 1.0,
                        b: 0.0,
                        c: 0.0,
                        d: 1.0,
                        e: 0.0,
                        f: 0.0,
                    };
                }
                let count = self.b().FPDFFormObj_CountObjects(form);
                for index in (0..count).rev() {
                    let child = self.b().FPDFFormObj_GetObject(
                        form,
                        std::ffi::c_ulong::try_from(index).unwrap_or(0),
                    );
                    if child.is_null() {
                        continue;
                    }
                    if self.b().FPDFFormObj_RemoveObject(form, child) == 0 {
                        return Err("Failed to unwrap form object".into());
                    }
                    self.b().FPDFPageObj_Transform(
                        child,
                        f64::from(matrix.a),
                        f64::from(matrix.b),
                        f64::from(matrix.c),
                        f64::from(matrix.d),
                        f64::from(matrix.e),
                        f64::from(matrix.f),
                    );
                    self.b().FPDFPage_InsertObject(self.ptr, child);
                }
                if self.b().FPDFPage_RemoveObject(self.ptr, form) == 0 {
                    return Err("Failed to remove page object".into());
                }
                self.b().FPDFPageObj_Destroy(form);
            }
        }
        if unwrapped {
            // SAFETY: a loaded page.
            unsafe { self.b().FPDFPage_GenerateContent(self.ptr) };
        }
        Ok(())
    }

    /// `rotate`: bakes the page rotation into the content, boxes and annotations.
    fn rotate(&mut self) {
        // SAFETY: a loaded page; every pointer passed is to a local.
        unsafe {
            let rotation = self.b().FPDFPage_GetRotation(self.ptr);
            if rotation == 0 {
                return;
            }
            let (mut l, mut b, mut r, mut t) = (0f32, 0f32, 0f32, 0f32);
            let has_crop = self
                .b()
                .FPDFPage_GetCropBox(self.ptr, &raw mut l, &raw mut b, &raw mut r, &raw mut t)
                != 0;
            if !has_crop {
                self.b()
                    .FPDFPage_GetMediaBox(self.ptr, &raw mut l, &raw mut b, &raw mut r, &raw mut t);
            }
            let (pl, pb, pr, pt) = (f64::from(l), f64::from(b), f64::from(r), f64::from(t));
            let (ma, mb, mc, md, me, mf) = match rotation {
                1 => (0.0, -1.0, 1.0, 0.0, -pb, pr),
                2 => (-1.0, 0.0, 0.0, -1.0, pr, pt),
                _ => (0.0, 1.0, -1.0, 0.0, pt, -pl),
            };

            let boxes: [(Getter, Setter); 5] = [
                (
                    |x, p, l, b, r, t| x.FPDFPage_GetMediaBox(p, l, b, r, t),
                    |x, p, l, b, r, t| {
                        x.FPDFPage_SetMediaBox(p, l, b, r, t);
                    },
                ),
                (
                    |x, p, l, b, r, t| x.FPDFPage_GetCropBox(p, l, b, r, t),
                    |x, p, l, b, r, t| {
                        x.FPDFPage_SetCropBox(p, l, b, r, t);
                    },
                ),
                (
                    |x, p, l, b, r, t| x.FPDFPage_GetBleedBox(p, l, b, r, t),
                    |x, p, l, b, r, t| {
                        x.FPDFPage_SetBleedBox(p, l, b, r, t);
                    },
                ),
                (
                    |x, p, l, b, r, t| x.FPDFPage_GetTrimBox(p, l, b, r, t),
                    |x, p, l, b, r, t| {
                        x.FPDFPage_SetTrimBox(p, l, b, r, t);
                    },
                ),
                (
                    |x, p, l, b, r, t| x.FPDFPage_GetArtBox(p, l, b, r, t),
                    |x, p, l, b, r, t| {
                        x.FPDFPage_SetArtBox(p, l, b, r, t);
                    },
                ),
            ];
            for (get, set) in boxes {
                if get(
                    self.b(),
                    self.ptr,
                    &raw mut l,
                    &raw mut b,
                    &raw mut r,
                    &raw mut t,
                ) == 0
                {
                    continue;
                }
                let (bl, bb, br, bt) = (f64::from(l), f64::from(b), f64::from(r), f64::from(t));
                let (c1x, c1y, c2x, c2y) = match rotation {
                    1 => (br, bb, bl, bt),
                    2 => (br, bt, bl, bb),
                    _ => (bl, bt, br, bb),
                };
                #[allow(clippy::cast_possible_truncation, reason = "FFI takes floats")]
                set(
                    self.b(),
                    self.ptr,
                    (ma * c1x + mc * c1y + me) as f32,
                    (mb * c1x + md * c1y + mf) as f32,
                    (ma * c2x + mc * c2y + me) as f32,
                    (mb * c2x + md * c2y + mf) as f32,
                );
            }
            self.b()
                .FPDFPage_TransformAnnots(self.ptr, ma, mb, mc, md, me, mf);
            #[allow(clippy::cast_possible_truncation, reason = "FS_MATRIX holds floats")]
            let matrix = FS_MATRIX {
                a: ma as f32,
                b: mb as f32,
                c: mc as f32,
                d: md as f32,
                e: me as f32,
                f: mf as f32,
            };
            self.b()
                .FPDFPage_TransFormWithClip(self.ptr, &raw const matrix, std::ptr::null());
            self.b().FPDFPage_SetRotation(self.ptr, 0);
        }
        // Rails reloads too; a failure would surface when the page is read.
        let _ = self.reload();
    }

    /// `text_nodes`: each character's loose box, sorted into lines (characters whose
    /// bottoms are less than 4/width apart share a line) and by `x`, with Ruby's sort.
    fn text_nodes(&self) -> Result<Vec<TextNode>, String> {
        let (width, height) = (self.width(), self.height());
        let mut nodes = Vec::new();
        // SAFETY: a loaded page; the text page is closed below.
        unsafe {
            let text_page = self.b().FPDFText_LoadPage(self.ptr);
            let count = self.b().FPDFText_CountChars(text_page);
            let mut rect = FS_RECTF {
                left: 0.0,
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
            };
            let mut i = 0;
            let mut result = Ok(());
            while i < count {
                let box_index = i;
                let mut codepoint = self.b().FPDFText_GetUnicode(text_page, i);
                if (0xD800..=0xDBFF).contains(&codepoint) && i + 1 < count {
                    let low = self.b().FPDFText_GetUnicode(text_page, i + 1);
                    if (0xDC00..=0xDFFF).contains(&low) {
                        codepoint = 0x10000 + ((codepoint - 0xD800) << 10) + (low - 0xDC00);
                        i += 1;
                    }
                }
                i += 1;
                let Some(c) = char::from_u32(codepoint) else {
                    result = Err(format!("invalid codepoint 0x{codepoint:X} in UTF-8"));
                    break;
                };
                if self
                    .b()
                    .FPDFText_GetLooseCharBox(text_page, box_index, &raw mut rect)
                    == 0
                {
                    continue;
                }
                let (left, top, right, bottom) = (
                    f64::from(rect.left),
                    f64::from(rect.top),
                    f64::from(rect.right),
                    f64::from(rect.bottom),
                );
                if right <= left || top <= bottom {
                    continue;
                }
                nodes.push(TextNode {
                    text: c.to_string(),
                    x: left / width,
                    y: (height - top) / height,
                    w: (right - left) / width,
                    h: (top - bottom) / height,
                });
            }
            if !text_page.is_null() {
                self.b().FPDFText_ClosePage(text_page);
            }
            result?;
        }
        let threshold = 4.0 / width;
        ruby_sort(&mut nodes, |a, b| {
            let (a_end, b_end) = (a.y + a.h, b.y + b.h);
            if (a_end - b_end).abs() < threshold {
                spaceship(a.x, b.x)
            } else {
                spaceship(a_end, b_end)
            }
        });
        Ok(nodes)
    }

    /// `line_nodes`: path objects of 2 to 10 segments under 10 points thick (and over a
    /// point long), sorted by bottom edge, then `x`, with Ruby's sort.
    fn line_nodes(&self) -> Vec<LineNode> {
        let (width, height) = (self.width(), self.height());
        let mut nodes = Vec::new();
        for object in self.objects() {
            if self.object_type(object) != FPDF_PAGEOBJ_PATH {
                continue;
            }
            let (mut l, mut b, mut r, mut t) = (0f32, 0f32, 0f32, 0f32);
            // SAFETY: an object of the loaded page and floats to fill. Rails ignores the
            // result, reading the zeroed floats on failure.
            let segments = unsafe {
                self.b()
                    .FPDFPageObj_GetBounds(object, &raw mut l, &raw mut b, &raw mut r, &raw mut t);
                self.b().FPDFPath_CountSegments(object)
            };
            let (left, bottom) = (f64::from(l), f64::from(b));
            let obj_width = f64::from(r) - left;
            let obj_height = f64::from(t) - bottom;
            if obj_width < 1.0 && obj_height < 1.0 {
                continue;
            }
            if segments < 2 || !(segments <= 10 && (obj_height < 10.0 || obj_width < 10.0)) {
                continue;
            }
            let tilt = if obj_width > obj_height && obj_height < 10.0 {
                0
            } else if obj_height > obj_width && obj_width < 10.0 {
                90
            } else {
                continue;
            };
            nodes.push(LineNode {
                x: left / width,
                y: (height - bottom - obj_height) / height,
                w: obj_width / width,
                h: obj_height / height,
                tilt,
            });
        }
        ruby_sort(&mut nodes, |a, b| {
            let (a_end, b_end) = (a.y + a.h, b.y + b.h);
            #[allow(clippy::float_cmp, reason = "Ruby's `a.endy == b.endy`")]
            if a_end == b_end {
                spaceship(a.x, b.x)
            } else {
                spaceship(a_end, b_end)
            }
        });
        nodes
    }

    /// `image_nodes`: the bounds of the page's image objects, clamped to the page.
    fn image_nodes(&self) -> Vec<ImageNode> {
        let (width, height) = (self.width(), self.height());
        let mut nodes = Vec::new();
        for object in self.objects() {
            if self.object_type(object) != FPDF_PAGEOBJ_IMAGE {
                continue;
            }
            let (mut l, mut b, mut r, mut t) = (0f32, 0f32, 0f32, 0f32);
            // SAFETY: an object of the loaded page and floats to fill.
            if unsafe {
                self.b()
                    .FPDFPageObj_GetBounds(object, &raw mut l, &raw mut b, &raw mut r, &raw mut t)
            } == 0
            {
                continue;
            }
            let left = Coord::clamp01(f64::from(l) / width);
            let top = Coord::clamp01((height - f64::from(t)) / height);
            let right = Coord::clamp01(f64::from(r) / width);
            let bottom = Coord::clamp01((height - f64::from(b)) / height);
            let (w, h) = (right.sub(left), bottom.sub(top));
            if w.as_f64() <= 0.0 || h.as_f64() <= 0.0 {
                continue;
            }
            nodes.push(ImageNode {
                x: left,
                y: top,
                w,
                h,
            });
        }
        nodes
    }
}

impl Drop for Page<'_> {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        // SAFETY: the page this struct loaded.
        unsafe {
            if self.view_loaded && !self.doc.form.is_null() {
                self.b().FORM_OnBeforeClosePage(self.ptr, self.doc.form);
            }
            self.b().FPDF_ClosePage(self.ptr);
        }
    }
}

/// Ruby's `<=>` on floats, as the sort block sees it (NaN compares equal here; Rails
/// would raise).
fn spaceship(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// `Array#sort` with a block in the Rails image: Ruby calls musl's `qsort_r`, a
/// smoothsort, so elements the block calls equal (and blocks that aren't a total order,
/// like the line threshold above) end up where musl puts them.
pub fn ruby_sort<T>(items: &mut [T], mut cmp: impl FnMut(&T, &T) -> Ordering) {
    let n = items.len();
    if n < 2 {
        return;
    }
    let mut lp = vec![1usize, 1];
    while lp[lp.len() - 1] < n {
        let len = lp.len();
        lp.push(lp[len - 2] + lp[len - 1] + 1);
    }
    let mut c = |a: usize, b: usize, items: &[T]| match cmp(&items[a], &items[b]) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };

    let mut head = 0usize;
    let high = n - 1;
    let mut p = [1u64, 0u64];
    let mut pshift = 1usize;
    while head < high {
        if p[0] & 3 == 3 {
            sift(items, head, &mut c, pshift, &lp);
            shr(&mut p, 2);
            pshift += 2;
        } else {
            if lp[pshift - 1] >= high - head {
                trinkle(items, head, &mut c, p, pshift, false, &lp);
            } else {
                sift(items, head, &mut c, pshift, &lp);
            }
            if pshift == 1 {
                shl(&mut p, 1);
                pshift = 0;
            } else {
                shl(&mut p, pshift - 1);
                pshift = 1;
            }
        }
        p[0] |= 1;
        head += 1;
    }
    trinkle(items, head, &mut c, p, pshift, false, &lp);
    while pshift != 1 || p[0] != 1 || p[1] != 0 {
        if pshift <= 1 {
            let trail = pntz(p);
            shr(&mut p, trail);
            pshift += trail;
        } else {
            shl(&mut p, 2);
            pshift -= 2;
            p[0] ^= 7;
            shr(&mut p, 1);
            trinkle(
                items,
                head - lp[pshift] - 1,
                &mut c,
                p,
                pshift + 1,
                true,
                &lp,
            );
            shl(&mut p, 1);
            p[0] |= 1;
            trinkle(items, head - 1, &mut c, p, pshift, true, &lp);
        }
        head -= 1;
    }
}

type Cmp<'c, T> = dyn FnMut(usize, usize, &[T]) -> i32 + 'c;

/// musl's `pntz` (its `ntz(0)` is 0 on x86-64).
fn pntz(p: [u64; 2]) -> usize {
    let low = p[0].wrapping_sub(1);
    let r = if low == 0 {
        0
    } else {
        low.trailing_zeros() as usize
    };
    if r != 0 {
        return r;
    }
    let r = 64 + p[1].trailing_zeros() as usize;
    if r == 64 { 0 } else { r }
}

fn shl(p: &mut [u64; 2], mut n: usize) {
    if n >= 64 {
        n -= 64;
        p[1] = p[0];
        p[0] = 0;
    }
    if n == 0 {
        return;
    }
    p[1] = (p[1] << n) | (p[0] >> (64 - n));
    p[0] <<= n;
}

fn shr(p: &mut [u64; 2], mut n: usize) {
    if n >= 64 {
        n -= 64;
        p[0] = p[1];
        p[1] = 0;
    }
    if n == 0 {
        return;
    }
    p[0] = (p[0] >> n) | (p[1] << (64 - n));
    p[1] >>= n;
}

/// musl's `cycle`: the element at `ar[0]` moves to the last position, the others move
/// up one.
fn cycle<T>(items: &mut [T], ar: &[usize]) {
    for i in 0..ar.len().saturating_sub(1) {
        items.swap(ar[i], ar[i + 1]);
    }
}

fn sift<T>(
    items: &mut [T],
    mut head: usize,
    cmp: &mut Cmp<'_, T>,
    mut pshift: usize,
    lp: &[usize],
) {
    let mut ar = vec![head];
    while pshift > 1 {
        let rt = head - 1;
        let lf = head - 1 - lp[pshift - 2];
        if cmp(ar[0], lf, items) >= 0 && cmp(ar[0], rt, items) >= 0 {
            break;
        }
        if cmp(lf, rt, items) >= 0 {
            ar.push(lf);
            head = lf;
            pshift -= 1;
        } else {
            ar.push(rt);
            head = rt;
            pshift -= 2;
        }
    }
    cycle(items, &ar);
}

fn trinkle<T>(
    items: &mut [T],
    mut head: usize,
    cmp: &mut Cmp<'_, T>,
    pp: [u64; 2],
    mut pshift: usize,
    mut trusty: bool,
    lp: &[usize],
) {
    let mut p = pp;
    let mut ar = vec![head];
    while p[0] != 1 || p[1] != 0 {
        let stepson = head - lp[pshift];
        if cmp(stepson, ar[0], items) <= 0 {
            break;
        }
        if !trusty && pshift > 1 {
            let rt = head - 1;
            let lf = head - 1 - lp[pshift - 2];
            if cmp(rt, stepson, items) >= 0 || cmp(lf, stepson, items) >= 0 {
                break;
            }
        }
        ar.push(stepson);
        head = stepson;
        let trail = pntz(p);
        shr(&mut p, trail);
        pshift += trail;
        trusty = false;
    }
    if !trusty {
        cycle(items, &ar);
        sift(items, head, cmp, pshift, lp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_like_ruby_in_the_rails_image() {
        // `(0...12).map { |i| [i % 3, i] }.sort { |a, b| a[0] <=> b[0] }.map(&:last)`.
        let mut items: Vec<(i32, i32)> = (0..12).map(|i| (i % 3, i)).collect();
        ruby_sort(&mut items, |a, b| a.0.cmp(&b.0));
        let order: Vec<i32> = items.iter().map(|i| i.1).collect();
        assert_eq!(order, [0, 9, 6, 3, 1, 4, 7, 10, 5, 2, 8, 11]);

        let mut equal: Vec<(i32, i32)> = (0..20).map(|i| (0, i)).collect();
        ruby_sort(&mut equal, |a, b| a.0.cmp(&b.0));
        assert!(
            equal
                .iter()
                .enumerate()
                .all(|(i, e)| e.1 == i32::try_from(i).unwrap())
        );

        let mut numbers: Vec<i32> = (0..500).map(|i| (i * 7919) % 503).collect();
        ruby_sort(&mut numbers, Ord::cmp);
        assert!(numbers.windows(2).all(|w| w[0] <= w[1]));
    }
}
