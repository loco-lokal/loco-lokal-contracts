//! End to end: overlay text and images on a fixture, sign it, and render the result.

use std::{path::Path, sync::Arc};

use loco_pdf::edit::{
    Editor,
    layout::{Align, Fragment, Input, Layouter, TextStyle, Valign},
    sign::SigningKey,
};
use rsa::{RsaPrivateKey, pkcs8::EncodePrivateKey};

fn fixture() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../spec/fixtures/sample-document.pdf"),
    )
    .unwrap()
}

fn signing_key() -> SigningKey {
    let key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let pem = key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
    let pair = rcgen::KeyPair::from_pkcs8_pem_and_sign_algo(&pem, &rcgen::PKCS_RSA_SHA256).unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Test Signer");
    let cert = params.self_signed(&pair).unwrap();
    SigningKey {
        certificate: cert.der().to_vec(),
        chain: Vec::new(),
        key,
    }
}

fn draw(editor: &mut Editor, family: &str, text: &str) {
    let page = editor.page_ids()[0];
    let [_, _, _, height] = editor.page_box(page);
    let (font, font_key) = editor.font(family, "none").unwrap();
    let style = Arc::new(TextStyle {
        font,
        font_key,
        font_size: 12.0,
        line_height: None,
        fill_color: Some([0.0, 0.0, 1.0]),
        white_underlay: false,
        underline: false,
        link: None,
    });
    let layouter = Layouter::new(Align::Left, Valign::Top);
    let fitted = layouter.fit(
        vec![Input::Text(Fragment::new(text, &style))],
        200.0,
        100.0,
        &style,
    );
    editor.draw_text(page, &fitted, 50.0, height - 50.0);
}

#[test]
fn overlays_signs_and_renders() {
    let original = fixture();
    let mut editor = Editor::load(original.clone()).unwrap();
    editor.rotate_pages();
    editor.flatten_form();
    let page = editor.page_ids()[0];
    editor.remove_field_links(page);
    draw(
        &mut editor,
        "GoNotoKurrent-Regular.ttf",
        "Ana Exämple — signed ✓",
    );
    draw(&mut editor, "Helvetica", "Grüße € ✓");
    let check = image::load_from_memory(
        &std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../lib/pdf_icons/check.png"),
        )
        .unwrap(),
    )
    .unwrap();
    let info = editor.add_image(&check, false).unwrap();
    let name = editor.use_image(page, info.index);
    editor.overlay(page).image(&name, 100.0, 100.0, 24.0, 24.0);
    editor.add_link(page, [10.0, 10.0, 60.0, 30.0], "https://example.test/file");
    editor.set_info(
        "Creator",
        lopdf::Object::string_literal("DocuSeal (https://www.docuseal.com)"),
    );

    let signed = editor
        .save_signed(
            "D:20261107120000+00'00'",
            "20261107120000",
            "Signed with DocuSeal.com",
            &signing_key(),
        )
        .unwrap();
    assert!(
        signed.starts_with(&original),
        "an incremental update keeps the original bytes"
    );

    let out = std::env::temp_dir().join("loco-pdf-signed.pdf");
    std::fs::write(&out, &signed).unwrap();

    let pdfium = loco_pdf::load_pdfium(&loco_pdf::default_library_dir()).unwrap();
    let document = pdfium.load_pdf_from_byte_slice(&signed, None).unwrap();
    let text = document.pages().get(0).unwrap().text().unwrap().all();
    assert!(text.contains("Ana Exämple"), "{text}");
    assert!(text.contains("Grüße €"), "{text}");

    // A second, unsigned revision on top of the signed one (the next signer).
    let mut next = Editor::load(signed.clone()).unwrap();
    draw(&mut next, "GoNotoKurrent-Regular.ttf", "Bob");
    let updated = next.save("D:20261107120100+00'00'").unwrap();
    assert!(updated.starts_with(&signed));
    let document = pdfium.load_pdf_from_byte_slice(&updated, None).unwrap();
    let text = document.pages().get(0).unwrap().text().unwrap().all();
    assert!(text.contains("Bob"), "{text}");
    assert!(
        text.contains("Ana Exämple"),
        "the earlier revision's fonts stay: {text}"
    );
}

#[test]
fn imports_pages_into_a_new_document() {
    let mut first = Editor::load(fixture()).unwrap();
    first.rotate_pages();
    first.flatten_form();
    draw(&mut first, "GoNotoKurrent-Regular.ttf", "First Party");
    let first_pages = first.page_ids().len();

    let mut second = Editor::blank().unwrap();
    second.add_page(595.0, 842.0).unwrap();
    draw(&mut second, "Helvetica", "Audit Log");

    let mut combined = Editor::blank().unwrap();
    combined
        .import_pages(&first.into_document().unwrap())
        .unwrap();
    combined
        .import_pages(&second.into_document().unwrap())
        .unwrap();
    assert_eq!(combined.page_ids().len(), first_pages + 1);
    let bytes = combined
        .save_signed(
            "D:20261107120000+00'00'",
            "20261107120000",
            "Signed with DocuSeal.com",
            &signing_key(),
        )
        .unwrap();

    let pdfium = loco_pdf::load_pdfium(&loco_pdf::default_library_dir()).unwrap();
    let document = pdfium.load_pdf_from_byte_slice(&bytes, None).unwrap();
    assert_eq!(
        usize::try_from(document.pages().len()).unwrap(),
        first_pages + 1
    );
    let text = |index: i32| document.pages().get(index).unwrap().text().unwrap().all();
    assert!(text(0).contains("First Party"), "{}", text(0));
    let last = i32::try_from(first_pages).unwrap();
    assert!(text(last).contains("Audit Log"), "{}", text(last));
}
