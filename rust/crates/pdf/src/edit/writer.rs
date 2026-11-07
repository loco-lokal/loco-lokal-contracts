//! Serializing objects and appending an incremental update (ISO 32000-1, 7.5.6), as
//! `HexaPDF::Document#write(io, incremental: true)` does: the original bytes stay as
//! they are, followed by the changed and new objects, a cross-reference section of the
//! same kind as the original's, and a trailer pointing at the previous one.

use std::io::Write as _;

use lopdf::{Dictionary, Object, ObjectId, StringFormat, xref::XrefType};

/// A real number as HexaPDF writes it: integers without a fraction, otherwise at most
/// four decimals without trailing zeros.
#[must_use]
pub fn real(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_owned();
    }
    let rounded = (value * 10_000.0).round() / 10_000.0;
    if rounded.fract().abs() < f64::EPSILON && rounded.abs() < 1e15 {
        #[allow(clippy::cast_possible_truncation, reason = "checked range")]
        return (rounded as i64).to_string();
    }
    let text = format!("{rounded:.4}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    match text {
        "-0" => "0".to_owned(),
        _ => text.to_owned(),
    }
}

fn is_regular(byte: u8) -> bool {
    !matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%' | b'#'
    ) && (b'!'..=b'~').contains(&byte)
}

fn write_name(out: &mut Vec<u8>, name: &[u8]) {
    out.push(b'/');
    for &byte in name {
        if is_regular(byte) {
            out.push(byte);
        } else {
            let _ = write!(out, "#{byte:02X}");
        }
    }
}

fn write_literal(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'(');
    for &byte in bytes {
        match byte {
            b'(' | b')' | b'\\' => {
                out.push(b'\\');
                out.push(byte);
            }
            b'\r' => out.extend_from_slice(b"\\r"),
            _ => out.push(byte),
        }
    }
    out.push(b')');
}

/// Writes one object (direct form).
pub fn write_object(out: &mut Vec<u8>, object: &Object) {
    match object {
        Object::Null => out.extend_from_slice(b"null"),
        Object::Boolean(value) => out.extend_from_slice(if *value { b"true" } else { b"false" }),
        Object::Integer(value) => {
            let _ = write!(out, "{value}");
        }
        Object::Real(value) => out.extend_from_slice(real(f64::from(*value)).as_bytes()),
        Object::Name(name) => write_name(out, name),
        Object::String(bytes, StringFormat::Literal) => write_literal(out, bytes),
        Object::String(bytes, StringFormat::Hexadecimal) => {
            out.push(b'<');
            for byte in bytes {
                let _ = write!(out, "{byte:02X}");
            }
            out.push(b'>');
        }
        Object::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b' ');
                }
                write_object(out, item);
            }
            out.push(b']');
        }
        Object::Dictionary(dict) => write_dictionary(out, dict),
        Object::Stream(stream) => {
            let mut dict = stream.dict.clone();
            dict.set(
                "Length",
                Object::Integer(i64::try_from(stream.content.len()).unwrap_or(i64::MAX)),
            );
            write_dictionary(out, &dict);
            out.extend_from_slice(b"stream\n");
            out.extend_from_slice(&stream.content);
            out.extend_from_slice(b"\nendstream");
        }
        Object::Reference((id, generation)) => {
            let _ = write!(out, "{id} {generation} R");
        }
    }
}

fn write_dictionary(out: &mut Vec<u8>, dict: &Dictionary) {
    out.extend_from_slice(b"<<");
    for (key, value) in dict {
        write_name(out, key);
        if !matches!(
            value,
            Object::Name(_)
                | Object::String(..)
                | Object::Array(_)
                | Object::Dictionary(_)
                | Object::Stream(_)
        ) {
            out.push(b' ');
        }
        write_object(out, value);
    }
    out.extend_from_slice(b">>");
}

/// Where each written object starts, for the cross-reference section and for patching
/// the signature placeholder.
pub struct Written {
    pub bytes: Vec<u8>,
    pub offsets: Vec<(ObjectId, usize)>,
}

/// Appends `objects` (in id order) to `original` as one incremental update.
pub fn append_update(
    original: &[u8],
    objects: &[(ObjectId, &Object)],
    trailer: &Dictionary,
    xref_type: XrefType,
    prev_startxref: usize,
) -> Written {
    let mut out = Vec::with_capacity(original.len() + 64 * 1024);
    out.extend_from_slice(original);
    if out.last() != Some(&b'\n') {
        out.push(b'\n');
    }
    let mut offsets = Vec::with_capacity(objects.len());
    for ((id, generation), object) in objects {
        offsets.push(((*id, *generation), out.len()));
        let _ = writeln!(out, "{id} {generation} obj");
        write_object(&mut out, object);
        out.extend_from_slice(b"\nendobj\n");
    }
    let max_id = objects.iter().map(|((id, _), _)| *id).max().unwrap_or(0);
    let size = trailer
        .get(b"Size")
        .ok()
        .and_then(|s| s.as_i64().ok())
        .unwrap_or(0)
        .max(i64::from(max_id) + 1);
    let mut trailer = trailer.clone();
    trailer.remove(b"XRefStm");
    trailer.remove(b"DecodeParms");
    trailer.remove(b"Filter");
    trailer.remove(b"W");
    trailer.remove(b"Index");
    trailer.remove(b"Length");
    trailer.remove(b"Type");
    trailer.set("Size", Object::Integer(size));
    trailer.set(
        "Prev",
        Object::Integer(i64::try_from(prev_startxref).unwrap_or_default()),
    );

    let startxref = out.len();
    match xref_type {
        XrefType::CrossReferenceTable => {
            out.extend_from_slice(b"xref\n");
            for run in runs(&offsets) {
                let _ = writeln!(out, "{} {}", run[0].0.0, run.len());
                for ((_, generation), offset) in run {
                    let _ = writeln!(out, "{offset:010} {generation:05} n ");
                }
            }
            out.extend_from_slice(b"trailer\n");
            write_dictionary(&mut out, &trailer);
            let _ = write!(out, "\nstartxref\n{startxref}\n%%EOF\n");
        }
        XrefType::CrossReferenceStream => {
            // The stream describes itself too.
            let stream_id = u32::try_from(size).unwrap_or(u32::MAX);
            let mut entries = offsets.clone();
            entries.push(((stream_id, 0), startxref));
            let mut data = Vec::new();
            let mut index = Vec::new();
            for run in runs(&entries) {
                index.push(Object::Integer(i64::from(run[0].0.0)));
                index.push(Object::Integer(
                    i64::try_from(run.len()).unwrap_or_default(),
                ));
                for ((_, generation), offset) in run {
                    data.push(1);
                    data.extend_from_slice(
                        &u32::try_from(offset).unwrap_or(u32::MAX).to_be_bytes(),
                    );
                    data.extend_from_slice(&generation.to_be_bytes());
                }
            }
            trailer.set("Type", Object::Name(b"XRef".to_vec()));
            trailer.set("Size", Object::Integer(size + 1));
            trailer.set("W", Object::Array(vec![1.into(), 4.into(), 2.into()]));
            trailer.set("Index", Object::Array(index));
            let stream = Object::Stream(lopdf::Stream::new(trailer, data));
            let _ = writeln!(out, "{stream_id} 0 obj");
            write_object(&mut out, &stream);
            out.extend_from_slice(b"\nendobj\n");
            let _ = write!(out, "startxref\n{startxref}\n%%EOF\n");
        }
    }
    Written {
        bytes: out,
        offsets,
    }
}

/// Consecutive object id runs, for cross-reference subsections.
fn runs(entries: &[(ObjectId, usize)]) -> Vec<Vec<(ObjectId, usize)>> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|((id, _), _)| *id);
    let mut result: Vec<Vec<(ObjectId, usize)>> = Vec::new();
    for entry in sorted {
        match result.last_mut() {
            Some(run) if run.last().is_some_and(|last| last.0.0 + 1 == entry.0.0) => {
                run.push(entry);
            }
            _ => result.push(vec![entry]),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_reals_like_hexapdf() {
        assert_eq!(real(12.0), "12");
        assert_eq!(real(0.123_456), "0.1235");
        assert_eq!(real(-0.000_01), "0");
        assert_eq!(real(841.89), "841.89");
    }

    #[test]
    fn escapes_names_and_strings() {
        let mut out = Vec::new();
        write_object(&mut out, &Object::Name(b"A B#".to_vec()));
        write_object(
            &mut out,
            &Object::String(b"(x)\\".to_vec(), StringFormat::Literal),
        );
        assert_eq!(out, b"/A#20B#23(\\(x\\)\\\\)");
    }
}
