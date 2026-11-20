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

fn check_baseline(name: &str, gray: bool, quality: u32) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let (samples, width, height, bands, expected) = if gray {
        let image = image::open(dir.join(format!("baseline-{name}-gray.png")))
            .unwrap()
            .to_luma8();
        let expected = dir.join(format!("baseline-{name}-gray-q{quality}.jpg"));
        (
            image.as_raw().clone(),
            image.width(),
            image.height(),
            1,
            expected,
        )
    } else {
        let image = image::open(dir.join(format!("{name}.png")))
            .unwrap()
            .to_rgba8();
        let rgb: Vec<u8> = image
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        let expected = dir.join(format!("baseline-{name}-q{quality}.jpg"));
        (rgb, image.width(), image.height(), 3, expected)
    };
    let expected = std::fs::read(expected).unwrap();

    let actual =
        loco_pdf::jpeg::encode_baseline(&samples, width as usize, height as usize, bands, quality);

    assert!(
        actual == expected,
        "{name} (gray {gray}, Q {quality}): {} bytes, libvips wrote {}",
        actual.len(),
        expected.len()
    );
}

/// `write_to_buffer('.jpg', Q:, strip: true)`, as `Templates::ModifyDocuments` writes
/// redacted and rotated images.
#[test]
fn matches_libvips_baseline() {
    for name in ["noise-37x23", "noise-200x151"] {
        check_baseline(name, false, 50);
        check_baseline(name, false, 90);
        check_baseline(name, true, 50);
    }
}
