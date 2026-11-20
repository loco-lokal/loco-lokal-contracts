//! `loco_pdf::upload` on PDFs from `scripts/parity/uploads_seed.rb` (HexaPDF): the fields
//! and links Rails extracts from them (checked against Rails by `uploads.py`, and against
//! Rails' output in `upload-golden.json`).

use std::collections::HashMap;
use std::path::Path;

use loco_pdf::upload::{OpenError, process};
use serde_json::Value;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

#[test]
fn extracts_form_fields_links_and_previews() {
    let bytes = fixture("upload-form.pdf");
    let processed = loco_pdf::with_shared(|pdfium| process(pdfium, &bytes, None, "doc", true))
        .expect("run rust/scripts/fetch-pdfium.sh")
        .unwrap();

    assert_eq!(processed.number_of_pages, 2);
    assert_eq!(processed.annotations.len(), 1);
    assert_eq!(
        processed.annotations[0]["value"],
        "https://example.com/rotated"
    );

    let fields = processed.fields.unwrap();
    let summary: Vec<(String, String)> = fields
        .iter()
        .map(|f| {
            (
                f["name"].as_str().unwrap().to_owned(),
                f["type"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let expected = [
        ("Full Name", "text"),
        ("Centered", "text"),
        ("Code", "cells"),
        ("Start Date", "date"),
        ("Agree", "checkbox"),
        ("Plan", "radio"),
        ("Country", "select"),
        ("Signature", "signature"),
        ("Initials", "initials"),
        ("Notes 2", "text"),
    ];
    assert_eq!(
        summary,
        expected
            .iter()
            .map(|(n, t)| ((*n).to_owned(), (*t).to_owned()))
            .collect::<Vec<_>>()
    );
    let name = &fields[0];
    assert_eq!(name["required"], true);
    assert_eq!(name["description"], "Your full name");
    assert_eq!(name["areas"][0]["x"], 0.083_994_711_368_148_97);
    assert_eq!(name["areas"][0]["attachment_uuid"], "doc");
    assert_eq!(fields[3]["preferences"]["format"], "MM/DD/YYYY");
    let plan = &fields[5];
    let options: Vec<&Value> = plan["options"].as_array().unwrap().iter().collect();
    assert_eq!(options[0]["value"], "Basic");
    assert_eq!(plan["areas"][1]["option_uuid"], options[1]["uuid"]);
    // The "Select a country" placeholder is dropped.
    assert_eq!(fields[6]["options"].as_array().unwrap().len(), 2);

    let previews = processed.previews.unwrap();
    let sizes: Vec<(u32, u32)> = previews.iter().map(|b| (b.width, b.height)).collect();
    assert_eq!(sizes, [(1400, 1980), (1400, 990)]);
}

#[test]
fn needs_the_password_and_decrypts_with_it() {
    let bytes = fixture("upload-encrypted.pdf");
    let without =
        loco_pdf::with_shared(|pdfium| process(pdfium, &bytes, None, "doc", true)).unwrap();
    assert!(matches!(without, Err(OpenError::Password)));

    let with = loco_pdf::with_shared(|pdfium| process(pdfium, &bytes, Some("secret"), "doc", true))
        .unwrap()
        .unwrap();
    assert_ne!(with.data, bytes);
    assert!(!String::from_utf8_lossy(&with.data).contains("/Encrypt"));
    assert_eq!(with.fields.unwrap(), Vec::<Value>::new());
}

/// Replaces uuids with their order of first appearance, so that areas' `option_uuid`s are
/// compared with the options they point to.
fn number_uuids(value: &Value, numbers: &mut HashMap<String, usize>) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), number_uuids(v, numbers)))
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| number_uuids(v, numbers)).collect())
        }
        Value::String(s) if uuid::Uuid::try_parse(s).is_ok() => {
            let next = numbers.len();
            Value::String(format!("uuid {}", numbers.entry(s.clone()).or_insert(next)))
        }
        other => other.clone(),
    }
}

/// The JSON as Rails stores it (`to_json`), key order and float digits included.
fn stored(value: &Value) -> String {
    loco_rails_compat::json::generate(&number_uuids(value, &mut HashMap::new()))
}

/// `upload-golden.json` holds what Rails' `Templates::FindPdfiumAcroFields` and
/// `Templates::BuildPdfiumAnnotations` return for the fixtures, as stored JSON with
/// numbered uuids (`scripts/generate-golden.sh`).
#[test]
fn extracts_the_fields_and_links_rails_does() {
    let golden: serde_json::Map<String, Value> =
        serde_json::from_slice(&fixture("upload-golden.json")).unwrap();
    assert_eq!(golden.len(), 5);
    for (name, expected) in &golden {
        let bytes = fixture(name);
        let processed = loco_pdf::with_shared(|pdfium| process(pdfium, &bytes, None, "doc", true))
            .expect("run rust/scripts/fetch-pdfium.sh")
            .unwrap();
        let fields = Value::Array(processed.fields.unwrap());
        assert_eq!(stored(&fields), expected["fields"], "{name}: fields");
        let annotations = Value::Array(processed.annotations);
        assert_eq!(
            stored(&annotations),
            expected["annotations"],
            "{name}: annotations"
        );
    }
}
