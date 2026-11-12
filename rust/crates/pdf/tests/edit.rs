//! End to end: overlay text and images on a fixture, sign it, and render the result.

use std::{path::Path, sync::Arc};

use loco_pdf::edit::{
    Editor,
    layout::{Align, Fragment, Input, Layouter, TextStyle, Valign},
    pdfa::Pdfa,
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
            None,
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
    // The earlier revision's text keeps its own font (the next signer's resources must not
    // replace it under the same name).
    assert!(text.contains("Ana Exämple"), "{text}");
}

fn pdf_text(pdf: &[u8]) -> Vec<String> {
    let pdfium = loco_pdf::load_pdfium(&loco_pdf::default_library_dir()).unwrap();
    let document = pdfium.load_pdf_from_byte_slice(pdf, None).unwrap();
    document
        .pages()
        .iter()
        .map(|p| p.text().unwrap().all())
        .collect()
}

#[test]
fn merges_pages_of_signed_documents() {
    let key = signing_key();
    let mut sources = Vec::new();
    for text in ["Front page", "Back page"] {
        let mut editor = Editor::load(fixture()).unwrap();
        draw(&mut editor, "Helvetica", text);
        sources.push(
            editor
                .save_signed("D:20261107120000", "20261107120000", "Signed", &key, None)
                .unwrap(),
        );
    }
    let slices: Vec<&[u8]> = sources.iter().map(Vec::as_slice).collect();
    let merged = Editor::merge(&slices, "D:20261107120000").unwrap();
    assert_eq!(merged.page_ids().len(), 2);
    let saved = merged
        .save_signed("D:20261107120000", "20261107120000", "Signed", &key, None)
        .unwrap();
    let text = pdf_text(&saved);
    assert_eq!(text.len(), 2);
    assert!(
        text[0].contains("Front page") && text[1].contains("Back page"),
        "{text:?}"
    );
    // Each page keeps the signature field it had, plus the merged document's own.
    assert_eq!(
        saved
            .windows(b"/ByteRange".len())
            .filter(|w| *w == b"/ByteRange")
            .count(),
        3
    );
}

#[test]
fn writes_pdfa_metadata_from_the_info_dictionary() {
    let mut editor = Editor::load(fixture()).unwrap();
    draw(&mut editor, "GoNotoKurrent-Regular.ttf", "Ana");
    editor.set_info_text("Creator", "DocuSeal (https://www.docuseal.com)");
    editor.set_pdfa(Pdfa {
        conformance: 'B',
        fix_glyph_widths: true,
    });
    let saved = editor.save("D:20261107120000").unwrap();
    let text = String::from_utf8_lossy(&saved);
    let xmp = &text[text.find("<x:xmpmeta").unwrap()..text.find("</x:xmpmeta>").unwrap()];
    // `parse_metadata` order: dc (Author), xmp (Creator, dates), pdf (Producer), pdfaid.
    let order = [
        "<rdf:li>Evangelos Vlachogiannis</rdf:li>",
        "<xmp:CreatorTool>DocuSeal (https://www.docuseal.com)</xmp:CreatorTool>",
        "<xmp:CreateDate>2007-02-23T17:56:37+02:00</xmp:CreateDate>",
        "<xmp:ModifyDate>2026-11-07T12:00:00+00:00</xmp:ModifyDate>",
        "<pdf:Producer>HexaPDF version 1.7.0</pdf:Producer>",
        "<pdfaid:part>3</pdfaid:part>\n<pdfaid:conformance>B</pdfaid:conformance>",
    ];
    let positions: Vec<usize> = order
        .iter()
        .map(|p| xmp.find(p).unwrap_or_else(|| panic!("{p} in {xmp}")))
        .collect();
    assert!(positions.is_sorted(), "{xmp}");
    assert!(text.contains("/S /GTS_PDFA1") || text.contains("/S/GTS_PDFA1"));
    assert!(pdf_text(&saved)[0].contains("Ana"));
}

#[test]
fn embeds_the_timestamp_token_of_the_signature() {
    let mut editor = Editor::load(fixture()).unwrap();
    draw(&mut editor, "Helvetica", "Stamped");
    let mut seen = Vec::new();
    let mut stamp = |signature: &[u8]| {
        seen.push(signature.to_vec());
        vec![
            0x18, 0x0F, b'2', b'0', b'2', b'6', b'1', b'1', b'0', b'7', b'1', b'2', b'0', b'0',
            b'0', b'0', b'Z',
        ]
    };
    let signed = editor
        .save_signed(
            "D:20261107120000",
            "20261107120000",
            "Signed",
            &signing_key(),
            Some(&mut stamp),
        )
        .unwrap();
    assert_eq!(seen.len(), 1, "one token for the signature value");
    assert_eq!(seen[0].len(), 256, "the RSA-2048 signature value");
    // `/Contents` is 20 000 bytes (`signature_size: 20_000`), the token in `[1]` unsigned
    // attributes under id-aa-timeStampToken.
    let start = signed.windows(10).position(|w| w == b"/Contents<").unwrap() + 10;
    let end = start + signed[start..].iter().position(|b| *b == b'>').unwrap();
    assert_eq!(end - start, 40_000);
    let hex = std::str::from_utf8(&signed[start..end]).unwrap();
    assert!(
        hex.contains("060b2a864886f70d010910020e"),
        "id-aa-timeStampToken"
    );
    assert!(
        hex.contains("180f32303236313130373132303030305a"),
        "the token"
    );
}

#[test]
fn loads_signing_keys_from_pkcs12() {
    use p12_keystore::{Certificate, KeyStore, KeyStoreEntry, PrivateKey, PrivateKeyChain};
    let key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let pem = key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
    let pair = rcgen::KeyPair::from_pkcs8_pem_and_sign_algo(&pem, &rcgen::PKCS_RSA_SHA256).unwrap();
    let ca_key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let ca_pem = ca_key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
    let ca_pair =
        rcgen::KeyPair::from_pkcs8_pem_and_sign_algo(&ca_pem, &rcgen::PKCS_RSA_SHA256).unwrap();
    let ca = rcgen::CertificateParams::new(Vec::<String>::new())
        .unwrap()
        .self_signed(&ca_pair)
        .unwrap();
    let cert = rcgen::CertificateParams::new(Vec::<String>::new())
        .unwrap()
        .self_signed(&pair)
        .unwrap();
    let der = key.to_pkcs8_der().unwrap();
    let mut store = KeyStore::new();
    store.add_entry(
        "parity",
        KeyStoreEntry::PrivateKeyChain(PrivateKeyChain::new(
            vec![1, 2, 3],
            PrivateKey::from_der(der.as_bytes()).unwrap(),
            // The CA first: the signing certificate is the one matching the key.
            [
                Certificate::from_der(ca.der()).unwrap(),
                Certificate::from_der(cert.der()).unwrap(),
            ],
        )),
    );
    let p12 = store.writer("secret").write().unwrap();
    let loaded = SigningKey::from_pkcs12(&p12, "secret").unwrap();
    assert_eq!(loaded.certificate, cert.der().to_vec());
    assert_eq!(loaded.chain, vec![ca.der().to_vec()]);
    assert!(SigningKey::from_pkcs12(&p12, "wrong").is_err());
}

/// A one-page PDF with a text field and a checked check box, neither with appearances, in
/// a form that sets `NeedAppearances`.
fn form_pdf() -> Vec<u8> {
    use lopdf::{Dictionary, Document, Object, Stream, dictionary};
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let helvetica = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let text = doc.add_object(dictionary! {
        "FT" => "Tx", "T" => Object::string_literal("name"), "V" => Object::string_literal("Jane Doe"),
        "DA" => Object::string_literal("0 g /Helv 12 Tf"), "Type" => "Annot", "Subtype" => "Widget",
        "Rect" => vec![50.into(), 700.into(), 250.into(), 720.into()],
    });
    let check = doc.add_object(dictionary! {
        "FT" => "Btn", "T" => Object::string_literal("agree"), "V" => "Yes", "AS" => "Yes",
        "Type" => "Annot", "Subtype" => "Widget",
        "Rect" => vec![50.into(), 650.into(), 66.into(), 666.into()],
    });
    let content = doc.add_object(Stream::new(Dictionary::new(), Vec::new()));
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        "Annots" => vec![text.into(), check.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
        ),
    );
    let form = dictionary! {
        "Fields" => vec![text.into(), check.into()], "NeedAppearances" => true,
        "DR" => dictionary! { "Font" => dictionary! { "Helv" => helvetica } },
    };
    let catalog = doc
        .add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id, "AcroForm" => form });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn creates_field_appearances_and_flattens_them() {
    let mut editor = Editor::load(form_pdf()).unwrap();
    assert!(editor.needs_appearances());
    editor.create_form_appearances().unwrap();
    editor.flatten_form();
    let saved = editor.save("D:20261107120000").unwrap();
    let text = pdf_text(&saved);
    assert!(text[0].contains("Jane Doe"), "{text:?}");
    // The check mark is ZapfDingbats' ✔ (a20, drawn as code 0x34 in the built-in encoding).
    let document = lopdf::Document::load_mem(&saved).unwrap();
    let catalog = document.catalog().unwrap();
    assert!(!catalog.has(b"AcroForm"), "every field was flattened");
    assert!(String::from_utf8_lossy(&saved).contains("/ZapfDingbats"));
}

#[test]
fn refuses_text_its_font_cannot_encode() {
    let mut editor = Editor::load(form_pdf()).unwrap();
    // A value with a character WinAnsi has no code for.
    let field = editor
        .doc
        .objects
        .iter()
        .find(|(_, o)| {
            o.as_dict()
                .is_ok_and(|d| d.has(b"FT") && d.get(b"FT").unwrap().as_name().unwrap() == b"Tx")
        })
        .map(|(id, _)| *id)
        .unwrap();
    editor
        .doc
        .get_object_mut(field)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("V", lopdf::text_string("Ж"));
    let err = editor.create_form_appearances().unwrap_err();
    assert!(err.to_string().contains("not supported yet"), "{err}");
}
