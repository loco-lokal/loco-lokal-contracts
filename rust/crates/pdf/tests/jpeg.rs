//! `loco_pdf::jpeg` against JPEG files libvips wrote (`write_to_buffer('.jpg', interlace:
//! true, Q: 35)`, libjpeg-turbo 3) from the PNG next to each.

use std::path::Path;

fn check(name: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let image = image::open(dir.join(format!("{name}.png")))
        .unwrap()
        .to_rgba8();
    let expected = std::fs::read(dir.join(format!("{name}.jpg"))).unwrap();
    let (width, height) = (image.width() as usize, image.height() as usize);

    let actual = loco_pdf::jpeg::encode_rgba(image.as_raw(), width, height, 35);

    assert!(
        actual == expected,
        "{name}: {} bytes, libvips wrote {}",
        actual.len(),
        expected.len()
    );
}

#[test]
fn matches_libvips_with_dummy_blocks() {
    check("noise-37x23");
}

#[test]
fn matches_libvips_with_odd_height() {
    check("noise-200x151");
}

#[test]
fn matches_libvips_for_text() {
    check("text-1400x90");
}
