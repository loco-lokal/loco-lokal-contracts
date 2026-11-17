//! `Pdfium::Document#signatures` (`Pdfium::Signature`) and `PdfUtils.merge`.
//!
//! The signature fields are read with `PDFium` directly: pdfium-render exposes neither the
//! byte range nor the raw `/Time` string.
#![allow(
    unsafe_code,
    reason = "PDFium FFI calls on handles this module opens and closes"
)]

use pdfium_render::prelude::{
    FPDF_SIGNATURE, Pdfium, PdfiumLibraryBindings, PdfiumLibraryBindingsAccessor,
};

use crate::PdfError;

/// A signature field of a document.
#[derive(Clone, Debug, Default)]
pub struct Signature {
    /// `/ByteRange`.
    pub byte_range: Vec<i64>,
    /// The decoded `/Contents` string, with any zero padding.
    pub contents: Vec<u8>,
    /// `/SubFilter`, e.g. `adbe.pkcs7.detached`.
    pub sub_filter: String,
    /// `/M`, e.g. `D:20261115100000+00'00'`.
    pub time: String,
    /// `/Reason`.
    pub reason: Option<String>,
}

impl Signature {
    /// `signed_end`: the end of the last signed range.
    #[must_use]
    pub fn signed_end(&self) -> i64 {
        let n = self.byte_range.len();
        self.byte_range[n.saturating_sub(2)..].iter().sum()
    }
}

/// `read_bytes(function)`: a length query, then the bytes.
fn read_bytes(len: impl Fn(*mut u8, u64) -> u64) -> Vec<u8> {
    let length = len(std::ptr::null_mut(), 0);
    if length == 0 {
        return Vec::new();
    }
    let mut buffer = vec![0_u8; usize::try_from(length).unwrap_or(0)];
    len(buffer.as_mut_ptr(), length);
    buffer
}

fn signature(bindings: &dyn PdfiumLibraryBindings, handle: FPDF_SIGNATURE) -> Signature {
    // SAFETY: `handle` is a signature of an open document; the buffers are as long as given.
    unsafe {
        let count = bindings.FPDFSignatureObj_GetByteRange(handle, std::ptr::null_mut(), 0);
        let mut range = vec![0_i32; usize::try_from(count).unwrap_or(0)];
        if count > 0 {
            bindings.FPDFSignatureObj_GetByteRange(handle, range.as_mut_ptr(), count);
        }
        let contents = read_bytes(|buffer, length| {
            bindings.FPDFSignatureObj_GetContents(handle, buffer.cast(), length)
        });
        let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).replace('\0', "");
        let sub_filter = text(read_bytes(|buffer, length| {
            bindings.FPDFSignatureObj_GetSubFilter(handle, buffer.cast(), length)
        }));
        let time = text(read_bytes(|buffer, length| {
            bindings.FPDFSignatureObj_GetTime(handle, buffer.cast(), length)
        }));
        let reason = read_bytes(|buffer, length| {
            bindings.FPDFSignatureObj_GetReason(handle, buffer.cast(), length)
        });
        let units: Vec<u16> = reason
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let reason = String::from_utf16_lossy(&units).replace('\0', "");
        Signature {
            byte_range: range.into_iter().map(i64::from).collect(),
            contents,
            sub_filter,
            time,
            reason: (!reason.trim().is_empty()).then_some(reason),
        }
    }
}

/// The document's signatures; an error when `PDFium` cannot open it.
pub fn signatures(pdfium: &Pdfium, bytes: &[u8]) -> Result<Vec<Signature>, PdfError> {
    let bindings = pdfium.bindings();
    // SAFETY: the document is closed before returning and `bytes` outlives it.
    unsafe {
        let document = bindings.FPDF_LoadMemDocument64(bytes, None);
        if document.is_null() {
            return Err(PdfError::Unavailable(
                "Failed to load document from memory".into(),
            ));
        }
        let count = bindings.FPDF_GetSignatureCount(document);
        let mut out = Vec::new();
        for index in 0..count.max(0) {
            let handle = bindings.FPDF_GetSignatureObject(document, index);
            if handle.is_null() {
                bindings.FPDF_CloseDocument(document);
                return Err(PdfError::Unavailable(format!(
                    "Failed to load signature {index}, pointer is NULL."
                )));
            }
            out.push(signature(bindings, handle));
        }
        bindings.FPDF_CloseDocument(document);
        Ok(out)
    }
}

/// What [`merge`] made of the files.
pub enum Merged {
    Pdf(Vec<u8>),
    /// A file has an interactive form, which `FPDF_ImportAcroForm` (a patch of the `PDFium`
    /// build Rails uses) would carry over.
    WithForm,
}

/// `PdfUtils.merge`: the pages of every file, in order, in a new document.
pub fn merge(pdfium: &Pdfium, files: &[Vec<u8>]) -> Result<Merged, PdfError> {
    let mut merged = pdfium.create_new_pdf()?;
    for file in files {
        let source = pdfium.load_pdf_from_byte_slice(file, None)?;
        if source.form().is_some() {
            return Ok(Merged::WithForm);
        }
        merged.pages_mut().append(&source)?;
    }
    Ok(Merged::Pdf(merged.save_to_bytes()?))
}
