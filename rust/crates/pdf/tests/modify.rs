//! `loco_pdf::modify` on the upload fixtures: pages imported as Rails' `PDFium` build
//! imports them (`FPDF_ImportAcroForm`), then turned and redacted.

use std::path::Path;

use lopdf::{Document, Object};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

fn build(
    sources: &[&[u8]],
    runs: &[(usize, &str, i32)],
    ops: &[Option<loco_pdf::modify::PageOps>],
) -> Document {
    let pdfium = loco_pdf::load_pdfium(&loco_pdf::default_library_dir())
        .expect("run rust/scripts/fetch-pdfium.sh");
    let runs: Vec<loco_pdf::modify::Run> = runs
        .iter()
        .map(|&(source, pages, length)| loco_pdf::modify::Run {
            source,
            pages: pages.into(),
            length,
        })
        .collect();
    let data = loco_pdf::modify::build(&pdfium, sources, &runs, ops, &|_, _| Vec::new()).unwrap();
    Document::load_mem(&data).unwrap()
}

/// The `/T` of each entry of `/AcroForm /Fields`, `None` without a form.
fn field_names(doc: &Document) -> Option<Vec<String>> {
    let form = doc.catalog().ok()?.get(b"AcroForm").ok()?;
    let form = doc.dereference(form).ok()?.1.as_dict().ok()?;
    let fields = doc
        .dereference(form.get(b"Fields").ok()?)
        .ok()?
        .1
        .as_array()
        .ok()?;
    Some(
        fields
            .iter()
            .map(|f| {
                let field = doc.dereference(f).unwrap().1.as_dict().unwrap();
                String::from_utf8_lossy(
                    field.get(b"T").and_then(Object::as_str).unwrap_or_default(),
                )
                .into_owned()
            })
            .collect(),
    )
}

#[test]
fn registers_imported_fields_in_page_order() {
    let form = fixture("upload-form.pdf");
    let doc = build(&[&form], &[(0, "2,1", 2)], &[None, None]);
    let names = field_names(&doc).unwrap();
    assert_eq!(names.first().map(String::as_str), Some("Notes 2"));
    assert_eq!(names.len(), 13);
}

#[test]
fn copies_parents_and_renames_taken_names() {
    let acro = fixture("upload-acro-fields.pdf");
    // "Repeated" has widgets on pages 1 and 2; importing them separately copies the field twice.
    let doc = build(&[&acro], &[(0, "1", 1), (0, "2", 1)], &[None, None]);
    let names = field_names(&doc).unwrap();
    assert!(names.contains(&"Duplicate_1".to_owned()), "{names:?}");
    assert!(
        names.contains(&"Repeated".to_owned()) && names.contains(&"Repeated_1".to_owned()),
        "{names:?}"
    );
    // Every widget's parent exists.
    for page in doc.get_pages().into_values() {
        let annots = doc
            .get_dictionary(page)
            .unwrap()
            .get(b"Annots")
            .ok()
            .cloned();
        let Some(annots) = annots.and_then(|a| {
            doc.dereference(&a)
                .ok()
                .and_then(|(_, a)| a.as_array().ok().cloned())
        }) else {
            continue;
        };
        for annot in annots {
            let annot = doc.dereference(&annot).unwrap().1.as_dict().unwrap();
            if let Ok(parent) = annot.get(b"Parent") {
                assert!(doc.get_object(parent.as_reference().unwrap()).is_ok());
            }
        }
    }
}

#[test]
fn leaves_documents_without_widgets_without_a_form() {
    let links = fixture("upload-many-links.pdf");
    let doc = build(&[&links], &[(0, "2,1", 2)], &[None, None]);
    assert_eq!(field_names(&doc), None);
    assert_eq!(doc.get_pages().len(), 2);
}

#[test]
fn flattens_and_drops_redacted_fields() {
    let form = fixture("upload-form.pdf");
    let redact = loco_pdf::modify::PageOps {
        rotate: 90,
        redact: vec![loco_pdf::modify::Redaction {
            x: 0.0,
            y: 0.0,
            w: 0.5,
            h: 0.2,
            white: false,
        }],
    };
    let doc = build(&[&form], &[(0, "1,2", 2)], &[Some(redact), None]);
    assert_eq!(field_names(&doc).unwrap(), ["Notes 2"]);
    let first = doc.get_pages()[&1];
    let page = doc.get_dictionary(first).unwrap();
    assert!(page.get(b"Annots").map_or(true, |a| {
        doc.dereference(a).unwrap().1.as_array().unwrap().is_empty()
    }));
    // The turn is baked in: landscape, without /Rotate.
    let media_box = page.get(b"MediaBox").unwrap().as_array().unwrap();
    let width = media_box[2].as_float().unwrap() - media_box[0].as_float().unwrap();
    let height = media_box[3].as_float().unwrap() - media_box[1].as_float().unwrap();
    assert!(width > height);
}
