//! `loco_pdf::image_page` against PDFs that `Templates::BuildImagePagePdf` wrote for the
//! same PNGs (libvips palette and RGB PNGs, `tests/fixtures/image_page-*`).

use std::path::Path;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

#[test]
fn writes_palette_pages_like_rails() {
    let pdf = loco_pdf::image_page::build(&fixture("image_page-palette.png"), 612.0, 792.0, None)
        .unwrap();
    assert_eq!(pdf, fixture("image_page-palette.pdf"));
}

#[test]
fn writes_rgb_pages_with_an_image_box_like_rails() {
    let pdf = loco_pdf::image_page::build(
        &fixture("image_page-rgb.png"),
        595.275_590_551_181_2,
        841.889_763_779_527_5,
        Some([10.5, 20.123_456, 300.0, 200.0]),
    )
    .unwrap();
    assert_eq!(pdf, fixture("image_page-rgb-box.pdf"));
}
