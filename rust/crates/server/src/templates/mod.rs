//! Template documents: `Templates::CreateAttachments` and `Templates::ProcessDocument`,
//! and the controllers that upload them (`TemplatesUploadsController`,
//! `TemplateDocumentsController`).
//!
//! Not supported yet (422 before anything is stored): ZIP archives, Word documents (Rails
//! converts them with the closed-source `WordCraft` gem) and uploads from a URL.

pub mod previews;
pub mod uploads;

use axum::body::Bytes;
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use loco_storage::{Attachment, Blob};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::{api::params::UploadedFile, state::AppState};

const PDF_CONTENT_TYPE: &str = "application/pdf";
const ZIP_CONTENT_TYPES: &[&str] = &["application/zip", "application/x-zip-compressed"];
/// `Templates::CreateAttachments::WORD_EXTENSIONS`.
const WORD_EXTENSIONS: &[&str] = &[".docx", ".doc", ".odt", ".rtf"];
/// `ActiveStorage::Attachment` names.
const DOCUMENTS: &str = "documents";
const PREVIEW_IMAGES: &str = "preview_images";

/// A created template document, with the metadata Rails holds in memory afterwards (it
/// includes `pdf.fields`, which is not stored).
#[derive(Debug)]
pub struct Document {
    pub attachment: Attachment,
    pub blob: Blob,
    pub metadata: Value,
    pub previews: Vec<(Attachment, Blob)>,
}

#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// `Templates::CreateAttachments::PdfEncrypted`.
    #[error("PDF encrypted")]
    PdfEncrypted,
    /// Any other error; documents created before it are kept, as in Rails.
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// The "not supported yet" message for files this port does not handle.
#[must_use]
pub fn unsupported(files: &[&UploadedFile]) -> Option<String> {
    for file in files {
        if ZIP_CONTENT_TYPES.contains(&file.content_type.as_str()) {
            return Some("Uploading ZIP archives is not supported yet".into());
        }
        if !is_pdf_or_image(&file.content_type)
            && WORD_EXTENSIONS.contains(&extname(&rack_filename(&file.filename)).to_lowercase().as_str())
        {
            return Some("Uploading Word documents is not supported yet".into());
        }
    }
    None
}

fn is_pdf_or_image(content_type: &str) -> bool {
    content_type.contains("image") || content_type == PDF_CONTENT_TYPE
}

/// Rack's multipart filename: percent-decoded when every `%` starts an escape, invalid
/// UTF-8 replaced, and only the part after the last `/` or `\`.
#[must_use]
pub fn rack_filename(filename: &str) -> String {
    let escapes_ok = filename.match_indices('%').all(|(i, _)| {
        let hex = filename.as_bytes().get(i + 1..i + 3);
        hex.is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit))
    });
    let decoded = if escapes_ok {
        String::from_utf8_lossy(
            &percent_encoding::percent_decode_str(filename).collect::<Vec<u8>>(),
        )
        .into_owned()
    } else {
        filename.to_owned()
    };
    decoded
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// `File.extname`: the last extension of the basename, with its dot; none for dotfiles.
#[must_use]
pub fn extname(filename: &str) -> &str {
    let base = filename.rsplit('/').next().unwrap_or(filename);
    let trimmed = base.trim_start_matches('.');
    match trimmed.rfind('.') {
        Some(i) if i + 1 < trimmed.len() => &trimmed[i..],
        _ => "",
    }
}

/// `File.basename(filename, '.*')`.
#[must_use]
pub fn basename(filename: &str) -> String {
    let base = filename.rsplit('/').next().unwrap_or(filename);
    let ext = extname(base);
    base[..base.len() - ext.len()].to_owned()
}

/// `Marcel::MimeType.for(io, name:, declared_type:)`, as Active Storage identifies
/// uploads: the content's magic bytes, else the declared type, else the extension.
#[must_use]
pub fn identify(data: &[u8], filename: &str, declared: &str) -> String {
    let magic: &[(&[u8], usize, &str)] = &[
        (b"%PDF-", 0, "application/pdf"),
        (b"\xEF\xBB\xBF%PDF-", 0, "application/pdf"),
        (b"\xFF\xD8\xFF", 0, "image/jpeg"),
        (b"\x89PNG\r\n\x1a\n", 0, "image/png"),
        (b"GIF87a", 0, "image/gif"),
        (b"GIF89a", 0, "image/gif"),
        (b"BM", 0, "image/bmp"),
        (b"WEBP", 8, "image/webp"),
        (b"II*\0", 0, "image/tiff"),
        (b"MM\0*", 0, "image/tiff"),
        (b"\0\0\x01\0", 0, "image/vnd.microsoft.icon"),
    ];
    for (bytes, offset, kind) in magic {
        if data.get(*offset..offset + bytes.len()) == Some(bytes)
            && (*offset == 0 || data.starts_with(b"RIFF"))
        {
            return (*kind).to_owned();
        }
    }
    if !declared.is_empty() && declared != "application/octet-stream" {
        return declared.to_owned();
    }
    mime_guess::from_path(filename)
        .first()
        .map_or_else(|| "application/octet-stream".to_owned(), |m| m.to_string())
}

fn sha256(data: &[u8]) -> String {
    URL_SAFE.encode(Sha256::digest(data))
}

/// `Templates::CreateAttachments.call(template, params, extract_fields:)` for PDF and
/// image files ([`unsupported`] filters out the rest first). Files of other types fail
/// with `InvalidFileType`.
pub async fn create_attachments(
    state: &AppState,
    template_id: i64,
    files: &[&UploadedFile],
    password: Option<&str>,
    extract_fields: bool,
) -> Result<Vec<Document>, CreateError> {
    let mut documents = Vec::new();
    for file in files {
        if !is_pdf_or_image(&file.content_type) {
            return Err(anyhow::anyhow!(
                "Templates::CreateAttachments::InvalidFileType: {}/false",
                file.content_type
            )
            .into());
        }
        documents.push(handle_pdf_or_image(state, template_id, file, password, extract_fields).await?);
    }
    Ok(documents)
}

async fn handle_pdf_or_image(
    state: &AppState,
    template_id: i64,
    file: &UploadedFile,
    password: Option<&str>,
    extract_fields: bool,
) -> Result<Document, CreateError> {
    let filename = rack_filename(&file.filename).replace('/', "-");
    let attachment_uuid = uuid::Uuid::new_v4().to_string();

    if file.content_type != PDF_CONTENT_TYPE {
        let content_type = identify(&file.data, &filename, &file.content_type);
        // `{ identified: false, analyzed: false, sha256: }.compact_blank`, then
        // `identified` by the upload and `analyzed` by the (null) analyzer.
        let metadata = json!({
            "sha256": sha256(&file.data),
            "identified": true,
            "analyzed": true,
        });
        let (attachment, blob) = create_document(
            state,
            template_id,
            &attachment_uuid,
            file.data.clone(),
            &filename,
            &content_type,
            &metadata,
        )
        .await?;
        let mut document = Document {
            attachment,
            blob,
            metadata,
            previews: Vec::new(),
        };
        if content_type == PDF_CONTENT_TYPE {
            // `generate_pdf_preview_images` without a PDFium document.
            return Err(anyhow::anyhow!("NoMethodError: undefined method 'page_count' for nil").into());
        }
        if content_type.starts_with("image/") {
            let data = file.data.clone();
            let preview = tokio::task::spawn_blocking(move || {
                previews::image_png(&data, &content_type)
            })
            .await
            .map_err(anyhow::Error::from)?
            .map_err(anyhow::Error::from)?;
            document.previews = upload_previews(state, document.attachment.id, vec![preview]).await?;
        }
        return Ok(document);
    }

    let data = file.data.clone();
    let password = password.map(str::to_owned);
    let uuid = attachment_uuid.clone();
    let processed = tokio::task::spawn_blocking(move || {
        loco_pdf::with_shared(|pdfium| {
            loco_pdf::upload::process(pdfium, &data, password.as_deref(), &uuid, extract_fields)
        })
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(anyhow::Error::from)?;
    let processed = match processed {
        Ok(processed) => processed,
        Err(loco_pdf::upload::OpenError::Password) => return Err(CreateError::PdfEncrypted),
        Err(err) => return Err(anyhow::Error::from(err).into()),
    };

    let data = Bytes::from(processed.data);
    let content_type = identify(&data, &filename, PDF_CONTENT_TYPE);
    let mut metadata = Map::new();
    metadata.insert("identified".into(), true.into());
    metadata.insert("analyzed".into(), true.into());
    if !processed.annotations.is_empty() {
        metadata.insert("pdf".into(), json!({ "annotations": processed.annotations }));
    }
    metadata.insert("sha256".into(), sha256(&data).into());
    if content_type == PDF_CONTENT_TYPE {
        let pdf = metadata.entry("pdf").or_insert_with(|| json!({}));
        pdf["number_of_pages"] = processed.number_of_pages.into();
    }
    let stored = Value::Object(metadata);
    let (attachment, blob) = create_document(
        state,
        template_id,
        &attachment_uuid,
        data,
        &filename,
        &content_type,
        &stored,
    )
    .await?;
    let mut document = Document {
        attachment,
        blob,
        metadata: stored,
        previews: Vec::new(),
    };
    if content_type != PDF_CONTENT_TYPE {
        return Ok(document);
    }

    let bitmaps = processed.previews.map_err(anyhow::Error::msg)?;
    let pngs = tokio::task::spawn_blocking(move || {
        bitmaps
            .iter()
            .map(previews::page_png)
            .collect::<Result<Vec<_>, _>>()
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(anyhow::Error::from)?;
    document.previews = upload_previews(state, document.attachment.id, pngs).await?;
    if let Some(fields) = processed.fields {
        document.metadata["pdf"]["fields"] = Value::Array(fields);
    }
    Ok(document)
}

/// `create_document`: the blob and the template's `documents` attachment.
async fn create_document(
    state: &AppState,
    template_id: i64,
    attachment_uuid: &str,
    data: Bytes,
    filename: &str,
    content_type: &str,
    metadata: &Value,
) -> anyhow::Result<(Attachment, Blob)> {
    let blob = state
        .storage
        .create_and_upload_exact(&state.db, data, filename, Some(content_type), metadata.clone())
        .await?;
    let attachment = Attachment::create_for_blob(
        &state.db,
        "Template",
        template_id,
        DOCUMENTS,
        blob.id,
        attachment_uuid,
    )
    .await?;
    Ok((attachment, blob))
}

/// `build_and_upload_blob` for each preview, then their `preview_images` attachments.
async fn upload_previews(
    state: &AppState,
    attachment_id: i64,
    pngs: Vec<previews::Preview>,
) -> anyhow::Result<Vec<(Attachment, Blob)>> {
    let mut blobs = Vec::new();
    for (index, png) in pngs.into_iter().enumerate() {
        let metadata = json!({
            "analyzed": true,
            "identified": true,
            "width": png.width,
            "height": png.height,
        });
        blobs.push(
            state
                .storage
                .create_and_upload_exact(
                    &state.db,
                    Bytes::from(png.data),
                    &format!("{index}.png"),
                    Some("image/png"),
                    metadata,
                )
                .await?,
        );
    }
    let mut previews = Vec::new();
    for blob in blobs {
        let attachment = Attachment::create_for_blob(
            &state.db,
            "ActiveStorage::Attachment",
            attachment_id,
            PREVIEW_IMAGES,
            blob.id,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await?;
        previews.push((attachment, blob));
    }
    Ok(previews)
}

/// `Templates::ProcessDocument.normalize_attachment_fields`: the extracted fields of the
/// documents, assigned to the template's first party.
#[must_use]
pub fn normalize_attachment_fields(documents: &mut [Document], submitter_uuid: &str) -> Vec<Value> {
    let mut fields = Vec::new();
    for document in documents {
        let Some(pdf) = document.metadata.get_mut("pdf").and_then(Value::as_object_mut) else {
            continue;
        };
        let Some(Value::Array(extracted)) = pdf.shift_remove("fields") else {
            continue;
        };
        for mut field in extracted {
            field["submitter_uuid"] = submitter_uuid.into();
            fields.push(field);
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_filenames_like_ruby() {
        assert_eq!(basename("contract.final.pdf"), "contract.final");
        assert_eq!(basename(".pdf"), ".pdf");
        assert_eq!(basename("scan"), "scan");
        assert_eq!(extname("Offer.DOCX"), ".DOCX");
        assert_eq!(rack_filename("C:\\docs\\a%20b.pdf"), "a b.pdf");
        assert_eq!(rack_filename("100%.pdf"), "100%.pdf");
    }

    #[test]
    fn identifies_content_like_marcel() {
        assert_eq!(identify(b"%PDF-1.7", "a.png", "image/png"), "application/pdf");
        assert_eq!(identify(b"\x89PNG\r\n\x1a\nxx", "a.jpg", "image/jpeg"), "image/png");
        assert_eq!(identify(b"<svg/>", "a.svg", "image/svg+xml"), "image/svg+xml");
        assert_eq!(identify(b"??", "a.gif", "application/octet-stream"), "image/gif");
    }

    #[test]
    fn rejects_unported_file_types() {
        let file = |name: &str, content_type: &str| UploadedFile {
            filename: name.into(),
            content_type: content_type.into(),
            data: Bytes::new(),
        };
        let docx = file(
            "offer.docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        );
        let zip = file("all.zip", "application/zip");
        let pdf = file("a.pdf", "application/pdf");
        assert!(unsupported(&[&pdf]).is_none());
        assert!(unsupported(&[&pdf, &docx]).unwrap().contains("Word documents"));
        assert!(unsupported(&[&zip]).unwrap().contains("ZIP archives"));
    }
}
