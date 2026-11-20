//! What Rails' `PDFium` build adds to page imports and redactions, done on the saved file:
//!
//! - `FPDF_ImportAcroForm`, called after each `FPDF_ImportPages`: the root fields of the
//!   widgets on the new document's pages are registered in its `/AcroForm /Fields`, in page
//!   order. A root whose `/T` is taken gets `_1` (or `_2`, …). `/DA`, `/DR`, `/Q` and
//!   `/NeedAppearances` come from the first imported source that has them. Without
//!   widgets there is no `/AcroForm`.
//! - Its `FPDFPage_Flatten` removes the flattened fields from `/Fields`, and
//!   `FPDF_RemoveOrphanObjects` (after a redaction) drops the objects nothing references
//!   any more, such as the fonts of redacted text.

use std::collections::{BTreeMap, HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId};

/// The `/AcroForm` keys `FPDF_ImportAcroForm` takes from a source, in `PDFium`'s
/// (sorted) dictionary order.
const INHERITED_KEYS: [&[u8]; 4] = [b"DA", b"DR", b"NeedAppearances", b"Q"];

fn load(data: &[u8]) -> Result<Document, String> {
    Document::load_mem(data).map_err(|e| format!("invalid PDF: {e}"))
}

fn save(doc: &mut Document) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    doc.save_to(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn reference(object: &Object) -> Option<ObjectId> {
    object.as_reference().ok()
}

fn dict<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    doc.dereference(object).ok()?.1.as_dict().ok()
}

/// The annotations of a page that are references.
fn page_annots(doc: &Document, page: ObjectId) -> Vec<ObjectId> {
    let Ok(page) = doc.get_dictionary(page) else {
        return Vec::new();
    };
    let Some(annots) = page
        .get(b"Annots")
        .ok()
        .and_then(|a| doc.dereference(a).ok())
        .and_then(|(_, a)| a.as_array().ok())
    else {
        return Vec::new();
    };
    annots.iter().filter_map(reference).collect()
}

/// The widget annotations of a page, as references.
fn page_widgets(doc: &Document, page: ObjectId) -> Vec<ObjectId> {
    page_annots(doc, page)
        .into_iter()
        .filter(|&id| {
            doc.get_dictionary(id)
                .ok()
                .and_then(|d| d.get(b"Subtype").ok())
                .and_then(|s| s.as_name().ok())
                == Some(b"Widget".as_slice())
        })
        .collect()
}

/// The top of a widget's field tree (`/Parent` up to the root).
fn root_field(doc: &Document, widget: ObjectId) -> ObjectId {
    let mut current = widget;
    let mut seen = HashSet::from([widget]);
    while let Some(parent) = doc
        .get_dictionary(current)
        .ok()
        .and_then(|d| d.get(b"Parent").ok())
        .and_then(reference)
    {
        if !seen.insert(parent) {
            break;
        }
        current = parent;
    }
    current
}

/// A text string with `_{n}` appended, in its own encoding.
fn suffixed(name: &[u8], n: usize) -> Vec<u8> {
    let suffix = format!("_{n}");
    let mut out = name.to_vec();
    if name.starts_with(&[0xFE, 0xFF]) {
        out.extend(suffix.encode_utf16().flat_map(u16::to_be_bytes));
    } else {
        out.extend_from_slice(suffix.as_bytes());
    }
    out
}

/// Copies `object` from `source` into `dest`, with the indirect objects it references.
fn copy_object(
    dest: &mut Document,
    source: &Document,
    object: &Object,
    map: &mut HashMap<ObjectId, ObjectId>,
) -> Object {
    match object {
        Object::Reference(id) => {
            if let Some(&copied) = map.get(id) {
                return Object::Reference(copied);
            }
            let new_id = dest.new_object_id();
            map.insert(*id, new_id);
            let copied = source
                .get_object(*id)
                .map_or(Object::Null, |o| copy_object(dest, source, &o.clone(), map));
            dest.objects.insert(new_id, copied);
            Object::Reference(new_id)
        }
        Object::Array(items) => Object::Array(
            items
                .iter()
                .map(|item| copy_object(dest, source, item, map))
                .collect(),
        ),
        Object::Dictionary(d) => {
            let mut out = Dictionary::new();
            for (key, value) in d {
                out.set(key.clone(), copy_object(dest, source, value, map));
            }
            Object::Dictionary(out)
        }
        Object::Stream(stream) => {
            let mut stream = stream.clone();
            let mut out = Dictionary::new();
            for (key, value) in &stream.dict {
                out.set(key.clone(), copy_object(dest, source, value, map));
            }
            stream.dict = out;
            Object::Stream(stream)
        }
        other => other.clone(),
    }
}

/// Copies the field `id` of `source` (`FPDF_ImportAcroForm`): its parents and kids, the
/// imported widgets as their copies in `map`, the others without their page (`/P`).
fn copy_field(
    dest: &mut Document,
    source: &Document,
    id: ObjectId,
    map: &mut HashMap<ObjectId, ObjectId>,
) -> ObjectId {
    if let Some(&copied) = map.get(&id) {
        return copied;
    }
    let new_id = dest.new_object_id();
    map.insert(id, new_id);
    let Ok(dict) = source.get_dictionary(id).cloned() else {
        let object = source.get_object(id).cloned().unwrap_or(Object::Null);
        let copied = copy_object(dest, source, &object, map);
        dest.objects.insert(new_id, copied);
        return new_id;
    };
    let widget = dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Widget".as_slice());
    let mut out = Dictionary::new();
    for (key, value) in &dict {
        let copied = match key.as_slice() {
            b"P" if widget => continue,
            b"Kids" => match source.dereference(value).map(|(_, v)| v.as_array()) {
                Ok(Ok(kids)) => Object::Array(
                    kids.iter()
                        .map(|kid| match reference(kid) {
                            Some(kid) => Object::Reference(copy_field(dest, source, kid, map)),
                            None => copy_object(dest, source, kid, map),
                        })
                        .collect(),
                ),
                _ => copy_object(dest, source, value, map),
            },
            b"Parent" => match reference(value) {
                Some(parent) => Object::Reference(copy_field(dest, source, parent, map)),
                None => copy_object(dest, source, value, map),
            },
            _ => copy_object(dest, source, value, map),
        };
        out.set(key.clone(), copied);
    }
    dest.objects.insert(new_id, Object::Dictionary(out));
    new_id
}

/// `FPDF_ImportPages` leaves an annotation's `/Parent` with the source's object number;
/// the field trees of the widgets one import brought are copied from its source.
fn copy_parents(
    doc: &mut Document,
    pages: &[ObjectId],
    source: &Document,
    numbers: &[usize],
    start: usize,
) {
    let source_pages: Vec<ObjectId> = source.get_pages().into_values().collect();
    let mut map = HashMap::new();
    for (offset, &number) in numbers.iter().enumerate() {
        let (Some(&dest_page), Some(&source_page)) = (
            pages.get(start + offset),
            number.checked_sub(1).and_then(|n| source_pages.get(n)),
        ) else {
            continue;
        };
        let (copied, original) = (
            page_annots(doc, dest_page),
            page_annots(source, source_page),
        );
        if copied.len() == original.len() {
            map.extend(original.into_iter().zip(copied));
        }
    }
    for offset in 0..numbers.len() {
        let Some(&page) = pages.get(start + offset) else {
            continue;
        };
        for widget in page_widgets(doc, page) {
            let Some(parent) = doc
                .get_dictionary(widget)
                .ok()
                .and_then(|d| d.get(b"Parent").ok())
                .and_then(reference)
            else {
                continue;
            };
            let linked = doc
                .get_dictionary(parent)
                .ok()
                .and_then(|p| p.get(b"Kids").ok())
                .and_then(|k| doc.dereference(k).ok())
                .and_then(|(_, k)| k.as_array().ok())
                .is_some_and(|kids| kids.iter().any(|k| reference(k) == Some(widget)));
            if linked || source.get_object(parent).is_err() {
                continue;
            }
            let copied = copy_field(doc, source, parent, &mut map);
            if let Ok(Object::Dictionary(d)) = doc.get_object_mut(widget) {
                d.set("Parent", Object::Reference(copied));
            }
        }
    }
}

/// One `FPDF_ImportPages` call: the source, its pages (`"1,3"`) and their number.
#[derive(Clone, Copy, Debug)]
pub struct Import<'a> {
    pub source: &'a [u8],
    pub pages: &'a str,
    pub length: i32,
}

/// The source's `/AcroForm`, if it has one.
fn source_form(source: &Document) -> Option<Dictionary> {
    let catalog = source.catalog().ok()?;
    dict(source, catalog.get(b"AcroForm").ok()?).cloned()
}

/// `FPDF_ImportAcroForm` after each import: `imports` are the sources in import order
/// with the number of pages each import added. Returns `data` unchanged when no imported
/// page has widgets.
pub fn import(data: Vec<u8>, imports: &[Import<'_>]) -> Result<Vec<u8>, String> {
    let mut doc = load(&data)?;
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    if !pages.iter().any(|&p| !page_widgets(&doc, p).is_empty()) {
        return Ok(data);
    }
    let sources = imports
        .iter()
        .map(|i| load(i.source))
        .collect::<Result<Vec<_>, _>>()?;
    let mut start = 0;
    for (import, source) in imports.iter().zip(&sources) {
        let numbers: Vec<usize> = import
            .pages
            .split(',')
            .filter_map(|n| n.trim().parse().ok())
            .collect();
        copy_parents(&mut doc, &pages, source, &numbers, start);
        start += usize::try_from(import.length).unwrap_or(0);
    }

    let mut fields: Vec<ObjectId> = Vec::new();
    let mut names: HashSet<Vec<u8>> = HashSet::new();
    let mut form: Option<BTreeMap<Vec<u8>, Object>> = None;
    let mut end = 0usize;
    for (import, source) in imports.iter().zip(&sources) {
        end = (end + usize::try_from(import.length).unwrap_or(0)).min(pages.len());
        let mut added = false;
        for &page in &pages[..end] {
            for widget in page_widgets(&doc, page) {
                let root = root_field(&doc, widget);
                if fields.contains(&root) {
                    continue;
                }
                fields.push(root);
                added = true;
                let name = doc
                    .get_dictionary(root)
                    .ok()
                    .and_then(|d| d.get(b"T").ok())
                    .and_then(|t| t.as_str().ok())
                    .map(<[u8]>::to_vec);
                if let Some(name) = name {
                    let mut unique = name.clone();
                    let mut n = 0;
                    while names.contains(&unique) {
                        n += 1;
                        unique = suffixed(&name, n);
                    }
                    if unique != name
                        && let Ok(Object::Dictionary(d)) = doc.get_object_mut(root)
                    {
                        d.set(
                            "T",
                            Object::String(unique.clone(), lopdf::StringFormat::Literal),
                        );
                    }
                    names.insert(unique);
                }
            }
        }
        if form.is_none() && !added {
            continue;
        }
        let form = form.get_or_insert_with(BTreeMap::new);
        let Some(source_form) = source_form(source) else {
            continue;
        };
        let mut map = HashMap::new();
        for key in INHERITED_KEYS {
            if form.contains_key(key) {
                continue;
            }
            if let Ok(value) = source_form.get(key) {
                let copied = copy_object(&mut doc, source, value, &mut map);
                form.insert(key.to_vec(), copied);
            }
        }
    }

    let mut acro_form = Dictionary::new();
    let form = form.unwrap_or_default();
    for (key, value) in &form {
        if key.as_slice() < b"Fields".as_slice() {
            acro_form.set(key.clone(), value.clone());
        }
    }
    acro_form.set(
        "Fields",
        Object::Array(fields.into_iter().map(Object::Reference).collect()),
    );
    for (key, value) in &form {
        if key.as_slice() > b"Fields".as_slice() {
            acro_form.set(key.clone(), value.clone());
        }
    }
    let id = doc.add_object(Object::Dictionary(acro_form));
    doc.catalog_mut()
        .map_err(|e| e.to_string())?
        .set("AcroForm", Object::Reference(id));
    save(&mut doc)
}

/// After page operations: fields whose widgets are no longer on a page (flattened) leave
/// `/Fields`, and with `remove_orphans` (a redaction ran) unreferenced objects are dropped.
pub fn after_flatten(data: Vec<u8>, remove_orphans: bool) -> Result<Vec<u8>, String> {
    let mut doc = load(&data)?;
    let mut changed = false;
    let form_id = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"AcroForm").ok())
        .and_then(reference);
    if let Some(form_id) = form_id {
        let mut on_pages = HashSet::new();
        for page in doc.get_pages().into_values() {
            for widget in page_widgets(&doc, page) {
                on_pages.insert(root_field(&doc, widget));
            }
        }
        let fields: Vec<Object> = doc
            .get_dictionary(form_id)
            .ok()
            .and_then(|f| f.get(b"Fields").ok())
            .and_then(|f| f.as_array().ok())
            .cloned()
            .unwrap_or_default();
        let kept: Vec<Object> = fields
            .iter()
            .filter(|f| reference(f).is_none_or(|id| on_pages.contains(&id)))
            .cloned()
            .collect();
        if kept.len() != fields.len()
            && let Ok(Object::Dictionary(form)) = doc.get_object_mut(form_id)
        {
            form.set("Fields", Object::Array(kept));
            changed = true;
        }
    }
    if remove_orphans && !doc.prune_objects().is_empty() {
        changed = true;
    }
    if !changed {
        return Ok(data);
    }
    save(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_names_in_their_encoding() {
        assert_eq!(suffixed(b"addr", 1), b"addr_1");
        assert_eq!(
            suffixed(&[0xFE, 0xFF, 0, b'a'], 2),
            [0xFE, 0xFF, 0, b'a', 0, b'_', 0, b'2']
        );
    }
}
