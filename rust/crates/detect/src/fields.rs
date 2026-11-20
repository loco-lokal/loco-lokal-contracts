//! `Templates::DetectFields`: fields detected on a page image, kept or promoted by the
//! page's underscores and lines, linked with the page text, and typed by the label
//! before them.

use std::sync::LazyLock;

use loco_pdf::pages::{DetectPage, LineNode, TextNode, ruby_sort};
use regex::Regex;
use serde_json::{Value, json};

use crate::DetectError;
use crate::image::Rgb;
use crate::inference::{self, Field, Options};
use crate::model::Model;

/// `DetectFields.call`'s defaults, as `TemplatesDetectFieldsController` calls it.
const CONFIDENCE: f64 = 0.3;
const NMS: f64 = 0.1;
const NMM: f64 = 0.5;
const PADDING: i64 = 20;

/// `ImageToFields.resolution * 1.5`: pages are rendered this many pixels along their
/// longer side for a model of `resolution`.
#[must_use]
pub fn render_size(model: &Model) -> f64 {
    #[allow(clippy::cast_precision_loss, reason = "a model input size")]
    let resolution = model.resolution as f64;
    resolution * 1.5
}

/// Ruby's `\s` is ASCII whitespace.
const S: &str = r"[ \t\n\x0B\f\r]";

static DATE_REGEXP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(?:date|signed{S}at|datum)[:_ \t\n\x0B\f\r-]*\z"
    ))
    .expect("valid regexp")
});

static NUMBER_REGEXP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:price|\$|€|total|quantity|prix|quantité|preis|summe|gesamt(?:betrag)?|menge|anzahl|stückzahl)[:_ \t\n\x0B\f\r-]*\z",
    )
    .expect("valid regexp")
});

static SIGNATURE_REGEXP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(?:signature|sign{S}here|sign|signez{S}ici|signer{S}ici|unterschrift|unterschreiben|unterzeichnen)[:_ \t\n\x0B\f\r-]*\z"
    ))
    .expect("valid regexp")
});

const LINEBREAK: [&str; 2] = ["\n", "\r"];
const CHECKBOXES: [&str; 2] = ["☐", "□"];

/// A box with Ruby's memoized `endx`/`endy`.
trait Area {
    fn x(&self) -> f64;
    fn y(&self) -> f64;
    fn w(&self) -> f64;
    fn h(&self) -> f64;
    fn endx(&self) -> f64;
    fn endy(&self) -> f64;
}

impl Area for Field {
    fn x(&self) -> f64 {
        self.x
    }
    fn y(&self) -> f64 {
        self.y
    }
    fn w(&self) -> f64 {
        self.w
    }
    fn h(&self) -> f64 {
        self.h
    }
    fn endx(&self) -> f64 {
        Field::endx(self)
    }
    fn endy(&self) -> f64 {
        Field::endy(self)
    }
}

/// `TextFieldBox`, and the widened lines of `extract_line_fields_from_page`.
#[derive(Clone, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    endy: f64,
}

impl Area for Rect {
    fn x(&self) -> f64 {
        self.x
    }
    fn y(&self) -> f64 {
        self.y
    }
    fn w(&self) -> f64 {
        self.w
    }
    fn h(&self) -> f64 {
        self.h
    }
    fn endx(&self) -> f64 {
        self.x + self.w
    }
    fn endy(&self) -> f64 {
        self.endy
    }
}

fn end_y(node: &TextNode) -> f64 {
    node.y + node.h
}

fn end_x(node: &TextNode) -> f64 {
    node.x + node.w
}

/// Ruby's `<=>` on floats inside a sort block (NaN compares equal; Rails would raise).
fn spaceship(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

/// `sort_fields`: rows (bottoms less than `y_threshold` apart), then `x`.
fn sort_fields(fields: &mut [Field], y_threshold: f64) {
    ruby_sort(fields, |a, b| {
        if (a.endy() - b.endy()).abs() < y_threshold {
            spaceship(a.x, b.x)
        } else {
            spaceship(a.endy(), b.endy())
        }
    });
}

/// `extract_text_fields_from_page`: runs of two or more underscores.
fn extract_text_fields(text_nodes: &[TextNode]) -> Vec<Rect> {
    let mut boxes = Vec::new();
    let mut i = 0;
    while i < text_nodes.len() {
        let node = &text_nodes[i];
        if node.text != "_" {
            i += 1;
            continue;
        }
        let (x1, mut y1, mut x2, mut y2) = (node.x, node.y, end_x(node), end_y(node));
        let mut count = 1;
        let mut j = i + 1;
        while j < text_nodes.len() {
            let next = &text_nodes[j];
            if next.text != "_" {
                break;
            }
            let distance = next.x - x2;
            let height_diff = (next.y - y1).abs();
            if distance > 0.02 || height_diff > node.h * 0.5 {
                break;
            }
            count += 1;
            x2 = end_x(next);
            y2 = ruby_max(y2, end_y(next));
            y1 = ruby_min(y1, next.y);
            j += 1;
        }
        if count >= 2 {
            let (w, h) = (x2 - x1, y2 - y1);
            boxes.push(Rect {
                x: x1,
                y: y1,
                w,
                h,
                endy: y1 + h,
            });
        }
        i = j;
    }
    boxes
}

fn ruby_max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

fn ruby_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

/// `extract_line_fields_from_page`: horizontal lines that are not part of a table and
/// not underlining text, widened upwards to hold a field.
fn extract_line_fields(page: &DetectPage) -> Vec<Rect> {
    let thickness = 5.0 / page.height;
    // `line_nodes` sorts the lines by their memoized `endy`; with a single line the sort
    // doesn't run, and `endy` is first read after the line is widened.
    let memoized = page.line_nodes.len() >= 2;
    let (vertical, all_horizontal): (Vec<&LineNode>, Vec<&LineNode>) =
        page.line_nodes.iter().partition(|line| line.tilt == 90);

    let horizontal = all_horizontal.into_iter().filter(|h_line| {
        if h_line.w > 0.7 && (h_line.h < 0.1 || h_line.h < 0.9) {
            return false;
        }
        if vertical.is_empty() {
            return true;
        }
        let (h_x_min, h_x_max) = (h_line.x, h_line.x + h_line.w);
        let h_y_avg = h_line.y + (h_line.h / 2.0);
        !vertical.iter().any(|v_line| {
            let v_x_avg = v_line.x + (v_line.w / 2.0);
            let (v_y_min, v_y_max) = (v_line.y, v_line.y + v_line.h);
            let x_overlap = v_x_avg - thickness <= h_x_max + thickness
                && v_x_avg + thickness >= h_x_min - thickness;
            let y_overlap = h_y_avg - thickness <= v_y_max + thickness
                && h_y_avg + thickness >= v_y_min - thickness;
            x_overlap && y_overlap
        })
    });

    let text_nodes = &page.text_nodes;
    let outside = |node: &TextNode, line: &LineNode| {
        node.x + node.w < line.x
            || line.x + line.w < node.x
            || node.y + node.h < line.y - node.h
            || line.y < node.y
    };
    // The text node index carries over from line to line, as in Rails.
    let mut node_index = 0;
    let mut kept = Vec::new();
    for line in horizontal {
        let mut first: Option<&TextNode> = None;
        let mut last: Option<&TextNode> = None;
        loop {
            node_index += 1;
            let Some(node) = text_nodes.get(node_index) else {
                break;
            };
            if node.y > line.y {
                break;
            }
            if outside(node, line) {
                continue;
            }
            first.get_or_insert(node);
            last = Some(node);
            let Some(next) = text_nodes.get(node_index + 1) else {
                break;
            };
            if outside(next, line) {
                break;
            }
        }
        if let (Some(first), Some(last)) = (first, last)
            && last.x + last.w - first.x > line.w / 2.0
        {
            continue;
        }
        kept.push(line);
    }

    kept.into_iter()
        .map(|line| {
            let h = line.h + 4.0 * thickness;
            let y = line.y - 4.0 * thickness;
            Rect {
                x: line.x,
                y,
                w: line.w,
                h,
                endy: if memoized { line.y + line.h } else { y + h },
            }
        })
        .collect()
}

fn calculate_iou(box1: &impl Area, box2: &impl Area) -> f64 {
    let x1 = ruby_max(box1.x(), box2.x());
    let y1 = ruby_max(box1.y(), box2.y());
    let x2 = ruby_min(box1.endx(), box2.endx());
    let y2 = ruby_min(box1.endy(), box2.endy());
    let intersection = ruby_max(0.0, x2 - x1) * ruby_max(0.0, y2 - y1);
    if intersection == 0.0 {
        return 0.0;
    }
    let union = box1.w() * box1.h() + box2.w() * box2.h() - intersection;
    intersection / union
}

fn boxes_overlap(box1: &impl Area, box2: &impl Area) -> bool {
    !(box1.endx() < box2.x()
        || box2.endx() < box1.x()
        || box1.endy() < box2.y()
        || box2.endy() < box1.y())
}

/// `increase_confidence_for_overlapping_fields(…, by: 1.0)`: unconfident text fields
/// that match an underscore run or a line well enough become confident.
fn increase_confidence(fields: &mut [Field], areas: &[Rect], confidence: f64) {
    if areas.is_empty() {
        return;
    }
    for field in fields {
        if field.confidence >= confidence || field.kind != Some("text") {
            continue;
        }
        for area in areas {
            if area.y > field.endy() {
                break;
            }
            if area.endy < field.y || !boxes_overlap(field, area) {
                continue;
            }
            if calculate_iou(field, area) < 0.4 {
                continue;
            }
            field.confidence += 1.0;
            break;
        }
    }
}

/// The end of the `PageNode` list: the text since the last field, or the last field.
#[derive(Clone, Debug, PartialEq)]
enum Tail {
    Text(String),
    Field { endx: f64, endy: f64 },
}

/// The text before each field and the end of the node list, kept from page to page
/// of a document (`head_node` and `tail_node`).
#[derive(Clone, Debug)]
pub struct PageNodes {
    tail: Tail,
}

impl Default for PageNodes {
    fn default() -> Self {
        Self {
            tail: Tail::Text(String::new()),
        }
    }
}

impl PageNodes {
    fn push_field(&mut self, field: Field, out: &mut Vec<(Field, Option<String>)>) {
        let prev = match &self.tail {
            Tail::Text(text) => Some(text.clone()),
            Tail::Field { .. } => None,
        };
        self.tail = Tail::Field {
            endx: field.endx(),
            endy: field.endy(),
        };
        out.push((field, prev));
    }

    /// `build_page_nodes`: places the fields between the page's characters. Returns
    /// each field with the text node before it (`None` when another field is).
    fn build(&mut self, page: &DetectPage, fields: Vec<Field>) -> Vec<(Field, Option<String>)> {
        let y_threshold = 4.0 / page.height;
        let x_threshold = 30.0 / page.width;
        let mut fields = fields.into_iter();
        let mut current = fields.next();
        let mut out = Vec::new();
        let mut prev_node: Option<&TextNode> = None;
        let linebreak = |node: &TextNode| LINEBREAK.contains(&node.text.as_str());

        for node in &page.text_nodes {
            // Rails compares the line break with itself, so line breaks are skipped.
            if linebreak(node) && (end_y(node) - end_y(node)) < y_threshold {
                continue;
            }
            while let Some(field) = current.take() {
                let same_line = (field.endy() - end_y(node)).abs() < y_threshold
                    && (field.x <= node.x || linebreak(node));
                if !(same_line || field.endy() < end_y(node)) {
                    current = Some(field);
                    break;
                }
                match &self.tail {
                    Tail::Field { endx, endy } => {
                        let divider = if (endy - field.endy()).abs() > y_threshold {
                            "\n"
                        } else if endx - field.x > x_threshold {
                            "\t"
                        } else {
                            " "
                        };
                        self.tail = Tail::Text(divider.into());
                    }
                    Tail::Text(_) => {
                        if prev_node.is_some_and(|p| (end_y(p) - field.endy()).abs() > y_threshold)
                        {
                            self.tail = Tail::Text("\n".into());
                        }
                    }
                }
                self.push_field(field, &mut out);
                current = fields.next();
            }

            let mut text = match std::mem::replace(&mut self.tail, Tail::Text(String::new())) {
                // A new text node after a field.
                Tail::Field { endx, endy } => {
                    let mut text = String::new();
                    if (end_y(node) - endy).abs() > y_threshold {
                        text.push('\n');
                    } else if (node.x - endx) > x_threshold {
                        text.push('\t');
                    }
                    text
                }
                Tail::Text(mut text) => {
                    if let Some(prev) = prev_node {
                        if (end_y(node) - end_y(prev)) > y_threshold && !linebreak(prev) {
                            text.push('\n');
                        } else if (node.x - end_x(prev)) > x_threshold && !text.ends_with('\t') {
                            text.push('\t');
                        }
                    }
                    text
                }
            };
            if (node.text != "_" || !text.ends_with("___"))
                && !CHECKBOXES.contains(&node.text.as_str())
            {
                text.push_str(&node.text);
            }
            self.tail = Tail::Text(text);
            prev_node = Some(node);
        }

        for field in current.into_iter().chain(fields) {
            self.push_field(field, &mut out);
        }
        match &mut self.tail {
            Tail::Field { .. } => self.tail = Tail::Text("\n".into()),
            Tail::Text(text) => text.push('\n'),
        }
        out
    }
}

/// `type_from_page_node`: text fields after a date, signature or number label.
fn type_from_label(field: &Field, label: Option<&str>) -> Option<&'static str> {
    let Some(label) = label else {
        return field.kind;
    };
    if field.kind != Some("text") {
        return field.kind;
    }
    Some(if DATE_REGEXP.is_match(label) {
        "date"
    } else if SIGNATURE_REGEXP.is_match(label) {
        "signature"
    } else if NUMBER_REGEXP.is_match(label) {
        "number"
    } else {
        "text"
    })
}

fn field_json(
    kind: Option<&str>,
    field: &Field,
    page: i64,
    attachment_uuid: Option<&str>,
) -> Value {
    json!({
        "uuid": uuid::Uuid::new_v4().to_string(),
        "type": kind,
        "required": kind == Some("signature"),
        "preferences": {},
        "areas": [{
            "x": field.x,
            "y": field.y,
            "w": field.w,
            "h": field.h,
            "page": page,
            "attachment_uuid": attachment_uuid,
        }],
    })
}

/// One page of `process_pdf_attachment`: the fields of `page` (rendered with
/// [`render_size`]), typed by their labels.
pub fn detect_pdf_page(
    model: &Model,
    nodes: &mut PageNodes,
    page: &DetectPage,
    page_index: i64,
    attachment_uuid: Option<&str>,
) -> Result<Vec<Value>, DetectError> {
    let image = Rgb::from_rgba(
        page.bitmap.width as usize,
        page.bitmap.height as usize,
        &page.bitmap.rgba,
    );
    let options = Options {
        confidence: CONFIDENCE / 3.0,
        nms: NMS,
        nmm: NMM,
        padding: Some(PADDING),
    };
    let mut fields = inference::call(model, &image, options)?;
    #[allow(clippy::cast_precision_loss, reason = "an image height")]
    sort_fields(&mut fields, 10.0 / image.height as f64);

    let text_fields = extract_text_fields(&page.text_nodes);
    let line_fields = extract_line_fields(page);
    increase_confidence(&mut fields, &text_fields, CONFIDENCE);
    increase_confidence(&mut fields, &line_fields, CONFIDENCE);
    fields.retain(|f| f.confidence >= CONFIDENCE || f.confidence.is_nan());

    Ok(nodes
        .build(page, fields)
        .iter()
        .map(|(field, label)| {
            field_json(
                type_from_label(field, label.as_deref()),
                field,
                page_index,
                attachment_uuid,
            )
        })
        .collect())
}

/// `process_image_attachment`: the fields of an image document's first preview.
pub fn detect_image(
    model: &Model,
    image: &Rgb,
    attachment_uuid: Option<&str>,
) -> Result<Vec<Value>, DetectError> {
    let options = Options {
        confidence: CONFIDENCE,
        nms: NMS,
        nmm: NMM,
        padding: Some(PADDING),
    };
    let mut fields = inference::call(model, image, options)?;
    #[allow(clippy::cast_precision_loss, reason = "an image height")]
    sort_fields(&mut fields, 10.0 / image.height as f64);
    Ok(fields
        .iter()
        .map(|field| field_json(field.kind, field, 0, attachment_uuid))
        .collect())
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, reason = "small test positions")]
mod tests {
    use super::*;

    fn text(text: &str, x: f64, y: f64) -> TextNode {
        TextNode {
            text: text.into(),
            x,
            y,
            w: 0.01,
            h: 0.012,
        }
    }

    fn field(kind: &'static str, x: f64, y: f64, w: f64) -> Field {
        Field {
            kind: Some(kind),
            x,
            y,
            w,
            h: 0.012,
            confidence: 0.9,
        }
    }

    fn page(text_nodes: Vec<TextNode>) -> DetectPage {
        DetectPage {
            bitmap: loco_pdf::pages::Bitmap {
                width: 1,
                height: 1,
                rgba: vec![255; 4],
            },
            width: 612.0,
            height: 792.0,
            text_nodes,
            line_nodes: Vec::new(),
        }
    }

    #[test]
    fn types_fields_by_label() {
        let mut nodes: Vec<TextNode> = "Date:"
            .chars()
            .enumerate()
            .map(|(i, c)| text(&c.to_string(), 0.1 + i as f64 * 0.01, 0.1))
            .collect();
        nodes.extend(
            "Unterschrift"
                .chars()
                .enumerate()
                .map(|(i, c)| text(&c.to_string(), 0.1 + i as f64 * 0.01, 0.2)),
        );
        let page = page(nodes);
        let fields = vec![
            field("text", 0.2, 0.1, 0.2),
            field("text", 0.3, 0.2, 0.2),
            field("checkbox", 0.6, 0.2, 0.02),
        ];
        let mut state = PageNodes::default();
        let placed = state.build(&page, fields);
        let types: Vec<_> = placed
            .iter()
            .map(|(f, label)| type_from_label(f, label.as_deref()))
            .collect();
        assert_eq!(types, [Some("date"), Some("signature"), Some("checkbox")]);
        assert_eq!(placed[0].1.as_deref(), Some("Date:"));
        assert_eq!(placed[1].1.as_deref(), Some("\nUnterschrift"));
        assert_eq!(placed[2].1, None);
        assert_eq!(state.tail, Tail::Text("\n".into()));
    }

    #[test]
    fn finds_underscore_runs() {
        let nodes = vec![
            text("_", 0.1, 0.5),
            text("_", 0.11, 0.5),
            text("_", 0.12, 0.5),
            text("a", 0.2, 0.5),
            text("_", 0.3, 0.5),
        ];
        let boxes = extract_text_fields(&nodes);
        assert_eq!(boxes.len(), 1);
        assert!((boxes[0].w - 0.03).abs() < 1e-12);
    }

    #[test]
    fn matches_labels_like_ruby() {
        assert!(DATE_REGEXP.is_match("Signed at:  __"));
        assert!(NUMBER_REGEXP.is_match("Gesamtbetrag €"));
        assert!(NUMBER_REGEXP.is_match("STÜCKZAHL:"));
        assert!(SIGNATURE_REGEXP.is_match("Signez\tici -"));
        assert!(!SIGNATURE_REGEXP.is_match("Signature here"));
        assert!(!DATE_REGEXP.is_match("Signed\u{a0}at"));
    }
}
