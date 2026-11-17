//! `Submitters::CreateStampAttachment.build_attachment`: the stamp image a completed
//! party's stamp fields show. The `DocuSeal` logo (or nothing), lightened by a half
//! transparent white layer, under black text "Digitally signed by:", the name and email
//! (bold), the role when there are several parties, and the completion time, laid out
//! as `Vips::Image.text` does (`loco_pdf::text_image`).
//!
//! The PNG is not byte-identical to libvips': the anti-aliasing of the text differs slightly
//! from `FreeType`'s.

use chrono::NaiveDateTime;
use image::{Rgba, RgbaImage, imageops};
use loco_pdf::text_image::{self, Run};
use loco_storage::Attachment;
use serde_json::{Value, json};

use super::Form;
use crate::{errors::AppError, state::AppState};

const WIDTH: u32 = 400;
const HEIGHT: u32 = 200;
/// `<span size="90">`.
const SIZE: f64 = 90.0;
const LRM: char = '\u{200E}';
const LOGO: &[u8] = include_bytes!("../../../../../lib/pdf_icons/stamp-logo.png");

/// `build_attachment(submitter, with_logo:)`: the uuid of the party's attachment with
/// this image (an existing one with the same checksum, or a new one).
pub async fn build_attachment(
    state: &AppState,
    form: &Form,
    completed_at: NaiveDateTime,
    with_logo: bool,
) -> Result<String, AppError> {
    let image = stamp_image(state, form, completed_at, with_logo)?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| anyhow::anyhow!("stamp image: {e}"))?;
    let checksum = loco_storage::checksum(&png);
    for (attachment, blob) in Attachment::with_blobs_for_records(
        &state.db,
        "Submitter",
        &[form.submitter.id],
        "attachments",
    )
    .await?
    {
        if blob.checksum.as_deref() == Some(checksum.as_str()) {
            return Ok(attachment.uuid);
        }
    }
    let blob = state
        .storage
        .create_and_upload_exact(
            &state.db,
            png.into(),
            "stamp.png",
            Some("image/png"),
            json!({ "analyzed": true, "identified": true, "width": WIDTH, "height": HEIGHT }),
        )
        .await?;
    let attachment = Attachment::create(
        &state.db,
        "Submitter",
        form.submitter.id,
        "attachments",
        &blob,
    )
    .await?;
    Ok(attachment.uuid)
}

/// `generate_stamp_image(submitter, with_logo:)`.
fn stamp_image(
    state: &AppState,
    form: &Form,
    completed_at: NaiveDateTime,
    with_logo: bool,
) -> Result<RgbaImage, AppError> {
    let mut base = RgbaImage::from_pixel(WIDTH, HEIGHT, Rgba([255, 255, 255, 255]));
    if with_logo {
        let logo = image::load_from_memory(LOGO)
            .map_err(|e| anyhow::anyhow!("stamp logo: {e}"))?
            .to_rgba8();
        let scale = (f64::from(WIDTH) / f64::from(logo.width()))
            .min(f64::from(HEIGHT) / f64::from(logo.height()));
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "sizes"
        )]
        let (w, h) = (
            (f64::from(logo.width()) * scale).round() as u32,
            (f64::from(logo.height()) * scale).round() as u32,
        );
        let logo = imageops::resize(&logo, w, h, imageops::FilterType::Lanczos3);
        composite(
            &mut base,
            &logo,
            i64::from((WIDTH - w) / 2),
            i64::from((HEIGHT - h) / 2),
        );
    }
    // The half transparent white layer over the logo.
    let opacity = RgbaImage::from_pixel(WIDTH, HEIGHT, Rgba([255, 255, 255, 127]));
    composite(&mut base, &opacity, 0, 0);

    let text = text_image::text(&runs(state, form, completed_at), SIZE, 400, 200)
        .ok_or_else(|| anyhow::anyhow!("stamp text: no fonts"))?;
    let mut layer = RgbaImage::new(text.width, text.height);
    for (pixel, alpha) in layer.pixels_mut().zip(&text.pixels) {
        *pixel = Rgba([0, 0, 0, *alpha]);
    }
    // `(WIDTH - text_layer.width) / 2` with Ruby's floor division.
    let x = (i64::from(WIDTH) - i64::from(text.width)).div_euclid(2);
    let y = (i64::from(HEIGHT) - i64::from(text.height)).div_euclid(2);
    composite(&mut base, &layer, x, y);
    Ok(base)
}

/// `composite(overlay, 'over', x:, y:)` onto an opaque image.
fn composite(base: &mut RgbaImage, overlay: &RgbaImage, x: i64, y: i64) {
    for (ox, oy, src) in overlay.enumerate_pixels() {
        let (bx, by) = (x + i64::from(ox), y + i64::from(oy));
        let (Ok(bx), Ok(by)) = (u32::try_from(bx), u32::try_from(by)) else {
            continue;
        };
        if bx >= base.width() || by >= base.height() {
            continue;
        }
        let dst = base.get_pixel_mut(bx, by);
        let a = f64::from(src[3]) / 255.0;
        for c in 0..3 {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "0..=255"
            )]
            let value = (f64::from(src[c]) * a + f64::from(dst[c]) * (1.0 - a)).round() as u8;
            dst[c] = value;
        }
    }
}

/// `String#present?`.
fn present(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|s| !s.trim().is_empty())
}

/// `build_text_image(submitter)`: the markup's runs.
fn runs(state: &AppState, form: &Form, completed_at: NaiveDateTime) -> Vec<Run> {
    let account = &form.account;
    let locale = account.locale.as_str();
    let time = crate::l10n::time(state, locale, completed_at, &account.timezone, "long");
    let timezone = crate::l10n::timezone_abbr(&account.timezone, completed_at);
    let submitter = &form.submitter;
    // `build_name`: "name email", else the first of name, email and phone that is set
    // (an empty name still counts).
    let name = match (
        present(submitter.name.as_ref()),
        present(submitter.email.as_ref()),
    ) {
        (Some(name), Some(email)) => format!("{name} {email}"),
        _ => submitter
            .name
            .clone()
            .or_else(|| submitter.email.clone())
            .or_else(|| submitter.phone.clone())
            .unwrap_or_default(),
    };
    // `build_role`.
    let role = if form.template_submitters().len() > 1 {
        let item_name = form
            .template_submitters()
            .iter()
            .find(|e| e.get("uuid").and_then(Value::as_str) == Some(&submitter.uuid))
            .and_then(|e| e.get("name"))
            .map(super::results::ruby_to_s)
            .unwrap_or_default();
        format!("{}: {item_name}\n", state.views.t(locale, "role", &[]))
    } else {
        String::new()
    };
    let digitally_signed_by = state.views.t(locale, "digitally_signed_by", &[]);
    vec![
        Run {
            text: format!("{LRM}{digitally_signed_by}:\n{LRM}"),
            bold: false,
        },
        Run {
            text: name,
            bold: true,
        },
        Run {
            text: format!("\n{LRM}{role}{time} {timezone}"),
            bold: false,
        },
    ]
}
