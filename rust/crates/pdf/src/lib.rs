//! PDF rendering, modification, and signing. Replaces `lib/pdfium.rb`, `lib/pdf_utils.rb`,
//! and the `HexaPDF` usage in `lib/submissions/`.

pub mod edit;
pub mod text_image;

use std::path::{Path, PathBuf};

use pdfium_render::prelude::{Pdfium, PdfiumError};

#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    #[error("unable to load libpdfium from {path}: {source}")]
    Load { path: PathBuf, source: PdfiumError },
    #[error(transparent)]
    Pdfium(#[from] PdfiumError),
}

/// Directory holding libpdfium: `PDFIUM_LIB_DIR`, else `rust/vendor/pdfium/lib`
/// (populated by `rust/scripts/fetch-pdfium.sh`).
#[must_use]
pub fn default_library_dir() -> PathBuf {
    std::env::var_os("PDFIUM_LIB_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/pdfium/lib"),
        PathBuf::from,
    )
}

pub fn load_pdfium(dir: &Path) -> Result<Pdfium, PdfError> {
    let path = Pdfium::pdfium_platform_library_name_at_path(dir);
    let bindings = Pdfium::bind_to_library(&path).map_err(|source| PdfError::Load {
        path: path.clone(),
        source,
    })?;

    Ok(Pdfium::new(bindings))
}

/// Number of pages in a PDF document.
pub fn page_count(pdfium: &Pdfium, bytes: &[u8]) -> Result<usize, PdfError> {
    let document = pdfium.load_pdf_from_byte_slice(bytes, None)?;

    Ok(usize::try_from(document.pages().len()).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sample_fixture() {
        let pdfium = load_pdfium(&default_library_dir()).expect("run rust/scripts/fetch-pdfium.sh");
        let bytes = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../spec/fixtures/sample-document.pdf"),
        )
        .unwrap();

        assert!(page_count(&pdfium, &bytes).unwrap() >= 1);
    }
}
