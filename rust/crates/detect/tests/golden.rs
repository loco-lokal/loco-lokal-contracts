//! Field detection against Rails' output for the same files (`detect-golden.json`, from
//! `rust/scripts/generate-golden.sh`): the pages of a form PDF, and two image previews
//! (the tall one is reduced with an integer shrink first). Needs pdfium and the model
//! (`rust/scripts/fetch-pdfium.sh`, `rust/scripts/fetch-onnx.sh`).
//!
//! ONNX Runtime picks its int8 kernels by CPU (AVX2, AVX-512, VNNI), so on another
//! machine the model's boxes move by a few pixels and its scores by up to about 0.02.
//! Each box must overlap Rails' by at least [`MIN_IOU`] of their union (on an AVX2-only CPU
//! the lowest is 0.97); everything else, including the number, order and types of the
//! fields, must match exactly. The fixtures' detections score well clear of the
//! confidence thresholds, so the drift doesn't change which fields are kept. The parity
//! check (`scripts/parity/detect_fields.py`) compares the coordinates exactly.

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

/// The lowest intersection over union of a field's box with Rails'.
const MIN_IOU: f64 = 0.95;

/// The `[x, y, w, h]` of a field's area, which are then left out of it.
fn take_box(field: &mut Value) -> [f64; 4] {
    ["x", "y", "w", "h"].map(|key| field["areas"][0][key].take().as_f64().unwrap())
}

fn iou([ax, ay, aw, ah]: [f64; 4], [bx, by, bw, bh]: [f64; 4]) -> f64 {
    let iw = ((ax + aw).min(bx + bw) - ax.max(bx)).max(0.0);
    let ih = ((ay + ah).min(by + bh) - ay.max(by)).max(0.0);
    let inter = iw * ih;
    inter / (aw * ah + bw * bh - inter)
}

fn assert_fields(label: &str, actual: &[Value], expected: &[Value]) {
    assert_eq!(actual.len(), expected.len(), "{label}: number of fields");
    for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
        let (mut a, mut e) = (a.clone(), e.clone());
        let (ab, eb) = (take_box(&mut a), take_box(&mut e));
        let iou = iou(ab, eb);
        assert!(
            iou >= MIN_IOU,
            "{label}: field {i}: box {ab:?} != {eb:?} (IoU {iou})"
        );
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
