//! Field detection against Rails' output for the same files (`detect-golden.json`, from
//! `rust/scripts/generate-golden.sh`): the pages of a form PDF, and two image previews
//! (one reduced with an integer shrink first). Needs pdfium and the model
//! (`rust/scripts/fetch-pdfium.sh`, `rust/scripts/fetch-onnx.sh`).
//!
//! ONNX Runtime picks its kernels by CPU, so on other machines the model's outputs can
//! differ from Rails' in the last bits: coordinates are compared to 1e-4 here. The
//! parity check (`scripts/parity/detect_fields.py`) compares them exactly.

use std::path::{Path, PathBuf};

use loco_detect::{Model, PageNodes, image};
use serde_json::Value;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn model() -> &'static Model {
    let app_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    Model::shared(&app_root).expect("run rust/scripts/fetch-onnx.sh")
}

fn golden(name: &str) -> Vec<Value> {
    let data = std::fs::read(fixtures().join("detect-golden.json")).unwrap();
    let golden: Value = serde_json::from_slice(&data).unwrap();
    golden[name].as_array().unwrap().clone()
}

/// The fields without their random uuids.
fn without_uuids(fields: Vec<Value>) -> Vec<Value> {
    fields
        .into_iter()
        .map(|mut field| {
            field.as_object_mut().unwrap().remove("uuid");
            field
        })
        .collect()
}

fn assert_fields(label: &str, actual: &[Value], expected: &[Value]) {
    assert_eq!(actual.len(), expected.len(), "{label}: number of fields");
    for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
        let (mut a, mut e) = (a.clone(), e.clone());
        for key in ["x", "y", "w", "h"] {
            let (av, ev) = (
                a["areas"][0][key].as_f64().unwrap(),
                e["areas"][0][key].as_f64().unwrap(),
            );
            assert!(
                (av - ev).abs() < 1e-4,
                "{label}: field {i} {key}: {av} != {ev}"
            );
            a["areas"][0][key] = Value::Null;
            e["areas"][0][key] = Value::Null;
        }
        assert_eq!(a, e, "{label}: field {i}");
    }
}

#[test]
fn pdf_pages_match_rails() {
    let model = model();
    let data = std::fs::read(fixtures().join("detect-form.pdf")).unwrap();
    let expected = golden("detect-form.pdf");
    let mut nodes = PageNodes::default();
    for page in &expected {
        let index = page["page"].as_i64().unwrap();
        let rendered = loco_pdf::with_shared(|pdfium| {
            loco_pdf::pages::detect_page(pdfium, &data, index, loco_detect::render_size(model))
        })
        .expect("run rust/scripts/fetch-pdfium.sh")
        .unwrap();
        let fields =
            loco_detect::detect_pdf_page(model, &mut nodes, &rendered, index, None).unwrap();
        assert_fields(
            &format!("page {index}"),
            &without_uuids(fields),
            page["fields"].as_array().unwrap(),
        );
    }
}

#[test]
fn image_previews_match_rails() {
    let model = model();
    for name in ["detect-image.png", "detect-tall.png"] {
        let data = std::fs::read(fixtures().join(name)).unwrap();
        let preview = image::load_png(&data).unwrap();
        let fields = loco_detect::detect_image(model, &preview, None).unwrap();
        let expected = golden(name);
        assert_fields(
            name,
            &without_uuids(fields),
            expected[0]["fields"].as_array().unwrap(),
        );
    }
}
