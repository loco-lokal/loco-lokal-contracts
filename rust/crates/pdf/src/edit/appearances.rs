//! Appearance streams for interactive form fields, as `maybe_flatten_pdf` has HexaPDF create
//! them before flattening: `acro_form.create_appearances(force: true)` when the form has
//! `NeedAppearances`, then the `create_appearances` of each field in `acro_form.flatten`.
//!
//! A port of `HexaPDF::Type::AcroForm::AppearanceGenerator` and the fields'
//! `create_appearances`, with `DocuSeal`'s patches (`config/initializers/hexapdf.rb`):
//! text fields (single-line, comb, multiline), combo and list boxes, check boxes and radio
//! buttons; push buttons get none. Where HexaPDF would raise (and Rails then leaves the
//! form unflattened), this stops with an "is not supported yet" error instead: text the
//! field's font cannot encode, embedded TrueType fonts, CMYK text, time formats, and
//! unusual appearance dictionaries.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt::Write as _,
    sync::Arc,
};

use lopdf::{Dictionary, Object, ObjectId, Stream};

use super::{
    EditError, Editor,
    canvas::{Canvas, Resources},
    fonts::{FontData, Glyph, UsedFont},
    layout::{Align, Fitted, Fragment, Input, Item, Layouter, Line, TextStyle, Valign},
    number, real,
    writer::real as fmt,
};

/// `acro_form.default_font_size`.
const DEFAULT_FONT_SIZE: f64 = 10.0;
/// `acro_form.text_field.default_width`.
const DEFAULT_WIDTH: f64 = 100.0;

const UNSUPPORTED_TEXT: &str = "Form field text that its font cannot encode";

/// Field flags (`FLAGS_BIT_MAPPING`).
const MULTILINE: i64 = 1 << 12;
const PASSWORD: i64 = 1 << 13;
const RADIO: i64 = 1 << 15;
const PUSH_BUTTON: i64 = 1 << 16;
const COMBO: i64 = 1 << 17;
const FILE_SELECT: i64 = 1 << 20;
const COMB: i64 = 1 << 24;

/// Widget annotation flags.
const HIDDEN: i64 = 1 << 1;
const PRINT: i64 = 1 << 2;

/// The widgets' appearance states of an earlier pass (`widget.cache`).
#[derive(Default)]
struct Cache {
    text: HashMap<ObjectId, Option<String>>,
    choice: HashMap<ObjectId, Vec<Option<Object>>>,
}

/// A device color (`prenormalized_device_color`).
#[derive(Clone, Debug, PartialEq)]
struct Color(Vec<f64>);

impl Color {
    fn from_components(components: &[f64]) -> Result<Option<Self>, EditError> {
        match components.len() {
            0 => Ok(None),
            1 | 3 | 4 => Ok(Some(Self(components.to_vec()))),
            _ => Err(EditError::Unsupported(
                "form field colors with 2 components",
            )),
        }
    }

    fn op(&self, stroke: bool) -> String {
        let values: Vec<String> = self.0.iter().map(|v| fmt(*v)).collect();
        let op = match (self.0.len(), stroke) {
            (1, false) => "g",
            (1, true) => "G",
            (3, false) => "rg",
            (3, true) => "RG",
            (_, false) => "k",
            (_, true) => "K",
        };
        format!("{} {op}", values.join(" "))
    }

    /// The color as RGB for text (gray as equal components).
    fn rgb(&self) -> Result<[f64; 3], EditError> {
        match self.0.as_slice() {
            [g] => Ok([*g; 3]),
            [r, g, b] => Ok([*r, *g, *b]),
            _ => Err(EditError::Unsupported("CMYK text in form fields")),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum BorderKind {
    Solid,
    Underlined,
    Other,
    Dashed(Vec<f64>),
}

/// `widget.border_style`.
struct BorderStyle {
    width: f64,
    color: Option<Color>,
    kind: BorderKind,
}

/// The parsed default appearance string: font name, size and color.
#[derive(Default)]
struct Appearance {
    font: Option<Vec<u8>>,
    size: Option<f64>,
    color: Option<Color>,
}

/// `VariableTextField.parse_appearance_string`: the `Tf` operands and the last color.
fn parse_appearance(da: &[u8]) -> Result<Appearance, EditError> {
    let text = String::from_utf8_lossy(da).replacen("//", "/", 1);
    let mut result = Appearance::default();
    let mut operands: Vec<&str> = Vec::new();
    for token in text.split_whitespace() {
        let is_operand = token.starts_with('/')
            || token
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.'));
        if is_operand {
            operands.push(token);
            continue;
        }
        let numbers: Vec<f64> = operands.iter().filter_map(|o| o.parse().ok()).collect();
        match token {
            "Tf" => {
                result.font = operands
                    .iter()
                    .find_map(|o| o.strip_prefix('/'))
                    .map(|n| n.as_bytes().to_vec());
                result.size = operands.last().and_then(|o| o.parse().ok());
            }
            "g" | "rg" | "k" => result.color = Color::from_components(&numbers)?,
            _ => {}
        }
        operands.clear();
    }
    Ok(result)
}

/// The standard font a font resource of the form draws with, and the characters its
/// encoding has (`font_object.font_wrapper`, else the `Helvetica` fallback font).
struct FieldFont {
    family: &'static str,
    variant: &'static str,
    chars: HashSet<char>,
}

/// `StandardFonts`: the standard 14 names and Adobe's aliases, as (family, variant).
fn standard_font(base: &[u8]) -> Option<(&'static str, &'static str)> {
    Some(match base {
        b"Helvetica" | b"Arial" => ("Helvetica", "none"),
        b"Helvetica-Bold" | b"Arial,Bold" => ("Helvetica", "bold"),
        b"Helvetica-Oblique" | b"Arial,Italic" => ("Helvetica", "italic"),
        b"Helvetica-BoldOblique" | b"Arial,BoldItalic" => ("Helvetica", "bold_italic"),
        b"Times-Roman" | b"TimesNewRoman" => ("Times", "none"),
        b"Times-Bold" | b"TimesNewRoman,Bold" => ("Times", "bold"),
        b"Times-Italic" | b"TimesNewRoman,Italic" => ("Times", "italic"),
        b"Times-BoldItalic" | b"TimesNewRoman,BoldItalic" => ("Times", "bold_italic"),
        b"Courier" | b"CourierNew" => ("Courier", "none"),
        b"Courier-Bold" | b"CourierNew,Bold" => ("Courier", "bold"),
        b"Courier-Oblique" | b"CourierNew,Italic" => ("Courier", "italic"),
        b"Courier-BoldOblique" | b"CourierNew,BoldItalic" => ("Courier", "bold_italic"),
        b"ZapfDingbats" => ("ZapfDingbats", "none"),
        _ => return None,
    })
}

/// A field and one of its widgets, with what the generators read from them.
struct Target {
    field: ObjectId,
    widget: ObjectId,
}

/// Fragments drawn into a form `XObject`, with the fonts they use.
struct FormResources<'a> {
    fonts: &'a mut Vec<UsedFont>,
    name_base: usize,
    used: BTreeSet<usize>,
}

impl Resources for FormResources<'_> {
    fn encode(&mut self, font_key: &str, glyphs: &[Glyph]) -> Object {
        let number: usize = font_key.trim_start_matches("DSF").parse().unwrap_or(0);
        let index = number.saturating_sub(self.name_base);
        self.used.insert(index);
        self.fonts[index].encode(glyphs)
    }

    fn image(&self, image: usize) -> String {
        format!("DSI{}", self.name_base + image)
    }
}

impl Editor {
    /// `maybe_flatten_pdf`'s appearance generation: the forced pass for `NeedAppearances`,
    /// then the one `acro_form.flatten` makes for fields that need it.
    pub fn create_form_appearances(&mut self) -> Result<(), EditError> {
        if self.acro_form_dict().is_none() {
            return Ok(());
        }
        let mut cache = Cache::default();
        if self.needs_appearances() {
            self.create_appearances(true, &mut cache)?;
        }
        self.create_appearances(false, &mut cache)
    }

    fn acro_form_dict(&self) -> Option<&Dictionary> {
        self.catalog()
            .and_then(|c| c.get(b"AcroForm").ok())
            .and_then(|f| self.resolve(f).as_dict().ok())
    }

    /// `each_field` (`DocuSeal`'s cycle-safe version): the terminal fields.
    fn terminal_fields(&self) -> Result<Vec<ObjectId>, EditError> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let roots = self
            .acro_form_dict()
            .and_then(|f| f.get(b"Fields").ok())
            .and_then(|f| self.resolve(f).as_array().ok())
            .cloned()
            .unwrap_or_default();
        let mut stack = vec![roots.into_iter()];
        while let Some(items) = stack.last_mut() {
            let Some(item) = items.next() else {
                stack.pop();
                continue;
            };
            let id = match item {
                Object::Null => continue,
                Object::Reference(id) => id,
                _ => {
                    return Err(EditError::Unsupported(
                        "form fields that are not indirect objects",
                    ));
                }
            };
            if self.dict(id).is_none() || !seen.insert(id) {
                continue;
            }
            if self.is_terminal(id) {
                out.push(id);
            } else {
                let kids = self.kids(id);
                stack.push(kids.into_iter());
            }
        }
        Ok(out)
    }

    fn kids(&self, id: ObjectId) -> Vec<Object> {
        self.dict(id)
            .and_then(|d| d.get(b"Kids").ok())
            .and_then(|k| self.resolve(k).as_array().ok())
            .cloned()
            .unwrap_or_default()
    }

    /// `terminal_field?`: no kids, or none of them has a name.
    fn is_terminal(&self, id: ObjectId) -> bool {
        self.kids(id)
            .iter()
            .all(|kid| !self.resolve(kid).as_dict().is_ok_and(|kid| kid.has(b"T")))
    }

    /// `Field.inherited_value(field, name)`: the value on the field or its nearest parent.
    fn inherited_value(&self, field: ObjectId, key: &[u8]) -> Option<Object> {
        let mut seen = HashSet::new();
        let mut current = field;
        loop {
            let dict = self.dict(current)?;
            if let Ok(value) = dict.get(key)
                && !matches!(value, Object::Null)
            {
                return Some(self.resolve(value).clone());
            }
            let parent = dict.get(b"Parent").ok()?.as_reference().ok()?;
            if !seen.insert(parent) {
                return None;
            }
            current = parent;
        }
    }

    fn flags(&self, field: ObjectId) -> i64 {
        self.inherited_value(field, b"Ff")
            .and_then(|f| f.as_i64().ok())
            .unwrap_or(0)
    }

    /// `each_widget(direct_only: true)`.
    fn widgets(&self, field: ObjectId) -> Vec<ObjectId> {
        if self.dict(field).is_some_and(|d| d.has(b"Subtype")) {
            return vec![field];
        }
        self.kids(field)
            .iter()
            .filter_map(|k| k.as_reference().ok())
            .filter(|k| {
                self.dict(*k)
                    .and_then(|d| d.get(b"Subtype").ok())
                    .and_then(|s| s.as_name().ok())
                    == Some(b"Widget")
            })
            .collect()
    }

    fn create_appearances(&mut self, force: bool, cache: &mut Cache) -> Result<(), EditError> {
        for field in self.terminal_fields()? {
            let kind = self
                .inherited_value(field, b"FT")
                .and_then(|t| t.as_name().ok().map(<[u8]>::to_vec));
            match kind.as_deref() {
                Some(b"Tx") => {
                    let value = self.text_value(field);
                    for widget in self.widgets(field) {
                        let cached = cache.text.get(&widget);
                        if !force {
                            if cached.is_some_and(|c| *c == value) {
                                continue;
                            }
                            if cached.is_none() && self.appearance(widget).is_some() {
                                cache.text.insert(widget, value.clone());
                                continue;
                            }
                        }
                        cache.text.insert(widget, value.clone());
                        self.create_text_appearances(&Target { field, widget })?;
                    }
                }
                Some(b"Ch") => {
                    let state: Vec<Option<Object>> = [&b"V"[..], b"I", b"Opt", b"TI"]
                        .iter()
                        .map(|key| {
                            if matches!(*key, b"V") {
                                self.inherited_value(field, key)
                            } else {
                                self.dict(field)
                                    .and_then(|d| d.get(key).ok())
                                    .map(|v| self.resolve(v).clone())
                            }
                        })
                        .collect();
                    for widget in self.widgets(field) {
                        if !force && cache.choice.get(&widget) == Some(&state) {
                            continue;
                        }
                        cache.choice.insert(widget, state.clone());
                        self.create_text_appearances(&Target { field, widget })?;
                    }
                }
                Some(b"Btn") => {
                    let push_button = self.flags(field) & PUSH_BUTTON != 0;
                    for widget in self.widgets(field) {
                        if push_button {
                            // DocuSeal's `create_push_button_appearances` does nothing.
                            continue;
                        }
                        if !force && self.has_two_state_appearances(widget) {
                            continue;
                        }
                        self.create_check_box_appearances(&Target { field, widget })?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The normal appearance is a dictionary of exactly two states, both streams.
    fn has_two_state_appearances(&self, widget: ObjectId) -> bool {
        let Some(normal) = self.normal_appearance(widget) else {
            return false;
        };
        let Object::Dictionary(states) = self.resolve(&normal) else {
            return false;
        };
        states.len() == 2
            && states
                .iter()
                .all(|(_, v)| matches!(self.resolve(v), Object::Stream(_)))
    }

    fn normal_appearance(&self, widget: ObjectId) -> Option<Object> {
        let ap = self
            .resolve(self.dict(widget)?.get(b"AP").ok()?)
            .as_dict()
            .ok()?;
        ap.get(b"N").ok().cloned()
    }

    /// `DocuSeal`'s `TextField#field_value`.
    fn text_value(&self, field: ObjectId) -> Option<String> {
        match self.inherited_value(field, b"V")? {
            Object::Name(name) => Some(String::from_utf8_lossy(&name).into_owned()),
            Object::String(..) => self
                .inherited_value(field, b"V")
                .and_then(|v| lopdf::decode_text_string(&v).ok()),
            Object::Stream(stream) => Some(
                String::from_utf8_lossy(
                    &stream
                        .decompressed_content()
                        .unwrap_or_else(|_| stream.content.clone()),
                )
                .into_owned(),
            ),
            _ => None,
        }
    }

    /// `ChoiceField#field_value` (`process_value(self[:V])`).
    fn choice_value(&self, field: ObjectId) -> Option<Vec<String>> {
        let text = |o: &Object| lopdf::decode_text_string(o).ok();
        match self.inherited_value(field, b"V")? {
            Object::Array(items) => Some(
                items
                    .iter()
                    .map(|i| text(self.resolve(i)).unwrap_or_default())
                    .collect(),
            ),
            value @ Object::String(..) => text(&value).map(|v| vec![v]),
            _ => None,
        }
    }

    /// `option_items`: the display texts of `Opt`.
    fn option_items(&self, field: ObjectId) -> Vec<String> {
        // `prepare_option_items`: a terminal field without `Opt` takes its first kid's.
        let kids = self.kids(field);
        let opt = self
            .dict(field)
            .and_then(|d| d.get(b"Opt").ok())
            .or_else(|| {
                kids.iter().find_map(|k| {
                    self.resolve(k)
                        .as_dict()
                        .ok()
                        .and_then(|d| d.get(b"Opt").ok())
                })
            })
            .and_then(|o| self.resolve(o).as_array().ok())
            .cloned()
            .unwrap_or_default();
        opt.iter()
            .map(|item| {
                let item = match self.resolve(item) {
                    Object::Array(pair) => pair
                        .get(1)
                        .map_or(Object::Null, |v| self.resolve(v).clone()),
                    other => other.clone(),
                };
                lopdf::decode_text_string(&item).unwrap_or_default()
            })
            .collect()
    }

    /// The widget's `Rect`, normalized (`HexaPDF::Rectangle`).
    fn widget_rect(&self, widget: ObjectId) -> [f64; 4] {
        self.dict(widget)
            .and_then(|d| d.get(b"Rect").ok())
            .and_then(|r| self.rect(r))
            .unwrap_or_default()
    }

    fn set_rect(&mut self, widget: ObjectId, rect: [f64; 4]) {
        if let Some(dict) = self.dict_mut(widget) {
            dict.set(
                "Rect",
                Object::Array(rect.iter().map(|v| real(*v)).collect()),
            );
        }
    }

    fn mk(&self, widget: ObjectId) -> Option<Dictionary> {
        self.dict(widget)
            .and_then(|d| d.get(b"MK").ok())
            .and_then(|m| self.resolve(m).as_dict().ok())
            .cloned()
    }

    fn numbers(&self, object: Option<&Object>) -> Vec<f64> {
        object
            .and_then(|o| self.resolve(o).as_array().ok())
            .map(|a| a.iter().filter_map(|v| number(self.resolve(v))).collect())
            .unwrap_or_default()
    }

    /// `widget.border_style`.
    fn border_style(&self, widget: ObjectId) -> Result<BorderStyle, EditError> {
        let mut style = BorderStyle {
            width: 1.0,
            color: None,
            kind: BorderKind::Solid,
        };
        if let Some(mk) = self.mk(widget) {
            style.color = Color::from_components(&self.numbers(mk.get(b"BC").ok()))?;
        }
        let dict = self.dict(widget).cloned().unwrap_or_default();
        if let Some(bs) = dict
            .get(b"BS")
            .ok()
            .and_then(|b| self.resolve(b).as_dict().ok())
        {
            if let Some(width) = bs.get(b"W").ok().and_then(|w| number(self.resolve(w))) {
                style.width = width;
            }
            style.kind = match bs.get(b"S").ok().and_then(|s| s.as_name().ok()) {
                Some(b"U") => BorderKind::Underlined,
                Some(b"B" | b"I") => BorderKind::Other,
                Some(b"D") => BorderKind::Dashed(self.numbers(bs.get(b"D").ok())),
                _ => BorderKind::Solid,
            };
        } else if let Ok(border) = dict.get(b"Border").map(|b| self.resolve(b)) {
            let border = border.as_array().cloned().unwrap_or_default();
            style.width = border
                .get(2)
                .and_then(|w| number(self.resolve(w)))
                .unwrap_or(0.0);
            if let Some(dash) = border.get(3) {
                style.kind = BorderKind::Dashed(self.numbers(Some(dash)));
            }
        }
        Ok(style)
    }

    fn background_color(&self, widget: ObjectId) -> Result<Option<Color>, EditError> {
        match self.mk(widget) {
            Some(mk) => Color::from_components(&self.numbers(mk.get(b"BG").ok())),
            None => Ok(None),
        }
    }

    /// `perform_rotation(width, height)`: the box size and matrix for `MK /R`.
    fn widget_rotation(
        &self,
        widget: ObjectId,
        width: f64,
        height: f64,
    ) -> (f64, f64, Option<[f64; 6]>) {
        let r = self
            .mk(widget)
            .and_then(|mk| mk.get(b"R").ok().and_then(|r| r.as_i64().ok()))
            .unwrap_or(0)
            .rem_euclid(360);
        match r {
            90 => (height, width, Some([0.0, 1.0, -1.0, 0.0, 0.0, 0.0])),
            270 => (height, width, Some([0.0, -1.0, 1.0, 0.0, 0.0, 0.0])),
            180 => (width, height, Some([0.0, -1.0, -1.0, 0.0, 0.0, 0.0])),
            _ => (width, height, None),
        }
    }

    fn set_widget_flags(&mut self, widget: ObjectId, appearance_state: Option<&[u8]>) {
        if let Some(dict) = self.dict_mut(widget) {
            let flags = dict
                .get(b"F")
                .ok()
                .and_then(|f| f.as_i64().ok())
                .unwrap_or(0);
            dict.set("F", Object::Integer((flags | PRINT) & !HIDDEN));
            if let Some(state) = appearance_state {
                dict.set("AS", Object::Name(state.to_vec()));
            }
        }
    }

    /// `apply_background_and_border(border_style, canvas, circular:)`.
    fn background_and_border(
        &self,
        target: &Target,
        border: &BorderStyle,
        comb_cells: Option<i64>,
        circular: bool,
        ops: &mut String,
    ) -> Result<(), EditError> {
        let rect = self.widget_rect(target.widget);
        let (rw, rh) = (rect[2] - rect[0], rect[3] - rect[1]);
        let background = self.background_color(target.widget)?;
        if !((border.width > 0.0 && border.color.is_some()) || background.is_some()) {
            return Ok(());
        }
        ops.push_str("q\n");
        if let Some(background) = &background {
            let _ = writeln!(ops, "{}", background.op(false));
            if circular {
                circle(ops, rw / 2.0, rh / 2.0, (rw / 2.0).min(rh / 2.0));
            } else {
                let _ = writeln!(ops, "0 0 {} {} re", fmt(rw), fmt(rh));
            }
            ops.push_str("f\n");
        }
        if let Some(color) = &border.color {
            let offset = (border.width / 2.0).max(0.5);
            let (width, height) = (rw - 2.0 * offset, rh - 2.0 * offset);
            let _ = writeln!(ops, "{}\n{} w", color.op(true), fmt(border.width));
            if border.kind == BorderKind::Underlined {
                if circular {
                    arc(
                        ops,
                        rw / 2.0,
                        rh / 2.0,
                        (width / 2.0).min(height / 2.0),
                        180.0,
                        360.0,
                        true,
                    );
                } else {
                    let _ = writeln!(
                        ops,
                        "{} {} m {} {} l",
                        fmt(offset),
                        fmt(offset),
                        fmt(offset + width),
                        fmt(offset)
                    );
                }
            } else {
                if let BorderKind::Dashed(dash) = &border.kind {
                    let dash: Vec<String> = dash.iter().map(|d| fmt(*d)).collect();
                    let _ = writeln!(ops, "[{}] 0 d", dash.join(" "));
                }
                if circular {
                    circle(ops, rw / 2.0, rh / 2.0, (width / 2.0).min(height / 2.0));
                } else {
                    let _ = writeln!(
                        ops,
                        "{} {} {} {} re",
                        fmt(offset),
                        fmt(offset),
                        fmt(width),
                        fmt(height)
                    );
                    if let Some(cells) = comb_cells {
                        #[allow(clippy::cast_precision_loss, reason = "MaxLen")]
                        let cell_width = rw / cells as f64;
                        for i in 1..cells {
                            #[allow(clippy::cast_precision_loss, reason = "MaxLen")]
                            let x = i as f64 * cell_width;
                            let _ = writeln!(
                                ops,
                                "{} {} m {} {} l",
                                fmt(x),
                                fmt(border.width),
                                fmt(x),
                                fmt(border.width + height)
                            );
                        }
                    }
                }
            }
            ops.push_str("S\n");
        }
        ops.push_str("Q\n");
        Ok(())
    }

    /// Adds a form `XObject` (`{Type: :XObject, Subtype: :Form, BBox:, Matrix:}`), or
    /// replaces the object `existing`, with the given content and resources.
    fn put_form(
        &mut self,
        existing: Option<ObjectId>,
        bbox: (f64, f64),
        matrix: Option<[f64; 6]>,
        resources: Option<Dictionary>,
        ops: String,
    ) -> ObjectId {
        let mut dict = Dictionary::new();
        dict.set("Type", Object::Name(b"XObject".to_vec()));
        dict.set("Subtype", Object::Name(b"Form".to_vec()));
        dict.set(
            "BBox",
            Object::Array(vec![0.into(), 0.into(), real(bbox.0), real(bbox.1)]),
        );
        if let Some(matrix) = matrix {
            dict.set(
                "Matrix",
                Object::Array(matrix.iter().map(|v| real(*v)).collect()),
            );
        }
        if let Some(resources) = resources {
            dict.set("Resources", Object::Dictionary(resources));
        }
        let stream = Object::Stream(Stream::new(dict, ops.into_bytes()));
        match existing {
            Some(id) => {
                self.doc.objects.insert(id, stream);
                self.dirty.insert(id);
                id
            }
            None => self.add(stream),
        }
    }

    /// `create_check_box_appearances` (and `create_radio_button_appearances`).
    fn create_check_box_appearances(&mut self, target: &Target) -> Result<(), EditError> {
        let widget = target.widget;
        let value = self
            .inherited_value(target.field, b"V")
            .filter(|v| v.as_name().ok() != Some(b"Off"));
        let value_name = match &value {
            None => None,
            Some(Object::Name(n)) => Some(n.clone()),
            Some(Object::String(s, _)) => Some(s.clone()),
            Some(_) => {
                return Err(EditError::Unsupported(
                    "button field values that are not names",
                ));
            }
        };
        // The normal appearance: a dictionary of states, which may be an indirect object.
        let normal = self.normal_appearance(widget);
        let normal_id = normal.as_ref().and_then(|n| n.as_reference().ok());
        let mut states =
            if let Some(Object::Dictionary(states)) = normal.as_ref().map(|n| self.resolve(n)) {
                states.clone()
            } else {
                let mut states = Dictionary::new();
                states.set("Off", Object::Null);
                states.set(
                    value_name.clone().unwrap_or_else(|| b"Yes".to_vec()),
                    Object::Null,
                );
                states
            };
        let replaced = !matches!(
            normal.as_ref().map(|n| self.resolve(n)),
            Some(Object::Dictionary(_))
        );
        let on_name =
            if let Some(name) = states.iter().map(|(k, _)| k.clone()).find(|k| k != b"Off") {
                name
            } else {
                let name = value_name.clone().unwrap_or_else(|| b"Yes".to_vec());
                states.set(name.clone(), Object::Null);
                name
            };
        let field_v = self
            .inherited_value(target.field, b"V")
            .and_then(|v| v.as_name().ok().map(<[u8]>::to_vec));
        let state = if field_v.as_deref() == Some(on_name.as_slice()) {
            on_name.clone()
        } else {
            b"Off".to_vec()
        };
        self.set_widget_flags(widget, Some(&state));

        let border = self.border_style(widget)?;
        let marker = self.marker_style(target)?;
        let radio = self.flags(target.field) & RADIO != 0;
        let circular = radio && marker.style == "circle";
        let mut rect = self.widget_rect(widget);
        if rect[2] - rect[0] == 0.0 {
            rect[2] = rect[0] + DEFAULT_FONT_SIZE + 2.0 * border.width;
        }
        if rect[3] - rect[1] == 0.0 {
            rect[3] = rect[1] + DEFAULT_FONT_SIZE + 2.0 * border.width;
        }
        self.set_rect(widget, rect);
        let (width, height, matrix) =
            self.widget_rotation(widget, rect[2] - rect[0], rect[3] - rect[1]);

        let mut off_ops = String::new();
        self.background_and_border(target, &border, None, circular, &mut off_ops)?;
        let off = self.put_form(None, (width, height), matrix, None, off_ops);

        let mut on_ops = String::new();
        self.background_and_border(target, &border, None, circular, &mut on_ops)?;
        on_ops.push_str("q\n");
        let mut resources = None;
        if radio && marker.style == "circle" {
            let _ = writeln!(on_ops, "{}", marker.color.op(false));
            circle(
                &mut on_ops,
                width / 2.0,
                height / 2.0,
                ((width / 2.0).min(height / 2.0) - border.width) / 2.0,
            );
            on_ops.push_str("f\n");
        } else if marker.style == "cross" {
            let bw = border.width;
            let _ = writeln!(
                on_ops,
                "{}\n{} {} m {} {} l\n{} {} m {} {} l\nS",
                marker.color.op(true),
                fmt(bw),
                fmt(bw),
                fmt(width - bw),
                fmt(height - bw),
                fmt(bw),
                fmt(height - bw),
                fmt(width - bw),
                fmt(bw)
            );
        } else {
            let mark = marker.mark.chars().next().unwrap_or('4');
            let code = u8::try_from(u32::from(mark))
                .map_err(|_| EditError::Unsupported("check box markers outside ASCII"))?;
            let (font, key) = self.font("ZapfDingbats", "none")?;
            let FontData::Type1(afm) = font.as_ref() else {
                return Err(EditError::Font("ZapfDingbats".to_owned()));
            };
            let (glyph, (y_min, y_max)) = afm.builtin_glyph(code).ok_or(EditError::Unsupported(
                "check box markers ZapfDingbats has no glyph for",
            ))?;
            let square = width.min(height) - 2.0 * border.width;
            let size = if marker.size == 0.0 {
                square
            } else {
                marker.size
            };
            let mark_width = glyph.width * size / 1000.0;
            let mark_height = (y_max - y_min) * size / 1000.0;
            let x = (width - square) / 2.0 + (square - mark_width) / 2.0;
            let y = (height - square) / 2.0 + (square - mark_height) / 2.0 - y_min * size / 1000.0;
            let mut form_resources = FormResources {
                fonts: &mut self.fonts,
                name_base: self.name_base,
                used: BTreeSet::new(),
            };
            let encoded = form_resources.encode(&key, &[glyph]);
            let used: Vec<(String, ObjectId)> = form_resources
                .used
                .iter()
                .map(|i| {
                    (
                        format!("DSF{}", self.name_base + i),
                        self.fonts[*i].object_id,
                    )
                })
                .collect();
            let mut text = Vec::new();
            super::writer::write_object(&mut text, &encoded);
            let _ = writeln!(
                on_ops,
                "BT\n/{key} {} Tf\n{}\n{} {} Td\n{} Tj\nET",
                fmt(size),
                marker.color.op(false),
                fmt(x),
                fmt(y),
                String::from_utf8_lossy(&text)
            );
            resources = Some(self.form_font_resources(None, &used));
        }
        on_ops.push_str("Q\n");
        let on = self.put_form(None, (width, height), matrix, resources, on_ops);

        states.set("Off", Object::Reference(off));
        states.set(on_name, Object::Reference(on));
        if let (Some(id), false) = (normal_id, replaced) {
            self.doc.objects.insert(id, Object::Dictionary(states));
            self.dirty.insert(id);
        } else {
            let mut ap = self
                .dict(widget)
                .and_then(|d| d.get(b"AP").ok())
                .and_then(|a| self.resolve(a).as_dict().ok())
                .cloned()
                .unwrap_or_default();
            ap.set("N", Object::Dictionary(states));
            self.set_appearance_dict(widget, ap);
        }
        Ok(())
    }

    fn set_appearance_dict(&mut self, widget: ObjectId, ap: Dictionary) {
        let existing = self
            .dict(widget)
            .and_then(|d| d.get(b"AP").ok())
            .and_then(|a| a.as_reference().ok());
        match existing {
            Some(id) => {
                self.doc.objects.insert(id, Object::Dictionary(ap));
                self.dirty.insert(id);
            }
            None => {
                if let Some(dict) = self.dict_mut(widget) {
                    dict.set("AP", Object::Dictionary(ap));
                }
            }
        }
    }

    /// A form's resources: the form's default resources (`deep_copy`) and the fonts drawn.
    fn form_font_resources(
        &self,
        base: Option<Dictionary>,
        used: &[(String, ObjectId)],
    ) -> Dictionary {
        let mut resources = base.unwrap_or_default();
        if used.is_empty() {
            return resources;
        }
        let mut fonts = resources
            .get(b"Font")
            .ok()
            .and_then(|f| self.resolve(f).as_dict().ok())
            .cloned()
            .unwrap_or_default();
        for (key, id) in used {
            fonts.set(key.clone(), Object::Reference(*id));
        }
        resources.set("Font", Object::Dictionary(fonts));
        resources
    }

    /// `widget.marker_style`.
    fn marker_style(&self, target: &Target) -> Result<Marker, EditError> {
        let check_box = self.flags(target.field) & (RADIO | PUSH_BUTTON) == 0;
        let ca = self
            .mk(target.widget)
            .and_then(|mk| mk.get(b"CA").ok().cloned())
            .and_then(|c| lopdf::decode_text_string(self.resolve(&c)).ok());
        let style = match ca.as_deref() {
            Some("4") => "check".to_owned(),
            Some("l") => "circle".to_owned(),
            Some("8") => "cross".to_owned(),
            Some("u") => "diamond".to_owned(),
            Some("n") => "square".to_owned(),
            Some("H") => "star".to_owned(),
            Some(other) => format!("text:{other}"),
            None if check_box => "check".to_owned(),
            None => "circle".to_owned(),
        };
        let mut size = 0.0;
        let mut color = Color(vec![0.0]);
        let da = self
            .dict(target.widget)
            .and_then(|d| d.get(b"DA").ok())
            .or_else(|| self.dict(target.field).and_then(|d| d.get(b"DA").ok()))
            .and_then(|d| self.resolve(d).as_str().ok().map(<[u8]>::to_vec));
        if let Some(da) = da {
            let parsed = parse_appearance(&da)?;
            size = parsed.size.unwrap_or(size);
            color = parsed.color.unwrap_or(color);
        }
        Ok(Marker {
            style,
            size,
            color,
            mark: ca
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "4".to_owned()),
        })
    }

    /// `parse_default_appearance_string(widget)`: the widget's, the field's (inherited) or
    /// the form's `DA`. Without one, HexaPDF sets Helvetica at auto size.
    fn default_appearance(&self, target: &Target) -> Result<Appearance, EditError> {
        let da = self
            .dict(target.widget)
            .and_then(|d| d.get(b"DA").ok())
            .map(|d| self.resolve(d).clone())
            .or_else(|| self.inherited_value(target.field, b"DA"))
            .or_else(|| {
                self.acro_form_dict()
                    .and_then(|f| f.get(b"DA").ok())
                    .map(|d| self.resolve(d).clone())
            });
        match da.as_ref().and_then(|d| d.as_str().ok()) {
            Some(da) => parse_appearance(da),
            None => Ok(Appearance {
                font: None,
                size: Some(0.0),
                color: Some(Color(vec![0.0])),
            }),
        }
    }

    /// `retrieve_font_information(font_name, default_resources)`.
    fn field_font(&self, name: Option<&[u8]>) -> Result<FieldFont, EditError> {
        let font = name.and_then(|name| {
            let dr = self
                .acro_form_dict()?
                .get(b"DR")
                .ok()
                .and_then(|r| self.resolve(r).as_dict().ok())?;
            let fonts = self.resolve(dr.get(b"Font").ok()?).as_dict().ok()?;
            self.resolve(fonts.get(name).ok()?).as_dict().ok().cloned()
        });
        let subtype = font
            .as_ref()
            .and_then(|f| f.get(b"Subtype").ok())
            .and_then(|s| s.as_name().ok());
        let base = font
            .as_ref()
            .and_then(|f| f.get(b"BaseFont").ok())
            .and_then(|b| b.as_name().ok());
        let encoding_of = |dict: &Dictionary| -> HashSet<char> {
            let mut chars = HashSet::new();
            if let Ok(encoding) = dict.get_font_encoding(&self.doc) {
                for code in 0..=255_u8 {
                    if let Ok(text) = encoding.bytes_to_string(&[code]) {
                        chars.extend(text.chars());
                    }
                }
            }
            chars
        };
        if subtype == Some(b"Type1") && matches!(base, Some(b"Symbol" | b"ZapfDingbats")) {
            return Err(EditError::Unsupported("form field text in symbol fonts"));
        }
        match (subtype, base.and_then(standard_font)) {
            (Some(b"Type1"), Some((family, variant))) => {
                let dict = font.clone().unwrap_or_default();
                Ok(FieldFont {
                    family,
                    variant,
                    chars: encoding_of(&dict),
                })
            }
            (Some(b"TrueType"), _) => {
                let embedded = font
                    .as_ref()
                    .and_then(|f| f.get(b"FontDescriptor").ok())
                    .and_then(|d| self.resolve(d).as_dict().ok())
                    .is_some_and(|d| d.has(b"FontFile2"));
                let subset = base.is_some_and(|b| {
                    b.len() > 7 && b[6] == b'+' && b[..6].iter().all(u8::is_ascii_uppercase)
                });
                if embedded && !subset {
                    return Err(EditError::Unsupported(
                        "form field text in embedded TrueType fonts",
                    ));
                }
                Ok(self.fallback_font())
            }
            _ => Ok(self.fallback_font()),
        }
    }

    /// `acro_form.fallback_font`: Helvetica with the `WinAnsi` encoding.
    fn fallback_font(&self) -> FieldFont {
        let mut dict = Dictionary::new();
        dict.set("Type", Object::Name(b"Font".to_vec()));
        dict.set("Encoding", Object::Name(b"WinAnsiEncoding".to_vec()));
        let mut chars = HashSet::new();
        if let Ok(encoding) = dict.get_font_encoding(&self.doc) {
            for code in 0..=255_u8 {
                if let Ok(text) = encoding.bytes_to_string(&[code]) {
                    chars.extend(text.chars());
                }
            }
        }
        FieldFont {
            family: "Helvetica",
            variant: "none",
            chars,
        }
    }

    /// `create_text_appearances` (also for combo and list boxes).
    fn create_text_appearances(&mut self, target: &Target) -> Result<(), EditError> {
        let field = target.field;
        let widget = target.widget;
        let is_choice = self
            .inherited_value(field, b"FT")
            .is_some_and(|t| t.as_name().ok() == Some(b"Ch"));
        let flags = self.flags(field);
        let list_box = is_choice && flags & COMBO == 0;
        let multiline = !is_choice && flags & MULTILINE != 0;
        let comb = !is_choice
            && flags & MULTILINE == 0
            && flags & PASSWORD == 0
            && flags & FILE_SELECT == 0
            && flags & COMB != 0;
        let max_len = self
            .inherited_value(field, b"MaxLen")
            .and_then(|m| m.as_i64().ok());

        let appearance = self.default_appearance(target)?;
        let field_font = self.field_font(appearance.font.as_deref())?;
        let (font, key) = self.font(field_font.family, field_font.variant)?;
        let FontData::Type1(afm) = font.as_ref() else {
            return Err(EditError::Font(field_font.family.to_owned()));
        };
        let mut font_size = appearance.size.unwrap_or(0.0);
        let mut fill_color = appearance.color.as_ref().map(Color::rgb).transpose()?;
        let border = self.border_style(widget)?;
        let padding = border.width.max(1.0);
        self.set_widget_flags(widget, Some(b"N"));

        let mut rect = self.widget_rect(widget);
        if rect[2] - rect[0] == 0.0 {
            rect[2] = rect[0] + DEFAULT_WIDTH;
        }
        if rect[3] - rect[1] == 0.0 {
            font_size = if font_size == 0.0 {
                DEFAULT_FONT_SIZE
            } else {
                font_size
            };
            rect[3] = rect[1]
                + (afm_y(afm, true) - afm_y(afm, false)) * font_size / 1000.0
                + 2.0 * padding;
        }
        self.set_rect(widget, rect);
        let (width, height, matrix) =
            self.widget_rotation(widget, rect[2] - rect[0], rect[3] - rect[1]);

        // `(@widget[:AP] ||= {})[:N] ||= @document.add(...)`.
        let existing = match self.normal_appearance(widget) {
            None => None,
            Some(Object::Reference(id)) => Some(id),
            Some(_) => {
                return Err(EditError::Unsupported(
                    "text field appearances that are not streams",
                ));
            }
        };
        let default_resources = self
            .acro_form_dict()
            .and_then(|f| f.get(b"DR").ok())
            .and_then(|r| self.resolve(r).as_dict().ok())
            .cloned();

        let mut ops = String::new();
        self.background_and_border(
            target,
            &border,
            if comb { max_len } else { None },
            false,
            &mut ops,
        )?;
        ops.push_str("/Tx BMC\n");
        let text_value = if is_choice {
            match self.choice_value(field) {
                Some(values) if values.len() > 1 && !list_box => {
                    return Err(EditError::Unsupported("combo boxes with several values"));
                }
                values => values.map(|v| v.join(", ")),
            }
        } else {
            self.text_value(field)
        };
        let alignment = match self
            .inherited_value(field, b"Q")
            .and_then(|q| q.as_i64().ok())
            .unwrap_or(0)
        {
            1 => Align::Center,
            2 => Align::Right,
            _ => Align::Left,
        };
        let mut used = Vec::new();
        if text_value.is_some() || list_box {
            let _ = writeln!(
                ops,
                "q\n{} {} {} {} re W n",
                fmt(padding),
                fmt(padding),
                fmt(width - 2.0 * padding),
                fmt(height - 2.0 * padding)
            );
            let style = |size: f64, color: Option<[f64; 3]>| {
                Arc::new(TextStyle {
                    font: font.clone(),
                    font_key: key.clone(),
                    font_size: size,
                    line_height: None,
                    fill_color: color,
                    white_underlay: false,
                    underline: false,
                    link: None,
                })
            };
            let check = |text: &str, breaks: bool| -> Result<(), EditError> {
                for c in text.chars() {
                    if breaks && matches!(c, '\n' | '\r') {
                        continue;
                    }
                    if !field_font.chars.contains(&c) || !afm.has_glyph_for(c) {
                        return Err(EditError::Unsupported(UNSUPPORTED_TEXT));
                    }
                }
                Ok(())
            };
            let fitted;
            let (x, y);
            if multiline {
                let text = text_value.clone().unwrap_or_default();
                check(&text, true)?;
                let layouter = Layouter::new(alignment, Valign::Top).with_line_spacing(1.25);
                let fit = |size: f64| {
                    let s = style(size, fill_color);
                    let available = if font_size == 0.0 {
                        height - 4.0 * padding
                    } else {
                        2f64.powi(20)
                    };
                    layouter.fit(
                        vec![Input::Text(Fragment::new(&text, &s))],
                        width - 4.0 * padding,
                        available,
                        &s,
                    )
                };
                let result = if font_size == 0.0 {
                    let mut size = 12.0;
                    loop {
                        let result = fit(size);
                        if matches!(result.status, super::layout::Status::Success) || size <= 4.0 {
                            break result;
                        }
                        size -= 1.0;
                    }
                } else {
                    fit(font_size)
                };
                x = 2.0 * padding;
                y = height - 2.0 * padding - result.lines.first().map_or(0.0, Line::height) / 2.0;
                fitted = result;
            } else if list_box {
                let size = if font_size == 0.0 { 12.0 } else { font_size };
                let items = self.option_items(field);
                let top = usize::try_from(
                    self.dict(field)
                        .and_then(|d| d.get(b"TI").ok())
                        .and_then(|t| t.as_i64().ok())
                        .unwrap_or(0),
                )
                .unwrap_or(0);
                let text = items.get(top..).unwrap_or_default().join("\n");
                check(&text, true)?;
                let s = style(size, fill_color);
                let layouter = Layouter::new(alignment, Valign::Top).with_line_spacing(1.25);
                let result = layouter.fit(
                    vec![Input::Text(Fragment::new(&text, &s))],
                    width - 4.0 * padding,
                    height,
                    &s,
                );
                let (top_gap, line_height) = match result.lines.first() {
                    Some(line) => {
                        let distance = line.y_min().abs() + line.y_max();
                        (distance * 0.25, distance * 1.25)
                    }
                    None => (0.0, 0.0),
                };
                if !result.lines.is_empty() {
                    let mut selected = Vec::new();
                    for value in self.choice_value(field).unwrap_or_default() {
                        let index = items.iter().position(|i| *i == value).ok_or(
                            EditError::Unsupported("list box values that are not options"),
                        )?;
                        selected.push(index);
                    }
                    let mut any = false;
                    for index in selected {
                        #[allow(clippy::cast_precision_loss, reason = "option indices")]
                        let y = height - padding - (index as f64 - top as f64 + 1.0) * line_height;
                        if y + line_height > height || y + line_height < padding {
                            continue;
                        }
                        if !any {
                            ops.push_str("0.6 0.756863 0.854902 rg\n");
                            any = true;
                        }
                        let _ = writeln!(
                            ops,
                            "{} {} {} {} re",
                            fmt(padding),
                            fmt(y),
                            fmt(width - 2.0 * padding),
                            fmt(line_height)
                        );
                    }
                    if any {
                        ops.push_str("f\n");
                    }
                }
                x = 2.0 * padding;
                y = height - padding - top_gap;
                fitted = result;
            } else {
                let raw = text_value.clone().unwrap_or_default();
                let (value, text_color) = self.apply_format(field, raw)?;
                if let Some(color) = text_color {
                    fill_color = Some(color);
                }
                check(&value, false)?;
                let mut size = font_size;
                if size == 0.0 {
                    // `calculate_and_apply_font_size`.
                    let unit = (afm.bbox[3] - afm.bbox[1]) / 1000.0;
                    size = (height - 2.0 * padding) / unit * 0.85;
                    let calc_width = Fragment::new(&value, &style(size, None)).width();
                    let available = width - 4.0 * padding;
                    if calc_width == 0.0 {
                        if available <= 0.0 {
                            return Err(EditError::Unsupported("auto-sized text in empty boxes"));
                        }
                    } else {
                        size = size.min(size * available / calc_width);
                    }
                }
                let s = style(size, fill_color);
                let mut fragment = Fragment::new(&value, &s);
                let line_width;
                if comb && !value.is_empty() {
                    let max_len =
                        max_len.ok_or(EditError::Unsupported("comb fields without MaxLen"))?;
                    #[allow(clippy::cast_precision_loss, reason = "MaxLen")]
                    let cell_width = width / max_len as f64;
                    let scaled_cell_width = cell_width / (size / 1000.0);
                    let glyphs: Vec<Glyph> = fragment
                        .items
                        .iter()
                        .filter_map(|i| match i {
                            Item::Glyph(g) => Some(g.clone()),
                            Item::Adjust(_) => None,
                        })
                        .collect();
                    let mut items = Vec::new();
                    for pair in glyphs.windows(2) {
                        items.push(Item::Glyph(pair[0].clone()));
                        items.push(Item::Adjust(
                            -(scaled_cell_width - pair[0].width / 2.0 - pair[1].width / 2.0),
                        ));
                    }
                    if let Some(last) = glyphs.last() {
                        items.push(Item::Glyph(last.clone()));
                    }
                    fragment.items = items;
                    let first_width = glyphs.first().map_or(0.0, |g| g.width * size / 1000.0);
                    let x_offset = 1.0 + (cell_width - first_width) / 2.0;
                    let length = i64::try_from(value.chars().count()).unwrap_or(0);
                    #[allow(clippy::cast_precision_loss, reason = "MaxLen")]
                    let comb_x = match alignment {
                        Align::Right => x_offset + cell_width * (max_len - length) as f64,
                        Align::Center => {
                            x_offset + cell_width * (max_len - length).div_euclid(2) as f64
                        }
                        _ => x_offset,
                    };
                    x = comb_x;
                    line_width = 0.0;
                } else {
                    line_width = fragment.width();
                    x = match alignment {
                        Align::Right => (width - 2.0 * padding - line_width).max(2.0 * padding),
                        Align::Center => ((width - line_width) / 2.0).max(2.0 * padding),
                        _ => 2.0 * padding,
                    };
                }
                let _ = line_width;
                let cap_height = afm.cap_height / 1000.0 * size;
                let mut baseline = padding + (height - 2.0 * padding - cap_height) / 2.0;
                if baseline < 0.0 {
                    baseline = padding - afm_y(afm, false) * size / 1000.0;
                }
                y = baseline;
                fitted = Fitted {
                    status: super::layout::Status::Success,
                    lines: vec![Line::of(vec![fragment])],
                    remaining: Vec::new(),
                };
            }
            let mut canvas = Canvas::default();
            let mut form_resources = FormResources {
                fonts: &mut self.fonts,
                name_base: self.name_base,
                used: BTreeSet::new(),
            };
            canvas.draw(&fitted, x, y, &mut form_resources);
            for index in &form_resources.used {
                used.push((
                    format!("DSF{}", self.name_base + index),
                    self.fonts[*index].object_id,
                ));
            }
            ops.push_str(&String::from_utf8_lossy(&canvas.ops));
            ops.push_str("Q\n");
        }
        ops.push_str("EMC\n");
        let resources = self.form_font_resources(default_resources, &used);
        let form = self.put_form(existing, (width, height), matrix, Some(resources), ops);
        if existing.is_none() {
            let mut ap = self
                .dict(widget)
                .and_then(|d| d.get(b"AP").ok())
                .and_then(|a| self.resolve(a).as_dict().ok())
                .cloned()
                .unwrap_or_default();
            ap.set("N", Object::Reference(form));
            self.set_appearance_dict(widget, ap);
        }
        Ok(())
    }

    /// `JavaScriptActions.apply_format(value, field[:AA][:F])`: the value as the field's
    /// format action shows it, and the text color it asks for.
    fn apply_format(
        &self,
        field: ObjectId,
        value: String,
    ) -> Result<(String, Option<[f64; 3]>), EditError> {
        let action = self
            .dict(field)
            .and_then(|d| d.get(b"AA").ok())
            .and_then(|a| self.resolve(a).as_dict().ok())
            .and_then(|a| a.get(b"F").ok())
            .and_then(|f| self.resolve(f).as_dict().ok());
        let Some(action) = action else {
            return Ok((value, None));
        };
        if action.get(b"S").ok().and_then(|s| s.as_name().ok()) != Some(b"JavaScript") {
            return Ok((value, None));
        }
        let script = match action.get(b"JS").ok().map(|j| self.resolve(j)) {
            Some(Object::Stream(stream)) => String::from_utf8_lossy(
                &stream
                    .decompressed_content()
                    .unwrap_or_else(|_| stream.content.clone()),
            )
            .into_owned(),
            Some(js @ Object::String(..)) => lopdf::decode_text_string(js).unwrap_or_default(),
            _ => return Ok((value, None)),
        };
        javascript::apply_format(&value, &script)
    }
}

/// `style.scaled_y_max` / `scaled_y_min` before scaling: the AFM ascender or descender.
fn afm_y(afm: &super::fonts::Afm, top: bool) -> f64 {
    if top { afm.ascender() } else { afm.descender() }
}

struct Marker {
    style: String,
    size: f64,
    color: Color,
    mark: String,
}

/// `canvas.circle(cx, cy, r)`: four Bézier curves.
fn circle(ops: &mut String, cx: f64, cy: f64, r: f64) {
    arc(ops, cx, cy, r, 0.0, 360.0, false);
    ops.push_str("h\n");
}

/// An arc from `start` to `end` degrees (counterclockwise) as Bézier curves of at most 90°.
fn arc(ops: &mut String, cx: f64, cy: f64, r: f64, start: f64, end: f64, open: bool) {
    let _ = open;
    let segments = ((end - start) / 90.0).ceil().max(1.0);
    let step = (end - start) / segments;
    let point = |angle: f64| {
        let a = angle.to_radians();
        (cx + r * a.cos(), cy + r * a.sin())
    };
    let (x0, y0) = point(start);
    let _ = writeln!(ops, "{} {} m", fmt(x0), fmt(y0));
    let k = 4.0 / 3.0 * (step.to_radians() / 4.0).tan();
    let mut angle = start;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "at most 4"
    )]
    for _ in 0..segments as usize {
        let (a0, a1) = (angle.to_radians(), (angle + step).to_radians());
        let (x1, y1) = (cx + r * a0.cos(), cy + r * a0.sin());
        let (x2, y2) = (cx + r * a1.cos(), cy + r * a1.sin());
        let c1 = (x1 - k * r * a0.sin(), y1 + k * r * a0.cos());
        let c2 = (x2 + k * r * a1.sin(), y2 - k * r * a1.cos());
        let _ = writeln!(
            ops,
            "{} {} {} {} {} {} c",
            fmt(c1.0),
            fmt(c1.1),
            fmt(c2.0),
            fmt(c2.1),
            fmt(x2),
            fmt(y2)
        );
        angle += step;
    }
}

mod javascript {
    //! `JavaScriptActions.apply_format`: `AFNumber_Format` and `AFPercent_Format`.

    use super::EditError;

    pub(super) fn apply_format(
        value: &str,
        script: &str,
    ) -> Result<(String, Option<[f64; 3]>), EditError> {
        if script.starts_with("AFNumber_Format(") {
            number_format(value, script)
        } else if script.starts_with("AFPercent_Format(") {
            Ok((percent_format(value, script), None))
        } else if script.starts_with("AFTime_Format(") {
            Err(EditError::Unsupported("AFTime_Format form fields"))
        } else {
            Ok((value.to_owned(), None))
        }
    }

    /// The arguments of `Name(…)` with an optional `;`, split at top-level commas.
    fn arguments<'a>(script: &'a str, name: &str) -> Option<Vec<&'a str>> {
        let inner = script
            .strip_prefix(name)?
            .strip_prefix('(')?
            .trim_end_matches(';')
            .strip_suffix(')')?;
        let mut parts = Vec::new();
        let mut start = 0;
        let mut quoted = false;
        for (i, c) in inner.char_indices() {
            match c {
                '"' => quoted = !quoted,
                ',' if !quoted => {
                    parts.push(inner[start..i].trim());
                    start = i + 1;
                }
                _ => {}
            }
        }
        parts.push(inner[start..].trim());
        Some(parts)
    }

    fn is_digits(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
    }

    /// `apply_af_number_format`.
    fn number_format(value: &str, script: &str) -> Result<(String, Option<[f64; 3]>), EditError> {
        let parts = arguments(script, "AFNumber_Format");
        let Some([ndec, sep, neg, zero, currency, prepend]) = parts.as_deref() else {
            return Ok((value.to_owned(), None));
        };
        if !is_digits(ndec)
            || !matches!(*sep, "0" | "1" | "2" | "3")
            || !matches!(*neg, "0" | "1" | "2" | "3")
            || *zero != "0"
            || !currency.starts_with('"')
            || !currency.ends_with('"')
            || currency.len() < 2
            || !matches!(*prepend, "true" | "false")
        {
            return Ok((value.to_owned(), None));
        }
        let currency = &currency[1..currency.len() - 1];
        if currency.contains(['\\', '%', '"']) {
            return Err(EditError::Unsupported(
                "number formats with special currency strings",
            ));
        }
        let decimals: usize = ndec.parse().unwrap_or(0);
        let mut number = make_number(value);
        let mut red = false;
        let mut prefix = String::new();
        let mut suffix = String::new();
        if *prepend == "true" {
            prefix.push_str(currency);
        } else {
            suffix.push_str(currency);
        }
        if number < 0.0 {
            number = number.abs();
            match *neg {
                "0" => prefix.insert(0, '-'),
                "1" => red = true,
                "2" => {
                    prefix.insert(0, '(');
                    suffix.push(')');
                }
                _ => {
                    prefix.insert(0, '(');
                    suffix.push(')');
                    red = true;
                }
            }
        }
        let formatted = format!("{prefix}{}{suffix}", fixed(number, decimals));
        let color = if red {
            [1.0, 0.0, 0.0]
        } else {
            [0.0, 0.0, 0.0]
        };
        Ok((format_number(&formatted, sep), Some(color)))
    }

    /// `apply_af_percent_format`.
    fn percent_format(value: &str, script: &str) -> String {
        let parts = arguments(script, "AFPercent_Format");
        let Some([ndec, sep]) = parts.as_deref() else {
            return value.to_owned();
        };
        if !is_digits(ndec) || !matches!(*sep, "0" | "1" | "2" | "3") {
            return value.to_owned();
        }
        let decimals: usize = ndec.parse().unwrap_or(0);
        let formatted = format!("{}%", fixed(make_number(value) * 100.0, decimals));
        format_number(&formatted, sep)
    }

    /// `sprintf("%.Nf", value)`.
    fn fixed(value: f64, decimals: usize) -> String {
        if value.is_nan() {
            "NaN".to_owned()
        } else if value.is_infinite() {
            if value < 0.0 { "-Inf" } else { "Inf" }.to_owned()
        } else {
            format!("{value:.decimals$}")
        }
    }

    /// `af_make_number`: `Inf`/`NaN`, else Ruby's `String#to_f` of the value with commas as
    /// decimal points.
    fn make_number(value: &str) -> f64 {
        let lower = value.to_ascii_lowercase();
        if lower.contains("inf") {
            return if value.starts_with('-') {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
        }
        if lower.contains("nan") {
            return f64::NAN;
        }
        let text = value.replace(',', ".");
        let text = text.trim_start();
        let bytes = text.as_bytes();
        let mut end = 0;
        if matches!(bytes.first(), Some(b'+' | b'-')) {
            end = 1;
        }
        let digits = |mut i: usize| {
            while i < bytes.len()
                && (bytes[i].is_ascii_digit()
                    || (bytes[i] == b'_'
                        && i > 0
                        && bytes[i - 1].is_ascii_digit()
                        && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)))
            {
                i += 1;
            }
            i
        };
        let int_end = digits(end);
        let mut number_end = int_end;
        if bytes.get(int_end) == Some(&b'.')
            && bytes.get(int_end + 1).is_some_and(u8::is_ascii_digit)
        {
            number_end = digits(int_end + 1);
        }
        if number_end > end && matches!(bytes.get(number_end), Some(b'e' | b'E')) {
            let mut e = number_end + 1;
            if matches!(bytes.get(e), Some(b'+' | b'-')) {
                e += 1;
            }
            let e_end = digits(e);
            if e_end > e {
                number_end = e_end;
            }
        }
        text[..number_end].replace('_', "").parse().unwrap_or(0.0)
    }

    /// `af_format_number`'s separators: thousands (`\B(?=(\d\d\d)+(?:[^\d]|\z))`) before
    /// the decimal point, then the decimal point itself (`result.split('.')`, which drops
    /// trailing empty parts).
    fn format_number(formatted: &str, sep: &str) -> String {
        let mut parts: Vec<&str> = formatted.split('.').collect();
        while parts.last().is_some_and(|p| p.is_empty()) {
            parts.pop();
        }
        let mut before = parts.first().copied().unwrap_or_default().to_owned();
        if sep == "0" || sep == "2" {
            let separator = if sep == "0" { ',' } else { '.' };
            before = thousands(&before, separator);
        }
        match parts.get(1) {
            Some(after) => {
                let point = if sep <= "1" { '.' } else { ',' };
                format!("{before}{point}{after}")
            }
            None => before,
        }
    }

    fn thousands(text: &str, separator: char) -> String {
        let chars: Vec<char> = text.chars().collect();
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let mut out = String::new();
        for (i, c) in chars.iter().enumerate() {
            if i > 0 && word(chars[i - 1]) == word(*c) {
                // `(?=(\d\d\d)+(?:[^\d]|\z))`
                let digits = chars[i..].iter().take_while(|c| c.is_ascii_digit()).count();
                if digits > 0 && digits % 3 == 0 {
                    out.push(separator);
                }
            }
            out.push(*c);
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn formats_numbers_like_hexapdf() {
            let f = |v: &str, s: &str| apply_format(v, s).unwrap();
            assert_eq!(
                f("-1234.5", "AFNumber_Format(2, 0, 3, 0, \"$\", true);"),
                ("($1,234.50)".to_owned(), Some([1.0, 0.0, 0.0]))
            );
            assert_eq!(
                f(
                    "1234567,891",
                    "AFNumber_Format(1, 2, 0, 0, \" EUR\", false);"
                ),
                ("1.234.567,9 EUR".to_owned(), Some([0.0; 3]))
            );
            assert_eq!(f("0.125", "AFPercent_Format(1, 0);").0, "12.5%");
            assert_eq!(f("abc", "AFNumber_Format(0, 1, 0, 0, \"\", true);").0, "0");
            assert_eq!(f("12", "other();").0, "12");
        }

        #[test]
        fn parses_numbers_like_ruby() {
            assert!((make_number(" 12abc") - 12.0).abs() < 1e-9);
            assert!((make_number("1_000.5") - 1000.5).abs() < 1e-9);
            assert!((make_number("1e3") - 1000.0).abs() < 1e-9);
            assert_eq!(make_number("x"), 0.0);
        }
    }
}
