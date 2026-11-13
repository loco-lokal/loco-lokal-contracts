//! Editing existing PDFs the way `Submissions::GenerateResultAttachments` does with
//! HexaPDF: page overlays with text and images, rotation and annotation flattening, link
//! annotations, document info, and saving as an incremental update (optionally signed).

pub mod canvas;
mod colors;
pub mod compose;
pub mod fonts;
mod glyph_names;
pub mod layout;
pub mod sign;
mod writer;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat};

pub use canvas::{Canvas, parse_color};
pub use lopdf;

/// The `Producer` HexaPDF writes into every document it saves. The Rails backend's
/// documents carry it, so the ported output keeps it until cutover.
pub const PRODUCER: &str = "HexaPDF version 1.7.0";
use fonts::{FontData, Glyph, UsedFont};
use layout::Fitted;

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error(transparent)]
    Pdf(#[from] lopdf::Error),
    #[error("font: {0}")]
    Font(String),
    #[error("image: {0}")]
    Image(String),
    #[error("{0} is not supported yet")]
    Unsupported(&'static str),
    #[error("invalid PDF: {0}")]
    Invalid(String),
    #[error("signing: {0}")]
    Sign(String),
}

/// An image registered in the document.
#[derive(Clone, Copy, Debug)]
pub struct ImageInfo {
    pub index: usize,
    pub width: u32,
    pub height: u32,
}

struct ImageEntry {
    id: ObjectId,
}

#[derive(Default)]
struct PageUse {
    fonts: BTreeSet<usize>,
    images: BTreeSet<usize>,
    forms: Vec<(String, ObjectId)>,
}

/// A PDF being edited.
pub struct Editor {
    original: Vec<u8>,
    pub doc: Document,
    dirty: BTreeSet<ObjectId>,
    fonts: Vec<UsedFont>,
    font_index: HashMap<String, usize>,
    images: Vec<ImageEntry>,
    image_keys: HashMap<String, ImageInfo>,
    overlays: BTreeMap<ObjectId, Canvas>,
    uses: BTreeMap<ObjectId, PageUse>,
    /// Problems that were skipped, as `maybe_flatten_pdf` logs and continues.
    pub warnings: Vec<String>,
}

/// A PDF date string (`D:YYYYMMDDHHmmSS+HH'mm'`).
#[must_use]
pub fn pdf_date(date: &str) -> Object {
    Object::String(date.as_bytes().to_vec(), StringFormat::Literal)
}

impl Editor {
    pub fn load(bytes: Vec<u8>) -> Result<Self, EditError> {
        let doc = Document::load_mem(&bytes)?;
        if doc.trailer.has(b"Encrypt") {
            return Err(EditError::Unsupported("editing encrypted PDFs"));
        }
        Ok(Self {
            original: bytes,
            doc,
            dirty: BTreeSet::new(),
            fonts: Vec::new(),
            font_index: HashMap::new(),
            images: Vec::new(),
            image_keys: HashMap::new(),
            overlays: BTreeMap::new(),
            uses: BTreeMap::new(),
            warnings: Vec::new(),
        })
    }

    /// `build_pdf_from_image`: one page the size of the image scaled into A4, showing it.
    pub fn from_image(
        image: &image::DynamicImage,
        preview_width: f64,
        preview_height: f64,
    ) -> Result<Self, EditError> {
        let scale = (595.0 / preview_width).min(842.0 / preview_height);
        let (w, h) = (preview_width * scale, preview_height * scale);
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let mut page = Dictionary::new();
        page.set("Type", Object::Name(b"Page".to_vec()));
        page.set("Parent", Object::Reference(pages_id));
        page.set(
            "MediaBox",
            Object::Array(vec![0.into(), 0.into(), real(w), real(h)]),
        );
        let first_page = doc.add_object(page);
        let mut pages = Dictionary::new();
        pages.set("Type", Object::Name(b"Pages".to_vec()));
        pages.set("Kids", Object::Array(vec![Object::Reference(first_page)]));
        pages.set("Count", Object::Integer(1));
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let mut catalog = Dictionary::new();
        catalog.set("Type", Object::Name(b"Catalog".to_vec()));
        catalog.set("Pages", Object::Reference(pages_id));
        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", Object::Reference(catalog_id));
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes)
            .map_err(|e| EditError::Invalid(e.to_string()))?;
        let mut editor = Self::load(bytes)?;
        let info = editor.add_image(image, false)?;
        let page = editor.page_ids()[0];
        let name = editor.use_image(page, info.index);
        editor.overlay(page).image(&name, 0.0, 0.0, w, h);
        Ok(editor)
    }

    /// A new document with an empty page tree (`HexaPDF::Document.new`), to add pages to.
    pub fn blank() -> Result<Self, EditError> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut pages = Dictionary::new();
        pages.set("Type", Object::Name(b"Pages".to_vec()));
        pages.set("Kids", Object::Array(Vec::new()));
        pages.set("Count", Object::Integer(0));
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let mut catalog = Dictionary::new();
        catalog.set("Type", Object::Name(b"Catalog".to_vec()));
        catalog.set("Pages", Object::Reference(pages_id));
        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", Object::Reference(catalog_id));
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes)
            .map_err(|e| EditError::Invalid(e.to_string()))?;
        Self::load(bytes)
    }

    /// Appends an empty page with the media box `[0 0 width height]`.
    pub fn add_page(&mut self, width: f64, height: f64) -> Result<ObjectId, EditError> {
        let pages_id = self.pages_root()?;
        let mut page = Dictionary::new();
        page.set("Type", Object::Name(b"Page".to_vec()));
        page.set("Parent", Object::Reference(pages_id));
        page.set(
            "MediaBox",
            Object::Array(vec![0.into(), 0.into(), real(width), real(height)]),
        );
        page.set("Resources", Object::Dictionary(Dictionary::new()));
        page.set("Contents", Object::Array(Vec::new()));
        let id = self.add(Object::Dictionary(page));
        if let Some(pages) = self.dict_mut(pages_id) {
            let mut kids = pages
                .get(b"Kids")
                .ok()
                .and_then(|k| k.as_array().ok())
                .cloned()
                .unwrap_or_default();
            kids.push(Object::Reference(id));
            pages.set(
                "Count",
                Object::Integer(i64::try_from(kids.len()).unwrap_or(0)),
            );
            pages.set("Kids", Object::Array(kids));
        }
        Ok(id)
    }

    /// The document as edited so far, with fonts, overlays and resources written out, to
    /// import pages from (`pdf.dispatch_message(:complete_objects)`).
    pub fn into_document(mut self) -> Result<Document, EditError> {
        self.finish()?;
        Ok(self.doc)
    }

    /// `source.pages.each { |page| pages << import(page) }`: appends deep copies of the
    /// source's pages with everything they reference, as `HexaPDF::Importer` does. The
    /// catalog and page tree nodes are not copied (references to them become `null`), so
    /// form fields and signatures of the source are left behind, while their widgets
    /// stay on the pages. Inherited page attributes are copied onto each page.
    pub fn import_pages(&mut self, source: &Document) -> Result<(), EditError> {
        let pages_id = self.pages_root()?;
        let mut mapping: HashMap<ObjectId, Option<ObjectId>> = HashMap::new();
        let mut imported = Vec::new();
        for page in source.get_pages().into_values() {
            let mut dict = source
                .get_object(page)
                .and_then(Object::as_dict)
                .map_err(|_| EditError::Invalid("page is not a dictionary".to_owned()))?
                .clone();
            for key in [&b"Resources"[..], b"MediaBox", b"CropBox", b"Rotate"] {
                if !dict.has(key)
                    && let Some(value) = inherited_in(source, &dict, key)
                {
                    dict.set(key, value);
                }
            }
            dict.remove(b"Parent");
            let id = self.next_id();
            mapping.insert(page, Some(id));
            let mut copied = self.duplicate(source, Object::Dictionary(dict), &mut mapping);
            if let Object::Dictionary(dict) = &mut copied {
                dict.set("Parent", Object::Reference(pages_id));
            }
            self.doc.objects.insert(id, copied);
            self.dirty.insert(id);
            imported.push(Object::Reference(id));
        }
        if let Some(pages) = self.dict_mut(pages_id) {
            let mut kids = pages
                .get(b"Kids")
                .ok()
                .and_then(|k| k.as_array().ok())
                .cloned()
                .unwrap_or_default();
            kids.extend(imported);
            pages.set(
                "Count",
                Object::Integer(i64::try_from(kids.len()).unwrap_or(0)),
            );
            pages.set("Kids", Object::Array(kids));
        }
        Ok(())
    }

    /// Copies `object` from `source`, importing the objects it references.
    fn duplicate(
        &mut self,
        source: &Document,
        object: Object,
        mapping: &mut HashMap<ObjectId, Option<ObjectId>>,
    ) -> Object {
        match object {
            Object::Reference(id) => self.import_object(source, id, mapping),
            Object::Array(items) => Object::Array(
                items
                    .into_iter()
                    .map(|item| self.duplicate(source, item, mapping))
                    .collect(),
            ),
            Object::Dictionary(dict) => {
                Object::Dictionary(self.duplicate_dict(source, dict, mapping))
            }
            Object::Stream(stream) => {
                let dict = self.duplicate_dict(source, stream.dict, mapping);
                Object::Stream(Stream { dict, ..stream })
            }
            other => other,
        }
    }

    fn duplicate_dict(
        &mut self,
        source: &Document,
        dict: Dictionary,
        mapping: &mut HashMap<ObjectId, Option<ObjectId>>,
    ) -> Dictionary {
        let mut out = Dictionary::new();
        for (key, value) in dict {
            let value = self.duplicate(source, value, mapping);
            out.set(key, value);
        }
        out
    }

    fn import_object(
        &mut self,
        source: &Document,
        id: ObjectId,
        mapping: &mut HashMap<ObjectId, Option<ObjectId>>,
    ) -> Object {
        if let Some(mapped) = mapping.get(&id) {
            return mapped.map_or(Object::Null, Object::Reference);
        }
        let Ok(object) = source.get_object(id) else {
            return Object::Null;
        };
        let is_tree_node = object
            .as_dict()
            .or_else(|_| object.as_stream().map(|s| &s.dict))
            .ok()
            .and_then(|d| d.get(b"Type").ok())
            .and_then(|t| t.as_name().ok())
            .is_some_and(|t| t == b"Catalog" || t == b"Pages");
        if is_tree_node {
            mapping.insert(id, None);
            return Object::Null;
        }
        let new_id = self.next_id();
        mapping.insert(id, Some(new_id));
        let copied = self.duplicate(source, object.clone(), mapping);
        self.doc.objects.insert(new_id, copied);
        self.dirty.insert(new_id);
        Object::Reference(new_id)
    }

    fn pages_root(&self) -> Result<ObjectId, EditError> {
        self.catalog()
            .and_then(|c| c.get(b"Pages").ok())
            .and_then(|p| p.as_reference().ok())
            .ok_or_else(|| EditError::Invalid("no page tree".to_owned()))
    }

    fn catalog(&self) -> Option<&Dictionary> {
        let root = self.doc.trailer.get(b"Root").ok()?.as_reference().ok()?;
        self.dict(root)
    }

    /// Adds an object to the document (written with the next save).
    pub fn add_object(&mut self, object: Object) -> ObjectId {
        self.add(object)
    }

    /// Sets an entry of the document catalog.
    pub fn set_catalog(&mut self, key: &str, value: Object) -> Result<(), EditError> {
        let root = self
            .doc
            .trailer
            .get(b"Root")
            .ok()
            .and_then(|r| r.as_reference().ok())
            .ok_or_else(|| EditError::Invalid("no catalog".to_owned()))?;
        if let Some(catalog) = self.dict_mut(root) {
            catalog.set(key, value);
        }
        Ok(())
    }

    fn next_id(&mut self) -> ObjectId {
        self.doc.max_id += 1;
        (self.doc.max_id, 0)
    }

    fn add(&mut self, object: Object) -> ObjectId {
        let id = self.next_id();
        self.doc.objects.insert(id, object);
        self.dirty.insert(id);
        id
    }

    fn resolve<'a>(&'a self, object: &'a Object) -> &'a Object {
        self.doc.dereference(object).map_or(object, |(_, o)| o)
    }

    fn dict(&self, id: ObjectId) -> Option<&Dictionary> {
        self.doc.get_object(id).ok().and_then(|o| o.as_dict().ok())
    }

    fn dict_mut(&mut self, id: ObjectId) -> Option<&mut Dictionary> {
        self.dirty.insert(id);
        self.doc
            .get_object_mut(id)
            .ok()
            .and_then(|o| o.as_dict_mut().ok())
    }

    /// Page object ids in order.
    #[must_use]
    pub fn page_ids(&self) -> Vec<ObjectId> {
        self.doc.get_pages().into_values().collect()
    }

    /// An attribute of a page, inherited from its ancestors.
    fn inherited(&self, page: ObjectId, key: &[u8]) -> Option<Object> {
        let mut current = self.dict(page)?;
        for _ in 0..64 {
            if let Ok(value) = current.get(key) {
                return Some(self.resolve(value).clone());
            }
            let parent = current.get(b"Parent").ok()?.as_reference().ok()?;
            current = self.dict(parent)?;
        }
        None
    }

    fn rect(&self, object: &Object) -> Option<[f64; 4]> {
        let array = self.resolve(object).as_array().ok()?;
        if array.len() != 4 {
            return None;
        }
        let mut out = [0.0; 4];
        for (i, value) in array.iter().enumerate() {
            out[i] = number(self.resolve(value))?;
        }
        Some([
            out[0].min(out[2]),
            out[1].min(out[3]),
            out[0].max(out[2]),
            out[1].max(out[3]),
        ])
    }

    /// `page.box`: the crop box clamped to the media box, as `[left, bottom, right, top]`.
    #[must_use]
    pub fn page_box(&self, page: ObjectId) -> [f64; 4] {
        let media = self
            .inherited(page, b"MediaBox")
            .and_then(|o| self.rect(&o))
            .unwrap_or([0.0, 0.0, 595.0, 842.0]);
        let Some(crop) = self.inherited(page, b"CropBox").and_then(|o| self.rect(&o)) else {
            return media;
        };
        let clamped = [
            crop[0].max(media[0]),
            crop[1].max(media[1]),
            crop[2].min(media[2]),
            crop[3].min(media[3]),
        ];
        if clamped[2] > clamped[0] && clamped[3] > clamped[1] {
            clamped
        } else {
            media
        }
    }

    fn rotation(&self, page: ObjectId) -> i64 {
        self.inherited(page, b"Rotate")
            .and_then(|o| o.as_i64().ok())
            .unwrap_or(0)
    }

    /// `maybe_rotate_pdf`: resets the page tree root's rotation and flattens each page's.
    pub fn rotate_pages(&mut self) {
        let pages = self.page_ids();
        if pages.len() > 50 {
            return;
        }
        let root = self
            .doc
            .catalog()
            .ok()
            .and_then(|c| c.get(b"Pages").ok())
            .and_then(|p| p.as_reference().ok());
        let root_rotate = root
            .and_then(|r| self.dict(r))
            .and_then(|d| d.get(b"Rotate").ok())
            .and_then(|o| self.resolve(o).as_i64().ok())
            .unwrap_or(0);
        // The page tree root's rotation is reset before the pages are read.
        let inherited: Vec<i64> = pages.iter().map(|p| self.rotation(*p)).collect();
        if root_rotate != 0
            && let Some(root) = root
            && let Some(dict) = self.dict_mut(root)
        {
            dict.set("Rotate", Object::Integer(0));
        }
        for (page, rotate) in pages.into_iter().zip(inherited) {
            let own = self
                .dict(page)
                .and_then(|d| d.get(b"Rotate").ok())
                .is_some();
            let effective = if own || root_rotate == 0 { rotate } else { 0 };
            if effective != 0 {
                self.rotate_page(page, effective);
            }
        }
    }

    /// `page.rotate(0, flatten: true)`.
    pub fn rotate_page_flat(&mut self, page: ObjectId) {
        let rotate = self.rotation(page);
        if rotate != 0 {
            self.rotate_page(page, rotate);
        }
    }

    fn rotate_page(&mut self, page: ObjectId, rotate: i64) {
        let cw = rotate.rem_euclid(360);
        let [left, bottom, right, top] = self.page_box(page);
        let matrix: [f64; 6] = match cw {
            90 => [0.0, -1.0, 1.0, 0.0, -bottom, right],
            180 => [-1.0, 0.0, 0.0, -1.0, right, top],
            270 => [0.0, 1.0, -1.0, 0.0, top, -left],
            _ => {
                if let Some(dict) = self.dict_mut(page) {
                    dict.remove(b"Rotate");
                }
                return;
            }
        };
        let eval = |x: f64, y: f64| {
            [
                matrix[0] * x + matrix[2] * y + matrix[4],
                matrix[1] * x + matrix[3] * y + matrix[5],
            ]
        };
        let rotate_box = |r: [f64; 4]| {
            let [x_min, y_min, x_max, y_max] = r;
            let (lower, upper) = match cw {
                90 => ((x_max, y_min), (x_min, y_max)),
                180 => ((x_max, y_max), (x_min, y_min)),
                _ => ((x_min, y_max), (x_max, y_min)),
            };
            let lower = eval(lower.0, lower.1);
            let upper = eval(upper.0, upper.1);
            Object::Array(vec![
                real(lower[0]),
                real(lower[1]),
                real(upper[0]),
                real(upper[1]),
            ])
        };
        // Inherited boxes become the page's own.
        let mut boxes = Vec::new();
        for name in ["MediaBox", "CropBox", "BleedBox", "TrimBox", "ArtBox"] {
            let value = if matches!(name, "MediaBox" | "CropBox") {
                self.inherited(page, name.as_bytes())
            } else {
                self.dict(page)
                    .and_then(|d| d.get(name.as_bytes()).ok())
                    .map(|o| self.resolve(o).clone())
            };
            if let Some(rect) = value.and_then(|v| self.rect(&v)) {
                boxes.push((name, rotate_box(rect)));
            }
        }
        for annot in self.annotation_ids(page) {
            let Some(rect) = self
                .dict(annot)
                .and_then(|d| d.get(b"Rect").ok())
                .and_then(|r| self.rect(r))
            else {
                continue;
            };
            let appearance = self.appearance(annot);
            let quad = self
                .dict(annot)
                .and_then(|d| d.get(b"QuadPoints").ok())
                .and_then(|q| self.resolve(q).as_array().ok())
                .map(|q| q.iter().filter_map(number).collect::<Vec<_>>());
            let is_widget = self.is_subtype(annot, b"Widget");
            if let Some(dict) = self.dict_mut(annot) {
                dict.set("Rect", rotate_box(rect));
                if let Some(quad) = quad {
                    let rotated = quad
                        .chunks(2)
                        .filter(|p| p.len() == 2)
                        .flat_map(|p| eval(p[0], p[1]))
                        .map(real)
                        .collect();
                    dict.set("QuadPoints", Object::Array(rotated));
                }
                if is_widget {
                    let mut mk = match dict.get(b"MK") {
                        Ok(Object::Dictionary(mk)) => mk.clone(),
                        _ => Dictionary::new(),
                    };
                    let r = mk.get(b"R").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0);
                    mk.set("R", Object::Integer((r + 360 - cw).rem_euclid(360)));
                    dict.set("MK", Object::Dictionary(mk));
                }
            }
            if let Some(form) = appearance {
                let current = self.form_matrix(form);
                let combined = multiply(current, matrix);
                if let Ok(Object::Stream(stream)) = self.doc.get_object_mut(form) {
                    stream.dict.set(
                        "Matrix",
                        Object::Array(combined.iter().map(|v| real(*v)).collect()),
                    );
                    self.dirty.insert(form);
                }
            }
        }
        let m: Vec<String> = matrix.iter().map(|v| writer::real(*v)).collect();
        let before = self.add(Object::Stream(Stream::new(
            Dictionary::new(),
            format!(" q {} cm ", m.join(" ")).into_bytes(),
        )));
        let after = self.add(Object::Stream(Stream::new(
            Dictionary::new(),
            b" Q ".to_vec(),
        )));
        let mut contents = self.contents(page);
        contents.insert(0, Object::Reference(before));
        contents.push(Object::Reference(after));
        if let Some(dict) = self.dict_mut(page) {
            dict.remove(b"Rotate");
            for (name, value) in boxes {
                dict.set(name, value);
            }
            dict.set("Contents", Object::Array(contents));
        }
    }

    /// The page's content streams as references.
    fn contents(&self, page: ObjectId) -> Vec<Object> {
        let Some(value) = self.dict(page).and_then(|d| d.get(b"Contents").ok()) else {
            return Vec::new();
        };
        match value {
            Object::Reference(id) => match self.doc.get_object(*id) {
                Ok(Object::Array(items)) => items.clone(),
                _ => vec![value.clone()],
            },
            Object::Array(items) => items.clone(),
            _ => Vec::new(),
        }
    }

    fn annotation_ids(&self, page: ObjectId) -> Vec<ObjectId> {
        self.dict(page)
            .and_then(|d| d.get(b"Annots").ok())
            .and_then(|a| self.resolve(a).as_array().ok())
            .map(|items| items.iter().filter_map(|i| i.as_reference().ok()).collect())
            .unwrap_or_default()
    }

    fn set_annotations(&mut self, page: ObjectId, annots: Vec<ObjectId>) {
        let array = Object::Array(annots.into_iter().map(Object::Reference).collect());
        if let Some(dict) = self.dict_mut(page) {
            dict.set("Annots", array);
        }
    }

    fn is_subtype(&self, id: ObjectId, subtype: &[u8]) -> bool {
        self.dict(id)
            .and_then(|d| d.get(b"Subtype").ok())
            .and_then(|s| self.resolve(s).as_name().ok())
            .is_some_and(|s| s == subtype)
    }

    /// `annotation.appearance`: the normal appearance form for the current state.
    fn appearance(&self, annot: ObjectId) -> Option<ObjectId> {
        let dict = self.dict(annot)?;
        let ap = self.resolve(dict.get(b"AP").ok()?).as_dict().ok()?;
        let normal = ap.get(b"N").ok()?;
        let id = match self.resolve(normal) {
            Object::Dictionary(states) => {
                let state = dict
                    .get(b"AS")
                    .ok()
                    .and_then(|s| self.resolve(s).as_name().ok())?;
                states.get(state).ok()?.as_reference().ok()?
            }
            Object::Stream(_) => normal.as_reference().ok()?,
            _ => return None,
        };
        let Ok(Object::Stream(stream)) = self.doc.get_object(id) else {
            return None;
        };
        let type_ok = stream
            .dict
            .get(b"Type")
            .ok()
            .and_then(|t| t.as_name().ok())
            .is_none_or(|t| t == b"XObject");
        let subtype_ok = stream
            .dict
            .get(b"Subtype")
            .ok()
            .and_then(|t| t.as_name().ok())
            .is_none_or(|t| t == b"Form");
        (type_ok && subtype_ok && stream.dict.has(b"BBox")).then_some(id)
    }

    fn form_matrix(&self, form: ObjectId) -> [f64; 6] {
        let Ok(Object::Stream(stream)) = self.doc.get_object(form) else {
            return [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
        };
        let values: Vec<f64> = stream
            .dict
            .get(b"Matrix")
            .ok()
            .and_then(|m| self.resolve(m).as_array().ok())
            .map(|m| m.iter().filter_map(|v| number(self.resolve(v))).collect())
            .unwrap_or_default();
        values.try_into().unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0])
    }

    /// Removes the `file:///docuseal_field` link annotations templates placed on a page.
    pub fn remove_field_links(&mut self, page: ObjectId) {
        let annots = self.annotation_ids(page);
        let kept: Vec<ObjectId> = annots
            .iter()
            .copied()
            .filter(|id| {
                let uri = self
                    .dict(*id)
                    .and_then(|d| d.get(b"A").ok())
                    .and_then(|a| self.resolve(a).as_dict().ok())
                    .and_then(|a| a.get(b"URI").ok())
                    .and_then(|u| self.resolve(u).as_str().ok());
                !uri.is_some_and(|u| u.starts_with(b"file:///docuseal_field"))
            })
            .collect();
        let has_annots = self.dict(page).is_some_and(|d| d.has(b"Annots"));
        if kept.len() != annots.len() || !has_annots {
            self.set_annotations(page, kept);
        }
    }

    /// `page.flatten_annotations`: draws annotations with appearances into the page and
    /// removes them (and hidden ones). With `only`, just those annotations.
    pub fn flatten_annotations(
        &mut self,
        page: ObjectId,
        only: Option<&BTreeSet<ObjectId>>,
    ) -> BTreeSet<ObjectId> {
        let annots = self.annotation_ids(page);
        let mut flattened = BTreeSet::new();
        let page_box = self.page_box(page);
        for annot in annots.iter().copied() {
            if only.is_some_and(|only| !only.contains(&annot)) {
                continue;
            }
            let flags = self
                .dict(annot)
                .and_then(|d| d.get(b"F").ok())
                .and_then(|f| self.resolve(f).as_i64().ok())
                .unwrap_or(0);
            if flags & 0b11 != 0 {
                flattened.insert(annot);
                continue;
            }
            let Some(form) = self.appearance(annot) else {
                continue;
            };
            let Some(rect) = self
                .dict(annot)
                .and_then(|d| d.get(b"Rect").ok())
                .and_then(|r| self.rect(r))
            else {
                continue;
            };
            let bbox = match self.doc.get_object(form) {
                Ok(Object::Stream(stream)) => {
                    stream.dict.get(b"BBox").ok().and_then(|b| self.rect(b))
                }
                _ => None,
            };
            let Some(bbox) = bbox else { continue };
            let m = self.form_matrix(form);
            let eval = |x: f64, y: f64| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]);
            let (llx, lly) = eval(bbox[0], bbox[1]);
            let (ulx, uly) = eval(bbox[0], bbox[3]);
            let (lrx, lry) = eval(bbox[2], bbox[1]);
            let xs = [llx, ulx, lrx, lrx + (ulx - llx)];
            let ys = [lly, uly, lry, lry + (uly - lly)];
            let (left, right) = (
                xs.iter().copied().fold(f64::MAX, f64::min),
                xs.iter().copied().fold(f64::MIN, f64::max),
            );
            let (bottom, top) = (
                ys.iter().copied().fold(f64::MAX, f64::min),
                ys.iter().copied().fold(f64::MIN, f64::max),
            );
            flattened.insert(annot);
            if right - left == 0.0 || top - bottom == 0.0 {
                continue;
            }
            let sx = (rect[2] - rect[0]) / (right - left);
            let sy = (rect[3] - rect[1]) / (top - bottom);
            let tx = rect[0] - left + left - left * sx;
            let ty = rect[1] - bottom + bottom - bottom * sy;
            // `xobject(at: [box.left, box.bottom])` draws a form at its natural place, under
            // the flattening transform; the overlay's origin is the page box's corner.
            let combined = multiply(
                [sx, 0.0, 0.0, sy, tx, ty],
                [1.0, 0.0, 0.0, 1.0, -page_box[0], -page_box[1]],
            );
            let name = format!("DSA{}", form.0);
            self.uses
                .entry(page)
                .or_default()
                .forms
                .push((name.clone(), form));
            self.overlay(page).form(&name, combined);
        }
        if flattened.is_empty() {
            return flattened;
        }
        let kept: Vec<ObjectId> = annots
            .into_iter()
            .filter(|a| !flattened.contains(a))
            .collect();
        self.set_annotations(page, kept);
        for annot in &flattened {
            self.detach_widget(*annot);
        }
        flattened
    }

    /// `form_field.delete_widget(widget)`: unlinks a widget from its field.
    fn detach_widget(&mut self, widget: ObjectId) {
        if !self.is_subtype(widget, b"Widget") {
            return;
        }
        let parent = self
            .dict(widget)
            .and_then(|d| d.get(b"Parent").ok())
            .and_then(|p| p.as_reference().ok());
        let is_field = self
            .dict(widget)
            .is_some_and(|d| d.has(b"T") || d.has(b"FT"));
        if let Some(parent) = parent
            && !is_field
        {
            let kids: Vec<Object> = self
                .dict(parent)
                .and_then(|d| d.get(b"Kids").ok())
                .and_then(|k| self.resolve(k).as_array().ok())
                .map(|k| {
                    k.iter()
                        .filter(|o| o.as_reference().ok() != Some(widget))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if let Some(dict) = self.dict_mut(parent) {
                dict.set("Kids", Object::Array(kids));
            }
        } else {
            self.remove_field(widget, parent);
        }
    }

    fn acro_form(&self) -> Option<ObjectId> {
        match self.doc.catalog().ok()?.get(b"AcroForm").ok()? {
            Object::Reference(id) => Some(*id),
            _ => None,
        }
    }

    /// Ensures the AcroForm dictionary is an indirect object, and returns it.
    fn acro_form_id(&mut self) -> Result<ObjectId, EditError> {
        if let Some(id) = self.acro_form() {
            return Ok(id);
        }
        let root = self.doc.trailer.get(b"Root")?.as_reference()?;
        let existing = match self.dict(root).and_then(|c| c.get(b"AcroForm").ok()) {
            Some(Object::Dictionary(form)) => form.clone(),
            _ => Dictionary::new(),
        };
        let id = self.add(Object::Dictionary(existing));
        if let Some(catalog) = self.dict_mut(root) {
            catalog.set("AcroForm", Object::Reference(id));
        }
        Ok(id)
    }

    fn remove_field(&mut self, field: ObjectId, parent: Option<ObjectId>) {
        let holder = parent.or_else(|| self.acro_form());
        let key: &[u8] = if parent.is_some() { b"Kids" } else { b"Fields" };
        let Some(holder) = holder else { return };
        let items: Vec<Object> = self
            .dict(holder)
            .and_then(|d| d.get(key).ok())
            .and_then(|k| self.resolve(k).as_array().ok())
            .map(|k| {
                k.iter()
                    .filter(|o| o.as_reference().ok() != Some(field))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if let Some(dict) = self.dict_mut(holder) {
            dict.set(key, Object::Array(items));
        }
    }

    /// `maybe_flatten_pdf`: flattens the interactive form's widgets that have appearances
    /// and removes the form when nothing is left.
    pub fn flatten_form(&mut self) {
        let Ok(root) = self.doc.trailer.get(b"Root").and_then(Object::as_reference) else {
            return;
        };
        if matches!(
            self.dict(root).and_then(|c| c.get(b"AcroForm").ok()),
            Some(Object::Integer(_))
        ) {
            if let Some(catalog) = self.dict_mut(root) {
                catalog.remove(b"AcroForm");
            }
            return;
        }
        let Some(form) = self
            .dict(root)
            .and_then(|c| c.get(b"AcroForm").ok())
            .and_then(|f| self.resolve(f).as_dict().ok())
            .cloned()
        else {
            return;
        };
        if form
            .get(b"NeedAppearances")
            .ok()
            .and_then(|v| v.as_bool().ok())
            == Some(true)
        {
            self.warnings.push(
                "form fields without appearances (NeedAppearances) were left unflattened"
                    .to_owned(),
            );
        }
        let mut widgets = BTreeSet::new();
        let mut stack: Vec<ObjectId> = form
            .get(b"Fields")
            .ok()
            .and_then(|f| self.resolve(f).as_array().ok())
            .map(|f| f.iter().filter_map(|o| o.as_reference().ok()).collect())
            .unwrap_or_default();
        let mut seen = BTreeSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let kids: Vec<ObjectId> = self
                .dict(id)
                .and_then(|d| d.get(b"Kids").ok())
                .and_then(|k| self.resolve(k).as_array().ok())
                .map(|k| k.iter().filter_map(|o| o.as_reference().ok()).collect())
                .unwrap_or_default();
            if kids.is_empty() {
                if self.is_subtype(id, b"Widget") {
                    widgets.insert(id);
                }
            } else {
                stack.extend(kids);
            }
        }
        let mut flattened = BTreeSet::new();
        for page in self.page_ids() {
            flattened.extend(self.flatten_annotations(page, Some(&widgets)));
        }
        if widgets.is_subset(&flattened)
            && let Some(catalog) = self.dict_mut(root)
        {
            catalog.remove(b"AcroForm");
        }
    }

    /// The page's overlay canvas (`page.canvas(type: :overlay)`), in page box coordinates.
    pub fn overlay(&mut self, page: ObjectId) -> &mut Canvas {
        let origin = self.page_box(page);
        self.overlays.entry(page).or_insert_with(|| {
            let mut canvas = Canvas::default();
            if origin[0] != 0.0 || origin[1] != 0.0 {
                canvas.ops.extend_from_slice(
                    format!(
                        "1 0 0 1 {} {} cm\n",
                        writer::real(origin[0]),
                        writer::real(origin[1])
                    )
                    .as_bytes(),
                );
            }
            canvas
        })
    }

    /// Registers a font (`pdf.fonts.add`) and returns it with its resource key.
    pub fn font(
        &mut self,
        family: &str,
        variant: &str,
    ) -> Result<(Arc<FontData>, String), EditError> {
        let key = format!("{family}/{variant}");
        if let Some(&index) = self.font_index.get(&key) {
            let id = self.fonts[index].object_id;
            return Ok((self.fonts[index].font.clone(), font_name(id)));
        }
        let font = fonts::load(family, variant)?;
        let id = self.next_id();
        let index = self.fonts.len();
        self.fonts.push(UsedFont::new(font.clone(), id));
        self.font_index.insert(key, index);
        Ok((font, font_name(id)))
    }

    /// Draws laid-out text on a page's overlay, with link annotations for linked text.
    pub fn draw_text(&mut self, page: ObjectId, fitted: &Fitted, x: f64, y: f64) {
        self.overlay(page);
        let mut canvas = self.overlays.remove(&page).expect("overlay");
        let use_ = self.uses.entry(page).or_default();
        let mut resources = PageResources {
            fonts: &mut self.fonts,
            images: &self.images,
            used: use_,
        };
        let links = canvas.draw(fitted, x, y, &mut resources);
        self.overlays.insert(page, canvas);
        let origin = self.page_box(page);
        for link in links {
            let rect = link.rect;
            self.add_layer_link(
                page,
                [
                    rect[0] + origin[0],
                    rect[1] + origin[1],
                    rect[2] + origin[0],
                    rect[3] + origin[1],
                ],
                &link.uri,
            );
        }
    }

    /// `Style::LinkLayer` with a URI: a borderless link annotation with quad points.
    fn add_layer_link(&mut self, page: ObjectId, rect: [f64; 4], uri: &str) {
        let [l, b, r, t] = rect;
        let mut action = Dictionary::new();
        action.set("S", Object::Name(b"URI".to_vec()));
        action.set("URI", Object::string_literal(uri));
        let mut annot = Dictionary::new();
        annot.set("Type", Object::Name(b"Annot".to_vec()));
        annot.set("Subtype", Object::Name(b"Link".to_vec()));
        annot.set(
            "Rect",
            Object::Array(rect.iter().map(|v| real(*v)).collect()),
        );
        annot.set(
            "QuadPoints",
            Object::Array([l, b, r, b, r, t, l, t].iter().map(|v| real(*v)).collect()),
        );
        annot.set("A", Object::Dictionary(action));
        annot.set("Border", Object::Array(vec![0.into(), 0.into(), 0.into()]));
        let id = self.add(Object::Dictionary(annot));
        let mut annots = self.annotation_ids(page);
        annots.push(id);
        self.set_annotations(page, annots);
    }

    /// Marks an image as used on a page and returns its resource name.
    pub fn use_image(&mut self, page: ObjectId, index: usize) -> String {
        self.uses.entry(page).or_default().images.insert(index);
        image_name(self.images[index].id)
    }

    /// An image registered once under `key` (the check and paperclip icons).
    pub fn cached_image(
        &mut self,
        key: &str,
        load: impl FnOnce() -> Result<image::DynamicImage, EditError>,
    ) -> Result<ImageInfo, EditError> {
        if let Some(info) = self.image_keys.get(key) {
            return Ok(*info);
        }
        let info = self.add_image(&load()?, false)?;
        self.image_keys.insert(key.to_owned(), info);
        Ok(info)
    }

    /// Embeds an image: as a JPEG when `jpeg` (and it has no alpha), else losslessly with a
    /// soft mask for its alpha channel.
    pub fn add_image(
        &mut self,
        image: &image::DynamicImage,
        jpeg: bool,
    ) -> Result<ImageInfo, EditError> {
        let (width, height) = (image.width(), image.height());
        let mut dict = Dictionary::new();
        dict.set("Type", Object::Name(b"XObject".to_vec()));
        dict.set("Subtype", Object::Name(b"Image".to_vec()));
        dict.set("Width", Object::Integer(width.into()));
        dict.set("Height", Object::Integer(height.into()));
        dict.set("BitsPerComponent", Object::Integer(8));
        let gray = !image.color().has_color();
        dict.set(
            "ColorSpace",
            Object::Name(if gray {
                b"DeviceGray".to_vec()
            } else {
                b"DeviceRGB".to_vec()
            }),
        );
        let content = if jpeg && !image.color().has_alpha() {
            let mut out = Vec::new();
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 75);
            let rgb = if gray {
                image::DynamicImage::ImageLuma8(image.to_luma8())
            } else {
                image::DynamicImage::ImageRgb8(image.to_rgb8())
            };
            rgb.write_with_encoder(encoder)
                .map_err(|e| EditError::Image(e.to_string()))?;
            dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
            out
        } else {
            if image.color().has_alpha() {
                let alpha: Vec<u8> = image.to_rgba8().pixels().map(|p| p.0[3]).collect();
                let mut mask = Dictionary::new();
                mask.set("Type", Object::Name(b"XObject".to_vec()));
                mask.set("Subtype", Object::Name(b"Image".to_vec()));
                mask.set("Width", Object::Integer(width.into()));
                mask.set("Height", Object::Integer(height.into()));
                mask.set("BitsPerComponent", Object::Integer(8));
                mask.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
                mask.set("Filter", Object::Name(b"FlateDecode".to_vec()));
                let mask_id = self.add(Object::Stream(
                    Stream::new(mask, fonts::deflate(&alpha)).with_compression(false),
                ));
                dict.set("SMask", Object::Reference(mask_id));
            }
            let raw = if gray {
                image.to_luma8().into_raw()
            } else {
                image.to_rgb8().into_raw()
            };
            dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
            fonts::deflate(&raw)
        };
        let id = self.add(Object::Stream(
            Stream::new(dict, content).with_compression(false),
        ));
        let index = self.images.len();
        self.images.push(ImageEntry { id });
        Ok(ImageInfo {
            index,
            width,
            height,
        })
    }

    /// Adds a URI link annotation to a page.
    pub fn add_link(&mut self, page: ObjectId, rect: [f64; 4], uri: &str) {
        let mut action = Dictionary::new();
        action.set("Type", Object::Name(b"Action".to_vec()));
        action.set("S", Object::Name(b"URI".to_vec()));
        action.set("URI", Object::string_literal(uri));
        let mut annot = Dictionary::new();
        annot.set("Type", Object::Name(b"Annot".to_vec()));
        annot.set("Subtype", Object::Name(b"Link".to_vec()));
        annot.set(
            "Rect",
            Object::Array(rect.iter().map(|v| real(*v)).collect()),
        );
        annot.set("A", Object::Dictionary(action));
        let id = self.add(Object::Dictionary(annot));
        let mut annots = self.annotation_ids(page);
        annots.push(id);
        self.set_annotations(page, annots);
    }

    fn info_id(&mut self) -> ObjectId {
        if let Ok(Object::Reference(id)) = self.doc.trailer.get(b"Info")
            && self.dict(*id).is_some()
        {
            return *id;
        }
        let existing = match self.doc.trailer.get(b"Info") {
            Ok(Object::Dictionary(info)) => info.clone(),
            _ => Dictionary::new(),
        };
        let id = self.add(Object::Dictionary(existing));
        self.doc.trailer.set("Info", Object::Reference(id));
        id
    }

    /// An entry of the document information dictionary.
    #[must_use]
    pub fn info(&self, key: &str) -> Option<Vec<u8>> {
        let info = self
            .resolve(self.doc.trailer.get(b"Info").ok()?)
            .as_dict()
            .ok()?;
        self.resolve(info.get(key.as_bytes()).ok()?)
            .as_str()
            .ok()
            .map(<[u8]>::to_vec)
    }

    pub fn set_info_text(&mut self, key: &str, value: &str) {
        self.set_info(key, Object::string_literal(value));
    }

    pub fn set_info(&mut self, key: &str, value: Object) {
        let id = self.info_id();
        if let Some(info) = self.dict_mut(id) {
            info.set(key, value);
        }
    }

    /// Writes fonts, overlays and resources into the document's objects.
    fn finish(&mut self) -> Result<(), EditError> {
        let overlays = std::mem::take(&mut self.overlays);
        for (page, canvas) in overlays {
            let mut ops = b" Q q\n".to_vec();
            ops.extend_from_slice(&canvas.ops);
            ops.extend_from_slice(b" Q ");
            let mut dict = Dictionary::new();
            dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
            let before = self.add(Object::Stream(Stream::new(
                Dictionary::new(),
                b" q ".to_vec(),
            )));
            let after = self.add(Object::Stream(
                Stream::new(dict, fonts::deflate(&ops)).with_compression(false),
            ));
            let mut contents = self.contents(page);
            contents.insert(0, Object::Reference(before));
            contents.push(Object::Reference(after));
            if let Some(dict) = self.dict_mut(page) {
                dict.set("Contents", Object::Array(contents));
            }
        }
        let uses = std::mem::take(&mut self.uses);
        for (page, used) in uses {
            let mut resources = self
                .inherited(page, b"Resources")
                .and_then(|r| r.as_dict().ok().cloned())
                .unwrap_or_default();
            let sub = |resources: &mut Dictionary,
                       key: &str,
                       entries: Vec<(String, ObjectId)>,
                       this: &Self| {
                if entries.is_empty() {
                    return;
                }
                let mut dict = resources
                    .get(key.as_bytes())
                    .ok()
                    .and_then(|d| this.resolve(d).as_dict().ok().cloned())
                    .unwrap_or_default();
                for (name, id) in entries {
                    dict.set(name, Object::Reference(id));
                }
                resources.set(key, Object::Dictionary(dict));
            };
            let fonts = used
                .fonts
                .iter()
                .map(|i| {
                    (
                        font_name(self.fonts[*i].object_id),
                        self.fonts[*i].object_id,
                    )
                })
                .collect();
            sub(&mut resources, "Font", fonts, self);
            let mut xobjects: Vec<(String, ObjectId)> = used
                .images
                .iter()
                .map(|i| (image_name(self.images[*i].id), self.images[*i].id))
                .collect();
            xobjects.extend(used.forms);
            sub(&mut resources, "XObject", xobjects, self);
            if let Some(dict) = self.dict_mut(page) {
                dict.set("Resources", Object::Dictionary(resources));
            }
        }
        let fonts = std::mem::take(&mut self.fonts);
        for font in &fonts {
            let max_id = &mut self.doc.max_id;
            let objects = font.finish(|| {
                *max_id += 1;
                (*max_id, 0)
            })?;
            for (id, object) in objects {
                self.doc.objects.insert(id, object);
                self.dirty.insert(id);
            }
        }
        self.fonts = fonts;
        Ok(())
    }

    fn update_id(&mut self) {
        use sha2::{Digest as _, Sha256};
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut hasher = Sha256::new();
        hasher.update(&self.original);
        hasher.update(nanos.to_be_bytes());
        let fresh = hasher.finalize()[..16].to_vec();
        let first = match self.doc.trailer.get(b"ID") {
            Ok(Object::Array(id)) if !id.is_empty() => id[0].clone(),
            _ => Object::String(fresh.clone(), StringFormat::Hexadecimal),
        };
        self.doc.trailer.set(
            "ID",
            Object::Array(vec![
                first,
                Object::String(fresh, StringFormat::Hexadecimal),
            ]),
        );
    }

    /// `pdf.write(io, incremental: true)`, with `ModDate` set to `now`.
    pub fn save(mut self, now: &str) -> Result<Vec<u8>, EditError> {
        Ok(self.write_update(now)?.bytes)
    }

    /// `pdf.sign(io, reason:, certificate:, key:, certificate_chain:)`: an incremental
    /// update adding a signature field, signed over the whole file. `now` is the PDF date
    /// and `signing_time` the UTC `YYYYMMDDHHMMSS` time.
    pub fn save_signed(
        mut self,
        now: &str,
        signing_time: &str,
        reason: &str,
        key: &sign::SigningKey,
    ) -> Result<Vec<u8>, EditError> {
        const PLACEHOLDER: &[u8] = b"/ByteRange[0 1000000000000 1000000000000 1000000000000]";
        let utc = sign::utc_time(signing_time);
        let size = sign::signed_data(b"", key, &utc)?.len() + 5;
        let first_page = *self
            .page_ids()
            .first()
            .ok_or_else(|| EditError::Invalid("no pages".to_owned()))?;
        let annots = self.annotation_ids(first_page);
        self.set_annotations(first_page, annots);

        let mut signature = Dictionary::new();
        signature.set("Type", Object::Name(b"Sig".to_vec()));
        signature.set("Filter", Object::Name(b"Adobe.PPKLite".to_vec()));
        signature.set("SubFilter", Object::Name(b"adbe.pkcs7.detached".to_vec()));
        signature.set("M", pdf_date(now));
        signature.set("Reason", Object::string_literal(reason));
        signature.set(
            "ByteRange",
            Object::Array(vec![
                0.into(),
                1_000_000_000_000_i64.into(),
                1_000_000_000_000_i64.into(),
                1_000_000_000_000_i64.into(),
            ]),
        );
        signature.set(
            "Contents",
            Object::String(vec![0; size], StringFormat::Hexadecimal),
        );
        let signature_id = self.add(Object::Dictionary(signature));

        let form_id = self.acro_form_id()?;
        let mut fields: Vec<Object> = self
            .dict(form_id)
            .and_then(|f| f.get(b"Fields").ok())
            .and_then(|f| self.resolve(f).as_array().ok())
            .cloned()
            .unwrap_or_default();
        let index = fields
            .iter()
            .filter_map(|f| f.as_reference().ok())
            .filter_map(|f| self.dict(f))
            .filter_map(|f| f.get(b"T").ok().and_then(|t| self.resolve(t).as_str().ok()))
            .filter_map(|t| {
                let digits: String = t
                    .strip_prefix(b"Signature")?
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .map(|b| char::from(*b))
                    .collect();
                Some(digits.parse::<u64>().unwrap_or(0))
            })
            .max()
            .unwrap_or(0)
            + 1;
        let mut field = Dictionary::new();
        field.set("FT", Object::Name(b"Sig".to_vec()));
        field.set("T", Object::string_literal(format!("Signature{index}")));
        field.set("V", Object::Reference(signature_id));
        field.set("Type", Object::Name(b"Annot".to_vec()));
        field.set("Subtype", Object::Name(b"Widget".to_vec()));
        field.set(
            "Rect",
            Object::Array(vec![0.into(), 0.into(), 0.into(), 0.into()]),
        );
        field.set("F", Object::Integer(4));
        field.set("P", Object::Reference(first_page));
        let field_id = self.add(Object::Dictionary(field));
        fields.push(Object::Reference(field_id));
        if let Some(form) = self.dict_mut(form_id) {
            form.set("Fields", Object::Array(fields));
            form.set("SigFlags", Object::Integer(3));
        }
        let mut annots = self.annotation_ids(first_page);
        annots.push(field_id);
        self.set_annotations(first_page, annots);

        let written = self.write_update(now)?;
        let mut bytes = written.bytes;
        let offset = written
            .offsets
            .iter()
            .find(|(id, _)| *id == signature_id)
            .map(|(_, offset)| *offset)
            .ok_or_else(|| EditError::Sign("signature dictionary not written".to_owned()))?;
        let find = |bytes: &[u8], needle: &[u8]| {
            bytes[offset..]
                .windows(needle.len())
                .position(|w| w == needle)
                .map(|p| p + offset)
        };
        let contents_offset = find(&bytes, b"/Contents<")
            .ok_or_else(|| EditError::Sign("signature contents not found".to_owned()))?
            + b"/Contents".len();
        let offset2 = contents_offset + size * 2 + 2;
        let length2 = bytes.len() - offset2;
        let range_at = find(&bytes, PLACEHOLDER)
            .ok_or_else(|| EditError::Sign("byte range not found".to_owned()))?;
        let range = format!("/ByteRange[0 {contents_offset} {offset2} {length2}]");
        let range = format!("{range:<width$}", width = PLACEHOLDER.len());
        bytes[range_at..range_at + PLACEHOLDER.len()].copy_from_slice(range.as_bytes());

        let mut signed = bytes[..contents_offset].to_vec();
        signed.extend_from_slice(&bytes[offset2..]);
        let der = sign::signed_data(&signed, key, &utc)?;
        if der.len() > size {
            return Err(EditError::Sign(
                "signature larger than its placeholder".to_owned(),
            ));
        }
        let mut hex = String::with_capacity(size * 2);
        for byte in der {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        let hex = format!("{hex:0<width$}", width = size * 2);
        bytes[contents_offset + 1..offset2 - 1].copy_from_slice(hex.as_bytes());
        Ok(bytes)
    }

    fn write_update(&mut self, now: &str) -> Result<writer::Written, EditError> {
        self.finish()?;
        self.update_id();
        self.set_info("ModDate", pdf_date(now));
        self.set_info_text("Producer", PRODUCER);
        let objects: Vec<(ObjectId, &Object)> = self
            .dirty
            .iter()
            .filter_map(|id| self.doc.objects.get(id).map(|o| (*id, o)))
            .collect();
        let mut trailer = self.doc.trailer.clone();
        trailer.set("Size", Object::Integer(i64::from(self.doc.max_id) + 1));
        trailer.remove(b"Prev");
        Ok(writer::append_update(
            &self.original,
            &objects,
            &trailer,
            self.doc.reference_table.cross_reference_type,
            self.doc.xref_start,
        ))
    }
}

/// Font and image lookups for one page's overlay.
struct PageResources<'a> {
    fonts: &'a mut Vec<UsedFont>,
    images: &'a [ImageEntry],
    used: &'a mut PageUse,
}

impl canvas::Resources for PageResources<'_> {
    fn encode(&mut self, font_key: &str, glyphs: &[Glyph]) -> Object {
        let index = self
            .fonts
            .iter()
            .position(|f| font_name(f.object_id) == font_key)
            .unwrap_or(0);
        self.used.fonts.insert(index);
        self.fonts[index].encode(glyphs)
    }

    fn image(&self, image: usize) -> String {
        image_name(self.images[image].id)
    }
}

/// The resource names of fonts and images carry their object ids, which are new in every
/// revision, so a later revision's resources do not replace an earlier one's on a page.
fn font_name(id: ObjectId) -> String {
    format!("DSF{}", id.0)
}

fn image_name(id: ObjectId) -> String {
    format!("DSI{}", id.0)
}

/// An attribute inherited by a page of `doc` from its page tree ancestors.
fn inherited_in(doc: &Document, page: &Dictionary, key: &[u8]) -> Option<Object> {
    let mut parent = page.get(b"Parent").ok()?.as_reference().ok()?;
    for _ in 0..64 {
        let node = doc.get_object(parent).ok()?.as_dict().ok()?;
        if let Ok(value) = node.get(key) {
            return Some(value.clone());
        }
        parent = node.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

fn number(object: &Object) -> Option<f64> {
    match object {
        Object::Integer(v) =>
        {
            #[allow(clippy::cast_precision_loss, reason = "PDF numbers")]
            Some(*v as f64)
        }
        Object::Real(v) => Some(f64::from(*v)),
        _ => None,
    }
}

pub(crate) fn real(value: f64) -> Object {
    if value.fract() == 0.0 && value.abs() < 1e9 {
        #[allow(clippy::cast_possible_truncation, reason = "checked range")]
        return Object::Integer(value as i64);
    }
    #[allow(clippy::cast_possible_truncation, reason = "PDF reals")]
    Object::Real(value as f32)
}

/// `a` then `b` (`premultiply`): the matrix that applies `a` first.
fn multiply(a: [f64; 6], b: [f64; 6]) -> [f64; 6] {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}
