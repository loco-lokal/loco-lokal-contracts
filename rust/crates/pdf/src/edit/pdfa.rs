//! The `:pdfa` task (`HexaPDF::Task::PDFA`) with the metadata HexaPDF writes alongside it
//! (`HexaPDF::Document::Metadata`). Both run when the document is saved
//! (`:complete_objects`):
//!
//! - the XMP packet is built from the information dictionary, in HexaPDF's namespace and
//!   property order, with the `pdfaid` part and conformance added;
//! - the information dictionary is rewritten from it (dates in HexaPDF's format);
//! - the sRGB output intent is added unless the catalog has one; and
//! - for loaded documents, the embedded TrueType programs of the CID fonts the pages use are
//!   rebuilt with their `hmtx` widths corrected to the PDF widths (`fix_glyph_widths`).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, TimeZone as _};
use lopdf::{Dictionary, Object, ObjectId, Stream, StringFormat, content::Content};

use super::{EditError, Editor, PRODUCER, fonts::deflate, number};

const SRGB_ICC: &[u8] = include_bytes!("../../../../data/icc/sRGB2014.icc");

/// `pdf.task(:pdfa, level:)`.
#[derive(Clone, Copy, Debug)]
pub struct Pdfa {
    /// The conformance letter of the level (`'B'` for `3b`, `'U'` for `3u`).
    pub conformance: char,
    /// `fixes: :default` on a loaded document also fixes glyph widths; a new one has none.
    pub fix_glyph_widths: bool,
}

#[derive(Clone, Debug)]
enum Value {
    Text(String),
    Date(DateTime<FixedOffset>),
    Bool(bool),
}

/// `@metadata`: namespaces with their properties, both in insertion order.
#[derive(Default)]
struct Metadata {
    namespaces: Vec<(&'static str, Vec<(&'static str, Value)>)>,
}

impl Metadata {
    fn set(&mut self, prefix: &'static str, name: &'static str, value: Value) {
        let index = if let Some(index) = self.namespaces.iter().position(|(p, _)| *p == prefix) {
            index
        } else {
            self.namespaces.push((prefix, Vec::new()));
            self.namespaces.len() - 1
        };
        let values = &mut self.namespaces[index].1;
        match values.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = value,
            None => values.push((name, value)),
        }
    }

    fn get(&self, prefix: &str, name: &str) -> Option<&Value> {
        self.namespaces
            .iter()
            .find(|(p, _)| *p == prefix)?
            .1
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v)
    }

    /// `xmp_packet(descriptions)`.
    fn packet(&self, language: &str) -> String {
        let descriptions: Vec<String> = self
            .namespaces
            .iter()
            .filter(|(_, values)| !values.is_empty())
            .map(|(prefix, values)| description(prefix, values, language))
            .collect();
        format!(
            "<?xpacket begin=\"\u{feff}\" id=\"{}\"?>\n\
             <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
             {}\n\
             </rdf:RDF>\n\
             </x:xmpmeta>\n\
             <?xpacket end=\"r\"?>\n",
            uuid::Uuid::new_v4().simple(),
            descriptions.join("\n")
        )
    }
}

/// `PREDEFINED_NAMESPACES`.
fn namespace(prefix: &str) -> &'static str {
    match prefix {
        "xmp" => "http://ns.adobe.com/xap/1.0/",
        "pdf" => "http://ns.adobe.com/pdf/1.3/",
        "dc" => "http://purl.org/dc/elements/1.1/",
        _ => "http://www.aiim.org/pdfa/ns/id/",
    }
}

/// `xmp_description(ns_prefix, values)`.
fn description(prefix: &str, values: &[(&str, Value)], language: &str) -> String {
    let values: Vec<String> = values
        .iter()
        .map(|(name, value)| {
            let inner = match (*name, value) {
                ("title" | "description", Value::Text(text)) => format!(
                    "<rdf:Alt>\n<rdf:li xml:lang=\"{language}\">{}</rdf:li>\n</rdf:Alt>",
                    escape(text)
                ),
                ("creator", Value::Text(text)) => {
                    format!("<rdf:Seq>\n<rdf:li>{}</rdf:li>\n</rdf:Seq>", escape(text))
                }
                (_, Value::Text(text)) => escape(text),
                (_, Value::Date(date)) => date.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
                (_, Value::Bool(value)) => if *value { "True" } else { "False" }.to_owned(),
            };
            format!("<{prefix}:{name}>{inner}</{prefix}:{name}>")
        })
        .collect();
    format!(
        "<rdf:Description rdf:about=\"\" xmlns:{prefix}=\"{}\">\n{}\n</rdf:Description>",
        escape(namespace(prefix)),
        values.join("\n")
    )
}

/// `xmp_escape`.
fn escape(value: &str) -> String {
    value
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `HexaPDF::DictionaryFields::DateConverter.convert`: a PDF date string as a time
/// (`DATE_RE`, then `Time.new`, clamping the fields when it raises).
#[must_use]
pub fn parse_pdf_date(bytes: &[u8]) -> Option<DateTime<FixedOffset>> {
    let text = std::str::from_utf8(bytes).ok()?.strip_prefix("D:")?;
    let leading_digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
    if leading_digits(text) < 4 {
        return None;
    }
    let year: i32 = text[..4].parse().ok()?;
    let mut rest = &text[4..];
    // `(\d\d)?` five times.
    let mut parts: [Option<u32>; 5] = [None; 5];
    for part in &mut parts {
        if leading_digits(rest) < 2 {
            break;
        }
        *part = Some(rest[..2].parse().ok()?);
        rest = &rest[2..];
    }
    // `([Z+-])?(?:(\d+)(?:'|'(\d+)'?'?|\z)?)?\z`
    let sign = rest.chars().next().filter(|c| matches!(c, 'Z' | '+' | '-'));
    if sign.is_some() {
        rest = &rest[1..];
    }
    let (mut hours, mut minutes) = (0_i64, 0_i64);
    let hours_len = leading_digits(rest);
    if hours_len > 0 {
        hours = rest[..hours_len].parse().unwrap_or(i64::MAX / 7200);
        rest = &rest[hours_len..];
        if let Some(tail) = rest.strip_prefix('\'') {
            let len = leading_digits(tail);
            if len > 0 {
                minutes = tail[..len].parse().unwrap_or(0);
                rest = &tail[len..];
                rest = rest.strip_prefix('\'').unwrap_or(rest);
                rest = rest.strip_prefix('\'').unwrap_or(rest);
            } else {
                rest = tail;
            }
        }
    }
    if !rest.is_empty() {
        return None;
    }
    let offset = match sign {
        Some('+') => (hours * 3600 + minutes * 60).clamp(0, 86399),
        Some('-') => -(hours * 3600 + minutes * 60).clamp(0, 86399),
        _ => 0,
    };
    let offset = FixedOffset::east_opt(i32::try_from(offset).ok()?)?;
    let [month, day, hour, minute, second] = parts;
    let (mut month, mut day) = (month.unwrap_or(1), day.unwrap_or(1));
    let (mut hour, mut minute, mut second) =
        (hour.unwrap_or(0), minute.unwrap_or(0), second.unwrap_or(0));
    // `Time.new` raises for these (days past the month's end up to 31 roll over).
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 24
        || (hour == 24 && (minute, second) != (0, 0))
        || minute > 59
        || second > 60
    {
        month = month.clamp(1, 12);
        day = day.clamp(1, 31);
        hour = hour.min(23);
        minute = minute.min(59);
        second = second.min(59);
    }
    let naive = NaiveDate::from_ymd_opt(year, month, 1)?.and_hms_opt(0, 0, 0)?
        + Duration::days(i64::from(day) - 1)
        + Duration::hours(i64::from(hour))
        + Duration::minutes(i64::from(minute))
        + Duration::seconds(i64::from(second));
    offset.from_local_datetime(&naive).single()
}

/// `Serializer#serialize_time`.
fn pdf_date(date: &DateTime<FixedOffset>) -> Object {
    let seconds = date.offset().local_minus_utc();
    let zone = if seconds == 0 {
        String::new()
    } else {
        let sign = if seconds < 0 { '-' } else { '+' };
        let seconds = seconds.abs();
        format!("{sign}{:02}'{:02}'", seconds / 3600, seconds % 3600 / 60)
    };
    Object::String(
        format!("{}{zone}", date.format("D:%Y%m%d%H%M%S")).into_bytes(),
        StringFormat::Literal,
    )
}

/// `Serializer#serialize_string` for a UTF-8 text string: UTF-16BE with a byte order mark
/// unless it is printable ASCII.
fn text_string(text: &str) -> Object {
    if text
        .chars()
        .all(|c| matches!(c, ' '..='~' | '\t' | '\r' | '\n'))
    {
        return Object::String(text.as_bytes().to_vec(), StringFormat::Literal);
    }
    let mut bytes = vec![0xFE, 0xFF];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    Object::String(bytes, StringFormat::Literal)
}

impl Editor {
    /// Runs the `:pdfa` task when the document is next saved.
    pub fn set_pdfa(&mut self, pdfa: Pdfa) {
        self.pdfa = Some(pdfa);
    }

    /// The `:complete_objects` listeners of the `:pdfa` task and of `doc.metadata`, after
    /// `ModDate` was set (`write(update_fields: true)`).
    pub(super) fn apply_pdfa(&mut self, pdfa: Pdfa) -> Result<(), EditError> {
        let info_id = self.info_id();
        let info = self.dict(info_id).cloned().unwrap_or_default();
        let mut metadata = Metadata::default();

        // `parse_metadata`
        let text = |key: &[u8]| -> Option<String> {
            let value = self.resolve(info.get(key).ok()?);
            match value {
                Object::String(..) => lopdf::decode_text_string(value).ok(),
                Object::Integer(v) => Some(v.to_string()),
                Object::Real(v) => Some(v.to_string()),
                _ => None,
            }
        };
        let date = |key: &[u8]| -> Option<DateTime<FixedOffset>> {
            parse_pdf_date(self.resolve(info.get(key).ok()?).as_str().ok()?)
        };
        for (key, prefix, name) in [
            (&b"Title"[..], "dc", "title"),
            (b"Author", "dc", "creator"),
            (b"Subject", "dc", "description"),
            (b"Creator", "xmp", "CreatorTool"),
        ] {
            if let Some(value) = text(key) {
                metadata.set(prefix, name, Value::Text(value));
            }
        }
        for (key, name) in [
            (&b"CreationDate"[..], "CreateDate"),
            (b"ModDate", "ModifyDate"),
        ] {
            if let Some(value) = date(key) {
                metadata.set("xmp", name, Value::Date(value));
            }
        }
        for (key, name) in [(&b"Keywords"[..], "Keywords"), (b"Producer", "Producer")] {
            if let Some(value) = text(key) {
                metadata.set("pdf", name, Value::Text(value));
            }
        }
        if let Ok(trapped) = info.get(b"Trapped").map(|t| self.resolve(t))
            && trapped.as_name().ok() != Some(b"Unknown")
        {
            // `info_dict[:Trapped] == :True`: a boolean `true` is not the name.
            let value = matches!(trapped, Object::Name(n) if n == b"True");
            metadata.set("pdf", "Trapped", Value::Bool(value));
        }

        metadata.set("pdfaid", "part", Value::Text("3".to_owned()));
        metadata.set(
            "pdfaid",
            "conformance",
            Value::Text(pdfa.conformance.to_string()),
        );
        let has_intents = self.catalog().is_some_and(|c| c.has(b"OutputIntents"));
        if !has_intents {
            self.add_output_intent()?;
        }
        if pdfa.fix_glyph_widths {
            self.fix_glyph_widths()?;
        }

        // `write_metadata`
        metadata.set("pdf", "Producer", Value::Text(PRODUCER.to_owned()));
        let get_text = |prefix: &str, name: &str| match metadata.get(prefix, name) {
            Some(Value::Text(text)) => Some(text_string(text)),
            Some(Value::Date(date)) => Some(pdf_date(date)),
            _ => None,
        };
        let entries = [
            ("Title", get_text("dc", "title")),
            ("Subject", get_text("dc", "description")),
            ("Creator", get_text("xmp", "CreatorTool")),
            ("CreationDate", get_text("xmp", "CreateDate")),
            ("ModDate", get_text("xmp", "ModifyDate")),
            ("Keywords", get_text("pdf", "Keywords")),
            ("Producer", get_text("pdf", "Producer")),
        ];
        let author = get_text("dc", "creator");
        let trapped = match metadata.get("pdf", "Trapped") {
            Some(Value::Bool(value)) => Some(Object::Name(
                if *value { &b"True"[..] } else { b"False" }.to_vec(),
            )),
            _ => None,
        };
        let language = self
            .catalog()
            .and_then(|c| c.get(b"Lang").ok())
            .map(|l| self.resolve(l).clone())
            .and_then(|l| lopdf::decode_text_string(&l).ok())
            .unwrap_or_else(|| "x-default".to_owned());
        if let Some(info) = self.dict_mut(info_id) {
            for (key, value) in entries {
                match value {
                    Some(value) => info.set(key, value),
                    None => {
                        info.remove(key.as_bytes());
                    }
                }
            }
            if let Some(author) = author {
                info.set("Author", author);
            }
            if let Some(trapped) = trapped {
                info.set("Trapped", trapped);
            }
        }

        let packet = metadata.packet(&language).into_bytes();
        let existing = self
            .catalog()
            .and_then(|c| c.get(b"Metadata").ok())
            .and_then(|m| m.as_reference().ok())
            .filter(|id| matches!(self.doc.objects.get(id), Some(Object::Stream(_))));
        if let Some(id) = existing {
            if let Some(Object::Stream(stream)) = self.doc.objects.get_mut(&id) {
                stream.dict.remove(b"Filter");
                stream.dict.remove(b"DecodeParms");
                stream.set_content(packet);
                stream.allows_compression = false;
            }
            self.dirty.insert(id);
        } else {
            let mut dict = Dictionary::new();
            dict.set("Type", Object::Name(b"Metadata".to_vec()));
            dict.set("Subtype", Object::Name(b"XML".to_vec()));
            let id = self.add(Object::Stream(
                Stream::new(dict, packet).with_compression(false),
            ));
            self.set_catalog("Metadata", Object::Reference(id))?;
        }
        Ok(())
    }

    /// `add_srgb_icc_output_intent`.
    fn add_output_intent(&mut self) -> Result<(), EditError> {
        let mut icc = Dictionary::new();
        icc.set("N", Object::Integer(3));
        let profile = self.add(Object::Stream(
            Stream::new(icc, SRGB_ICC.to_vec()).with_compression(false),
        ));
        let mut intent = Dictionary::new();
        intent.set("S", Object::Name(b"GTS_PDFA1".to_vec()));
        intent.set(
            "OutputConditionIdentifier",
            Object::string_literal("sRGB2014.icc"),
        );
        intent.set("Info", Object::string_literal("sRGB2014.icc"));
        intent.set(
            "RegistryName",
            Object::string_literal("https://www.color.org"),
        );
        intent.set("DestOutputProfile", Object::Reference(profile));
        let intent = self.add(Object::Dictionary(intent));
        self.set_catalog(
            "OutputIntents",
            Object::Array(vec![Object::Reference(intent)]),
        )
    }

    /// `fix_glyph_widths`: the CIDs each `CIDFontType2` font shows on the pages and in the
    /// annotations' appearances, then each such font program rebuilt with the `hmtx` advance
    /// widths of those glyphs set to the PDF widths where they differ by more than one unit.
    fn fix_glyph_widths(&mut self) -> Result<(), EditError> {
        let mut collector = CidCollector {
            editor: self,
            map: BTreeMap::new(),
            depth: 0,
        };
        for page in collector.editor.page_ids() {
            let resources = collector.editor.inherited(page, b"Resources");
            let content = collector.editor.doc.get_page_content(page);
            collector.process(&content, resources.as_ref(), None)?;
            for annotation in collector.editor.annotation_ids(page) {
                let Some(appearance) = collector.editor.appearance(annotation) else {
                    continue;
                };
                collector.process_form(appearance, resources.as_ref())?;
            }
        }
        let map = std::mem::take(&mut collector.map);
        for (font, cids) in map {
            if cids.is_empty() {
                continue;
            }
            self.fix_font_widths(font, &cids)?;
        }
        Ok(())
    }

    fn fix_font_widths(&mut self, font: ObjectId, cids: &BTreeSet<u32>) -> Result<(), EditError> {
        let unsupported = EditError::Unsupported;
        let dict = self
            .dict(font)
            .cloned()
            .ok_or(unsupported("PDF/A output with a missing CID font"))?;
        let file_id = dict
            .get(b"FontDescriptor")
            .ok()
            .map(|d| self.resolve(d))
            .and_then(|d| d.as_dict().ok())
            .and_then(|d| d.get(b"FontFile2").ok())
            .and_then(|f| f.as_reference().ok())
            .ok_or(unsupported("PDF/A output with fonts that are not embedded"))?;
        let gid_map: Option<Vec<u16>> = match dict.get(b"CIDToGIDMap").map(|m| self.resolve(m)) {
            Ok(Object::Name(name)) if name == b"Identity" => None,
            Ok(Object::Stream(stream)) => Some(
                stream
                    .decompressed_content()
                    .unwrap_or_else(|_| stream.content.clone())
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_be_bytes(*c))
                    .collect(),
            ),
            _ => {
                return Err(unsupported(
                    "PDF/A output with CID fonts without a CIDToGIDMap",
                ));
            }
        };
        let widths = cid_widths(self, &dict);
        let default_width = dict
            .get(b"DW")
            .ok()
            .and_then(|w| number(self.resolve(w)))
            .unwrap_or(1000.0);
        let Some(Object::Stream(file)) = self.doc.objects.get(&file_id) else {
            return Err(unsupported("PDF/A output with fonts that are not embedded"));
        };
        let program = file
            .decompressed_content()
            .unwrap_or_else(|_| file.content.clone());
        let mut tables = TrueType::parse(&program)
            .ok_or_else(|| EditError::Font("invalid embedded TrueType font".to_owned()))?;
        let units_per_em = tables.u16_at("head", 18).unwrap_or(1000);
        let long_metrics = usize::from(tables.u16_at("hhea", 34).unwrap_or(0));
        let factor = 1000.0 / f64::from(units_per_em);
        let mut hmtx = tables.get("hmtx").map(<[u8]>::to_vec).unwrap_or_default();
        for cid in cids {
            let cid_width = widths.get(cid).copied().unwrap_or(default_width);
            let gid = match &gid_map {
                None => usize::try_from(*cid).unwrap_or(usize::MAX),
                Some(map) => {
                    usize::from(*map.get(usize::try_from(*cid).unwrap_or(usize::MAX)).ok_or(
                        unsupported("PDF/A output with a CID outside the CIDToGIDMap"),
                    )?)
                }
            };
            let index = gid.min(long_metrics.saturating_sub(1));
            let advance = hmtx
                .get(4 * index..4 * index + 2)
                .map_or(0, |b| u16::from_be_bytes([b[0], b[1]]));
            let gid_width = f64::from(advance) * factor;
            if (cid_width - gid_width).abs().round() <= 1.0 {
                continue;
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "`pack('n')` keeps the low 16 bits"
            )]
            let value = ((cid_width / factor).round() as i64 as u16).to_be_bytes();
            let at = 4 * gid;
            if at > hmtx.len() {
                return Err(unsupported(
                    "PDF/A output with glyphs outside the hmtx table",
                ));
            }
            hmtx.truncate(hmtx.len().max(at));
            let end = (at + 2).min(hmtx.len());
            hmtx.splice(at..end, value);
        }
        tables.set("hmtx", hmtx);
        let rebuilt = tables.build();
        if let Some(Object::Stream(file)) = self.doc.objects.get_mut(&file_id) {
            let compressed = file
                .dict
                .get(b"Filter")
                .is_ok_and(|f| f.as_name().ok() == Some(b"FlateDecode"));
            file.dict.remove(b"DecodeParms");
            if compressed {
                file.set_content(deflate(&rebuilt));
            } else {
                file.dict.remove(b"Filter");
                file.set_content(rebuilt);
            }
            file.allows_compression = false;
        }
        self.dirty.insert(file_id);
        Ok(())
    }
}

/// `CIDFont#widths`: the `W` array as CID to width.
fn cid_widths(editor: &Editor, font: &Dictionary) -> BTreeMap<u32, f64> {
    let mut result = BTreeMap::new();
    let Some(array) = font
        .get(b"W")
        .ok()
        .and_then(|w| editor.resolve(w).as_array().ok())
    else {
        return result;
    };
    let as_u32 = |o: &Object| {
        number(editor.resolve(o)).filter(|v| *v >= 0.0).map(|v| {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "CIDs"
            )]
            let v = v as u32;
            v
        })
    };
    let mut index = 0;
    while index + 1 < array.len() {
        let Some(first) = as_u32(&array[index]) else {
            break;
        };
        if let Ok(widths) = editor.resolve(&array[index + 1]).as_array() {
            for (i, width) in widths.iter().enumerate() {
                if let Some(width) = number(editor.resolve(width)) {
                    result.insert(first + u32::try_from(i).unwrap_or(0), width);
                }
            }
            index += 2;
        } else {
            let (Some(last), Some(width)) = (
                as_u32(&array[index + 1]),
                array.get(index + 2).and_then(|w| number(editor.resolve(w))),
            ) else {
                break;
            };
            for cid in first..=last {
                result.insert(cid, width);
            }
            index += 3;
        }
    }
    result
}

/// `CIDCollector`: the CIDs shown with each `CIDFontType2` descendant font.
struct CidCollector<'a> {
    editor: &'a Editor,
    map: BTreeMap<ObjectId, BTreeSet<u32>>,
    depth: usize,
}

impl CidCollector<'_> {
    /// `form.process_contents(processor, original_resources:)`.
    fn process_form(&mut self, form: ObjectId, original: Option<&Object>) -> Result<(), EditError> {
        let Some(Object::Stream(stream)) = self.editor.doc.objects.get(&form) else {
            return Ok(());
        };
        let content = stream
            .decompressed_content()
            .unwrap_or_else(|_| stream.content.clone());
        let resources = stream
            .dict
            .get(b"Resources")
            .ok()
            .map(|r| self.editor.resolve(r).clone())
            .or_else(|| original.cloned());
        self.process(&content, resources.as_ref(), None)
    }

    fn process(
        &mut self,
        content: &[u8],
        resources: Option<&Object>,
        font: Option<ObjectId>,
    ) -> Result<(), EditError> {
        self.depth += 1;
        if self.depth > 32 {
            return Err(EditError::Unsupported(
                "PDF/A output with deeply nested forms",
            ));
        }
        let operations = Content::decode(content)
            .map_err(|e| EditError::Invalid(format!("content stream: {e}")))?
            .operations;
        let resources = resources
            .and_then(|r| self.editor.resolve(r).as_dict().ok())
            .cloned()
            .unwrap_or_default();
        let lookup = |kind: &[u8], name: &[u8]| -> Option<&Object> {
            let group = self
                .editor
                .resolve(resources.get(kind).ok()?)
                .as_dict()
                .ok()?;
            group.get(name).ok()
        };
        let mut font = font;
        let mut stack = Vec::new();
        for operation in &operations {
            match operation.operator.as_str() {
                "q" => stack.push(font),
                "Q" => font = stack.pop().unwrap_or(font),
                "Tf" => {
                    font = operation
                        .operands
                        .first()
                        .and_then(|n| n.as_name().ok())
                        .and_then(|n| lookup(b"Font", n))
                        .and_then(|f| f.as_reference().ok());
                }
                "Tj" | "'" | "\"" | "TJ" => {
                    let strings: Vec<&[u8]> = operation
                        .operands
                        .iter()
                        .flat_map(|o| match o {
                            Object::Array(items) => items.iter().collect::<Vec<_>>(),
                            other => vec![other],
                        })
                        .filter_map(|o| o.as_str().ok())
                        .collect();
                    self.show_text(font, &strings)?;
                }
                "Do" => {
                    let Some(id) = operation
                        .operands
                        .first()
                        .and_then(|n| n.as_name().ok())
                        .and_then(|n| lookup(b"XObject", n))
                        .and_then(|x| x.as_reference().ok())
                    else {
                        continue;
                    };
                    let is_form = matches!(
                        self.editor.doc.objects.get(&id),
                        Some(Object::Stream(s)) if s.dict.get(b"Subtype").ok().and_then(|t| t.as_name().ok()) == Some(b"Form")
                    );
                    if is_form {
                        let original = Object::Dictionary(resources.clone());
                        self.process_form(id, Some(&original))?;
                    }
                }
                _ => {}
            }
        }
        self.depth -= 1;
        Ok(())
    }

    /// `show_text(data)`: records the CIDs when the font is a `Type0` font with a
    /// `CIDFontType2` descendant.
    fn show_text(&mut self, font: Option<ObjectId>, strings: &[&[u8]]) -> Result<(), EditError> {
        let font = font
            .and_then(|f| self.editor.dict(f))
            .ok_or(EditError::Unsupported(
                "PDF/A output with text in a missing font",
            ))?;
        if font.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) != Some(b"Type0") {
            return Ok(());
        }
        let Some(descendant) = font
            .get(b"DescendantFonts")
            .ok()
            .and_then(|d| self.editor.resolve(d).as_array().ok())
            .and_then(|d| d.first())
            .and_then(|d| d.as_reference().ok())
        else {
            return Err(EditError::Unsupported(
                "PDF/A output with a Type0 font without a descendant font",
            ));
        };
        let is_type2 = self
            .editor
            .dict(descendant)
            .and_then(|d| d.get(b"Subtype").ok())
            .and_then(|s| s.as_name().ok())
            == Some(b"CIDFontType2");
        if !is_type2 {
            return Ok(());
        }
        let encoding = font.get(b"Encoding").ok().and_then(|e| e.as_name().ok());
        if !matches!(encoding, Some(b"Identity-H" | b"Identity-V")) {
            return Err(EditError::Unsupported(
                "PDF/A output with CID fonts without an identity encoding",
            ));
        }
        let entry = self.map.entry(descendant).or_default();
        for string in strings {
            for pair in string.as_chunks::<2>().0 {
                entry.insert(u32::from(u16::from_be_bytes(*pair)));
            }
        }
        Ok(())
    }
}

/// The tables of a TrueType font program (`HexaPDF::Font::TrueType::Font`), rebuilt with
/// `Builder.build`.
struct TrueType {
    glyf: bool,
    tables: Vec<([u8; 4], Vec<u8>)>,
}

impl TrueType {
    fn parse(data: &[u8]) -> Option<Self> {
        let count = usize::from(u16::from_be_bytes([*data.get(4)?, *data.get(5)?]));
        let mut tables = Vec::with_capacity(count);
        for i in 0..count {
            let entry = data.get(12 + 16 * i..28 + 16 * i)?;
            let tag: [u8; 4] = entry[..4].try_into().ok()?;
            let offset = u32::from_be_bytes(entry[8..12].try_into().ok()?) as usize;
            let length = u32::from_be_bytes(entry[12..16].try_into().ok()?) as usize;
            tables.push((tag, data.get(offset..offset + length)?.to_vec()));
        }
        let glyf = tables.iter().any(|(t, _)| t == b"glyf");
        Some(Self { glyf, tables })
    }

    fn get(&self, tag: &str) -> Option<&[u8]> {
        self.tables
            .iter()
            .find(|(t, _)| t == tag.as_bytes())
            .map(|(_, d)| d.as_slice())
    }

    fn u16_at(&self, tag: &str, offset: usize) -> Option<u16> {
        let data = self.get(tag)?;
        Some(u16::from_be_bytes([
            *data.get(offset)?,
            *data.get(offset + 1)?,
        ]))
    }

    fn set(&mut self, tag: &str, data: Vec<u8>) {
        if let Some(entry) = self.tables.iter_mut().find(|(t, _)| t == tag.as_bytes()) {
            entry.1 = data;
        } else {
            let mut name = [b' '; 4];
            name.copy_from_slice(&tag.as_bytes()[..4]);
            self.tables.push((name, data));
        }
    }

    /// `Table.calculate_checksum`.
    fn checksum(data: &[u8]) -> u64 {
        let remainder = data.len() % 4;
        let mut checksum = 0_u64;
        if remainder != 0 {
            let mut last = [0_u8; 4];
            last[..remainder].copy_from_slice(&data[data.len() - remainder..]);
            checksum = u64::from(u32::from_be_bytes(last));
        }
        let sum = data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u64::from(u32::from_be_bytes(*c)))
            .sum::<u64>()
            % (1 << 32);
        checksum + sum
    }

    /// `Builder.build(tables)`.
    fn build(mut self) -> Vec<u8> {
        let count = self.tables.len();
        let bits = count.bit_width();
        let search_range = (1_usize << bits.saturating_sub(1)) * 16;
        let entry_selector = bits.saturating_sub(1) as usize;
        let range_shift = (count * 16).wrapping_sub(search_range);
        let mut font = if self.glyf {
            vec![0, 1, 0, 0]
        } else {
            b"OTTO".to_vec()
        };
        for value in [count, search_range, entry_selector, range_shift] {
            #[allow(clippy::cast_possible_truncation, reason = "`pack('n')`")]
            font.extend_from_slice(&(value as u16).to_be_bytes());
        }
        let mut offset = font.len() + count * 16;
        let mut checksum = u128::from(Self::checksum(&font));
        if let Some((_, head)) = self.tables.iter_mut().find(|(t, _)| t == b"head")
            && head.len() >= 12
        {
            head[8..12].copy_from_slice(&[0; 4]);
        }
        self.tables.sort_by_key(|a| a.0);
        let mut data = Vec::new();
        for (tag, table) in &mut self.tables {
            let length = table.len();
            let table_checksum = Self::checksum(table);
            if length % 4 != 0 {
                table.resize(length + 4 - length % 4, 0);
            }
            checksum += u128::from(u32::from_be_bytes(*tag))
                + 2 * u128::from(table_checksum)
                + offset as u128
                + table.len() as u128;
            font.extend_from_slice(tag);
            #[allow(clippy::cast_possible_truncation, reason = "`pack('N')`")]
            for value in [table_checksum, offset as u64, length as u64] {
                font.extend_from_slice(&(value as u32).to_be_bytes());
            }
            offset += table.len();
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "`pack('N')` keeps the low 32 bits"
        )]
        let adjustment = (0xB1B0_AFBA_u128.wrapping_sub(checksum) as u32).to_be_bytes();
        for (tag, table) in &mut self.tables {
            if tag == b"head" && table.len() >= 12 {
                table[8..12].copy_from_slice(&adjustment);
            }
            data.extend_from_slice(table);
        }
        font.extend_from_slice(&data);
        font
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pdf_dates_like_hexapdf() {
        let date = |s: &str| parse_pdf_date(s.as_bytes()).map(|d| d.to_rfc3339());
        assert_eq!(
            date("D:20070223155637Z").as_deref(),
            Some("2007-02-23T15:56:37+00:00")
        );
        assert_eq!(
            date("D:20070223155637+01'00'").as_deref(),
            Some("2007-02-23T15:56:37+01:00")
        );
        assert_eq!(
            date("D:20070223155637-05'30").as_deref(),
            Some("2007-02-23T15:56:37-05:30")
        );
        assert_eq!(date("D:2007").as_deref(), Some("2007-01-01T00:00:00+00:00"));
        assert_eq!(
            date("D:20070231").as_deref(),
            Some("2007-03-03T00:00:00+00:00")
        );
        assert_eq!(
            date("D:2007022315563").as_deref(),
            Some("2007-02-23T15:56:00+00:00")
        );
        assert_eq!(
            date("D:20071301").as_deref(),
            Some("2007-12-01T00:00:00+00:00")
        );
        assert_eq!(date("2007"), None);
        assert_eq!(date("D:2007x"), None);
    }

    #[test]
    fn serializes_dates_like_hexapdf() {
        let date = parse_pdf_date(b"D:20070223155637+01'00'").unwrap();
        assert_eq!(
            pdf_date(&date).as_str().unwrap(),
            b"D:20070223155637+01'00'"
        );
        let utc = parse_pdf_date(b"D:20070223155637Z").unwrap();
        assert_eq!(pdf_date(&utc).as_str().unwrap(), b"D:20070223155637");
    }

    #[test]
    fn rebuilds_true_type_tables_sorted() {
        let mut font = TrueType {
            glyf: true,
            tables: vec![
                (*b"hmtx", vec![0, 1, 0, 2]),
                (*b"head", vec![0; 54]),
                (*b"glyf", vec![1, 2, 3]),
            ],
        };
        font.set("hmtx", vec![0, 9, 0, 2]);
        let built = font.build();
        let parsed = TrueType::parse(&built).unwrap();
        let tags: Vec<&[u8; 4]> = parsed.tables.iter().map(|(t, _)| t).collect();
        assert_eq!(tags, [b"glyf", b"head", b"hmtx"]);
        assert_eq!(parsed.get("hmtx"), Some(&[0, 9, 0, 2][..]));
        assert_eq!(parsed.get("glyf"), Some(&[1, 2, 3][..]));
    }
}
