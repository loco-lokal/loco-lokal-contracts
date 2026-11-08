//! Fonts for drawing text into PDFs, as `HexaPDF::Document::Fonts#add` provides them:
//! TrueType fonts embedded as subset CID fonts (`Identity-H`, with a `ToUnicode` map), and
//! the standard Type1 fonts (Times, Helvetica, Courier) with AFM metrics.
//!
//! Metrics follow HexaPDF: glyph widths in 1/1000 em, the OS/2 typographic ascender and
//! descender (falling back to `hhea`), and no kerning. Characters a font lacks become
//! the glyphs `on_missing_glyph` picks in `Submissions::GenerateResultAttachments`.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use flate2::{Compression, write::ZlibEncoder};
use lopdf::{Dictionary, Object, Stream, StringFormat};

use super::EditError;

/// One positioned glyph: how to encode it and how wide it is (1/1000 em).
#[derive(Clone, Debug)]
pub struct Glyph {
    /// Glyph id (TrueType, before subsetting) or character code (Type1).
    pub id: u16,
    pub width: f64,
    /// The text it stands for (`Glyph#str`).
    pub text: String,
}

/// A loaded TrueType font file.
pub struct TrueTypeData {
    pub name: String,
    data: &'static [u8],
    units_per_em: f64,
    ascender: f64,
    descender: f64,
    cap_height: f64,
    italic_angle: f64,
    bbox: [f64; 4],
    /// `post` underline position and thickness (1/1000 em).
    underline: (f64, f64),
}

impl TrueTypeData {
    fn load(path: &PathBuf) -> Result<Self, EditError> {
        use skrifa::{MetadataProvider as _, raw::TableProvider as _, string::StringId};
        let bytes = std::fs::read(path)
            .map_err(|err| EditError::Font(format!("{}: {err}", path.display())))?;
        let data: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        let font = skrifa::FontRef::new(data).map_err(|err| EditError::Font(err.to_string()))?;
        let head = font
            .head()
            .map_err(|err| EditError::Font(err.to_string()))?;
        let upem = f64::from(head.units_per_em());
        let scale = 1000.0 / upem;
        let name = font
            .localized_strings(StringId::POSTSCRIPT_NAME)
            .english_or_first()
            .map_or_else(|| "Font".to_owned(), |n| n.chars().collect());
        let os2 = font.os2().ok();
        let (ascender, descender) = match (&os2, font.hhea()) {
            (Some(os2), _) => (
                f64::from(os2.s_typo_ascender()),
                f64::from(os2.s_typo_descender()),
            ),
            (None, Ok(hhea)) => (
                f64::from(hhea.ascender().to_i16()),
                f64::from(hhea.descender().to_i16()),
            ),
            (None, Err(_)) => (upem * 0.8, -upem * 0.2),
        };
        let cap_height = os2
            .as_ref()
            .and_then(skrifa::raw::tables::os2::Os2::s_cap_height)
            .map_or(ascender, f64::from);
        let italic_angle = font.post().map_or(0.0, |post| post.italic_angle().to_f64());
        let underline = font.post().map_or((-100.0, 50.0), |post| {
            (
                f64::from(post.underline_position().to_i16()) * scale,
                f64::from(post.underline_thickness().to_i16()) * scale,
            )
        });
        Ok(Self {
            name,
            data,
            units_per_em: upem,
            ascender: ascender * scale,
            descender: descender * scale,
            cap_height: cap_height * scale,
            italic_angle,
            bbox: [
                f64::from(head.x_min()) * scale,
                f64::from(head.y_min()) * scale,
                f64::from(head.x_max()) * scale,
                f64::from(head.y_max()) * scale,
            ],
            underline,
        })
    }

    fn font(&self) -> skrifa::FontRef<'static> {
        skrifa::FontRef::new(self.data).expect("parsed at load")
    }
}

/// Parsed AFM metrics of a standard Type1 font.
pub struct Afm {
    pub name: String,
    /// Glyph names and widths; a glyph's id is its index here.
    glyphs: Vec<(String, f64)>,
    by_name: HashMap<String, u16>,
    ascender: f64,
    descender: f64,
    underline: (f64, f64),
}

impl Afm {
    fn parse(name: &str, text: &str) -> Self {
        let mut glyphs = Vec::new();
        let mut ascender = 0.0;
        let mut descender = 0.0;
        let mut bbox = [0.0; 4];
        let mut underline = (-100.0, 50.0);
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("UnderlinePosition ") {
                underline.0 = rest.trim().parse().unwrap_or(underline.0);
            } else if let Some(rest) = line.strip_prefix("UnderlineThickness ") {
                underline.1 = rest.trim().parse().unwrap_or(underline.1);
            }
            if let Some(rest) = line.strip_prefix("Ascender ") {
                ascender = rest.trim().parse().unwrap_or(0.0);
            } else if let Some(rest) = line.strip_prefix("Descender ") {
                descender = rest.trim().parse().unwrap_or(0.0);
            } else if let Some(rest) = line.strip_prefix("FontBBox ") {
                for (i, v) in rest.split_whitespace().take(4).enumerate() {
                    bbox[i] = v.parse().unwrap_or(0.0);
                }
            } else if line.starts_with("C ") {
                let mut width = None;
                let mut glyph = None;
                for part in line.split(';') {
                    let part = part.trim();
                    if let Some(w) = part.strip_prefix("WX ") {
                        width = w.trim().parse().ok();
                    } else if let Some(n) = part.strip_prefix("N ") {
                        glyph = Some(n.trim().to_owned());
                    }
                }
                if let (Some(w), Some(n)) = (width, glyph) {
                    glyphs.push((n, w));
                }
            }
        }
        if ascender == 0.0 {
            ascender = bbox[3];
        }
        if descender == 0.0 {
            descender = bbox[1];
        }
        let by_name = glyphs
            .iter()
            .enumerate()
            .map(|(i, (n, _))| (n.clone(), u16::try_from(i).unwrap_or(u16::MAX)))
            .collect();
        Self {
            name: name.to_owned(),
            glyphs,
            by_name,
            ascender,
            descender,
            underline,
        }
    }

    fn glyph(&self, name: &str) -> Option<u16> {
        self.by_name.get(name).copied()
    }
}

/// The font a style draws with.
pub enum FontData {
    TrueType(TrueTypeData),
    Type1(Afm),
}

impl FontData {
    #[must_use]
    pub fn ascender(&self) -> f64 {
        match self {
            Self::TrueType(f) => f.ascender,
            Self::Type1(f) => f.ascender,
        }
    }

    #[must_use]
    pub fn descender(&self) -> f64 {
        match self {
            Self::TrueType(f) => f.descender,
            Self::Type1(f) => f.descender,
        }
    }

    /// Underline position (below the baseline, negative) and thickness, in 1/1000 em.
    #[must_use]
    pub fn underline(&self) -> (f64, f64) {
        match self {
            Self::TrueType(f) => f.underline,
            Self::Type1(f) => f.underline,
        }
    }

    #[must_use]
    pub fn is_type1(&self) -> bool {
        matches!(self, Self::Type1(_))
    }

    /// `font.decode_utf8(text)` with `on_missing_glyph`.
    #[must_use]
    pub fn glyphs(&self, text: &str) -> Vec<Glyph> {
        match self {
            Self::TrueType(font) => {
                use skrifa::{
                    MetadataProvider as _,
                    instance::{LocationRef, Size},
                };
                let face = font.font();
                let charmap = face.charmap();
                let metrics = face.glyph_metrics(Size::unscaled(), LocationRef::default());
                let scale = 1000.0 / font.units_per_em;
                text.chars()
                    .map(|c| {
                        let id = charmap
                            .map(c)
                            .and_then(|g| u16::try_from(g.to_u32()).ok())
                            .unwrap_or_else(|| missing_true_type(c));
                        let advance = metrics
                            .advance_width(skrifa::GlyphId::new(u32::from(id)))
                            .unwrap_or(0.0);
                        Glyph {
                            id,
                            width: f64::from(advance) * scale,
                            text: c.to_string(),
                        }
                    })
                    .collect()
            }
            Self::Type1(afm) => text
                .chars()
                .map(|c| {
                    // `decode_codepoint`, then `MISSING_GLYPH_REPLACE_TYPE1` or a space.
                    let id = glyph_name(c)
                        .and_then(|name| afm.glyph(name))
                        .or_else(|| afm.glyph(missing_type1(c)))
                        .or_else(|| afm.glyph("space"))
                        .unwrap_or(0);
                    Glyph {
                        id,
                        width: afm.glyphs.get(usize::from(id)).map_or(0.0, |g| g.1),
                        text: c.to_string(),
                    }
                })
                .collect(),
        }
    }
}

/// `MISSING_GLYPH_REPLACE`: the glyph id is the replacement's first byte minus 29, which is
/// its glyph in GoNoto's ordering.
fn missing_true_type(c: char) -> u16 {
    let replace = match c {
        '▪' => '-',
        '✔' | '✓' | '✅' => 'V',
        _ => ' ',
    };
    u16::try_from(u32::from(replace))
        .unwrap_or(32)
        .saturating_sub(29)
}

/// `MISSING_GLYPH_REPLACE_TYPE1`.
fn missing_type1(c: char) -> &'static str {
    match c {
        '▪' => "bullet",
        '✔' | '✓' | '✅' => "V",
        _ => "space",
    }
}

/// `GlyphList.unicode_to_name`.
fn glyph_name(c: char) -> Option<&'static str> {
    use super::glyph_names::UNICODE_NAMES;
    UNICODE_NAMES
        .binary_search_by(|(ch, _)| ch.cmp(&c))
        .ok()
        .map(|i| UNICODE_NAMES[i].1)
}

/// `FONT_PATH`'s directory: `FONTS_DIR`, `/fonts` (the Docker image), or `rust/vendor/fonts`
/// (`rust/scripts/fetch-fonts.sh`).
fn fonts_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FONTS_DIR") {
        return PathBuf::from(dir);
    }
    let docker = PathBuf::from("/fonts");
    if docker.join("GoNotoKurrent-Regular.ttf").exists() {
        return docker;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/fonts")
}

macro_rules! afm {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!("../../../../data/afm/", $name, ".afm")),
        )
    };
}

const AFMS: [(&str, &str); 12] = [
    afm!("Courier"),
    afm!("Courier-Bold"),
    afm!("Courier-Oblique"),
    afm!("Courier-BoldOblique"),
    afm!("Helvetica"),
    afm!("Helvetica-Bold"),
    afm!("Helvetica-Oblique"),
    afm!("Helvetica-BoldOblique"),
    afm!("Times-Roman"),
    afm!("Times-Bold"),
    afm!("Times-Italic"),
    afm!("Times-BoldItalic"),
];

/// The process-wide font cache (fonts are large and read once).
fn cache() -> &'static std::sync::Mutex<HashMap<String, Arc<FontData>>> {
    static CACHE: OnceLock<std::sync::Mutex<HashMap<String, Arc<FontData>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The font for a family (`"GoNotoKurrent-Regular.ttf"`-style file name, or a standard
/// family) and variant (`none`, `bold`, `italic`, `bold_italic`).
pub fn load(family: &str, variant: &str) -> Result<Arc<FontData>, EditError> {
    let key = format!("{family}/{variant}");
    if let Some(font) = cache().lock().expect("font cache").get(&key) {
        return Ok(font.clone());
    }
    let font = if let Some(base) = standard_font_name(family, variant) {
        let (_, text) = AFMS
            .iter()
            .find(|(name, _)| *name == base)
            .ok_or_else(|| EditError::Font(format!("no metrics for {base}")))?;
        FontData::Type1(Afm::parse(base, text))
    } else {
        FontData::TrueType(TrueTypeData::load(&fonts_dir().join(family))?)
    };
    let font = Arc::new(font);
    cache()
        .lock()
        .expect("font cache")
        .insert(key, font.clone());
    Ok(font)
}

/// The base font name of a standard family and variant.
fn standard_font_name(family: &str, variant: &str) -> Option<&'static str> {
    Some(match (family, variant) {
        ("Helvetica", "bold") => "Helvetica-Bold",
        ("Helvetica", "italic") => "Helvetica-Oblique",
        ("Helvetica", "bold_italic") => "Helvetica-BoldOblique",
        ("Helvetica", _) => "Helvetica",
        ("Times", "bold") => "Times-Bold",
        ("Times", "italic") => "Times-Italic",
        ("Times", "bold_italic") => "Times-BoldItalic",
        ("Times", _) => "Times-Roman",
        ("Courier", "bold") => "Courier-Bold",
        ("Courier", "italic") => "Courier-Oblique",
        ("Courier", "bold_italic") => "Courier-BoldOblique",
        ("Courier", _) => "Courier",
        _ => return None,
    })
}

/// A font used in one document: the glyphs drawn so far, remapped for the subset.
pub struct UsedFont {
    pub font: Arc<FontData>,
    pub object_id: lopdf::ObjectId,
    remapper: subsetter::GlyphRemapper,
    /// New glyph id → (width, text).
    glyphs: BTreeMap<u16, (f64, String)>,
    /// Type1 custom encoding: glyph id → code (`Type1Wrapper#encode`; 32 is the space).
    codes: BTreeMap<u16, u8>,
}

impl UsedFont {
    #[must_use]
    pub fn new(font: Arc<FontData>, object_id: lopdf::ObjectId) -> Self {
        Self {
            font,
            object_id,
            remapper: subsetter::GlyphRemapper::new(),
            glyphs: BTreeMap::new(),
            codes: BTreeMap::new(),
        }
    }

    /// The bytes a content stream shows for these glyphs.
    pub fn encode(&mut self, glyphs: &[Glyph]) -> Object {
        if let FontData::Type1(afm) = self.font.as_ref() {
            let space = afm.glyph("space");
            let mut bytes = Vec::with_capacity(glyphs.len());
            for glyph in glyphs {
                if Some(glyph.id) == space {
                    bytes.push(32);
                    continue;
                }
                let next = u8::try_from(33 + self.codes.len()).unwrap_or(32);
                let code = *self.codes.entry(glyph.id).or_insert(next);
                bytes.push(code);
            }
            return Object::String(bytes, StringFormat::Literal);
        }
        let mut bytes = Vec::with_capacity(glyphs.len() * 2);
        for glyph in glyphs {
            let id = self.remapper.remap(glyph.id);
            self.glyphs
                .entry(id)
                .or_insert_with(|| (glyph.width, glyph.text.clone()));
            bytes.extend_from_slice(&id.to_be_bytes());
        }
        Object::String(bytes, StringFormat::Hexadecimal)
    }

    /// The font's objects: `(id, object)` pairs, the font dictionary at `object_id`.
    pub fn finish(
        &self,
        mut next_id: impl FnMut() -> lopdf::ObjectId,
    ) -> Result<Vec<(lopdf::ObjectId, Object)>, EditError> {
        match self.font.as_ref() {
            FontData::Type1(afm) => {
                let mut by_code: BTreeMap<u8, u16> =
                    self.codes.iter().map(|(g, c)| (*c, *g)).collect();
                if let Some(space) = afm.glyph("space") {
                    by_code.insert(32, space);
                }
                let first = by_code.keys().next().copied().unwrap_or(32);
                let last = by_code.keys().next_back().copied().unwrap_or(32);
                let width = |code: u8| {
                    by_code
                        .get(&code)
                        .and_then(|g| afm.glyphs.get(usize::from(*g)))
                        .map_or(Object::Integer(0), |(_, w)| real(*w))
                };
                let mut differences = Vec::new();
                let mut previous = None;
                for (code, glyph) in &by_code {
                    if previous.is_none_or(|p: u8| p + 1 != *code) {
                        differences.push(Object::Integer(i64::from(*code)));
                    }
                    let name = afm
                        .glyphs
                        .get(usize::from(*glyph))
                        .map_or("space", |(n, _)| n.as_str());
                    differences.push(Object::Name(name.as_bytes().to_vec()));
                    previous = Some(*code);
                }
                let mut encoding = Dictionary::new();
                encoding.set("Differences", Object::Array(differences));
                let mut dict = Dictionary::new();
                dict.set("Type", Object::Name(b"Font".to_vec()));
                dict.set("Subtype", Object::Name(b"Type1".to_vec()));
                dict.set("BaseFont", Object::Name(afm.name.as_bytes().to_vec()));
                dict.set("FirstChar", Object::Integer(i64::from(first)));
                dict.set("LastChar", Object::Integer(i64::from(last)));
                dict.set("Widths", Object::Array((first..=last).map(width).collect()));
                dict.set("Encoding", Object::Dictionary(encoding));
                Ok(vec![(self.object_id, Object::Dictionary(dict))])
            }
            FontData::TrueType(font) => {
                let subset = subsetter::subset(font.data, 0, &self.remapper)
                    .map_err(|err| EditError::Font(format!("subsetting {}: {err:?}", font.name)))?;
                let tag = subset_tag(&self.glyphs);
                let base_font = format!("{tag}+{}", font.name);
                let file_id = next_id();
                let descriptor_id = next_id();
                let cid_id = next_id();
                let to_unicode_id = next_id();

                let mut file = Dictionary::new();
                file.set(
                    "Length1",
                    Object::Integer(i64::try_from(subset.len()).unwrap_or_default()),
                );
                file.set("Filter", Object::Name(b"FlateDecode".to_vec()));
                let file = Stream::new(file, deflate(&subset)).with_compression(false);

                let mut descriptor = Dictionary::new();
                descriptor.set("Type", Object::Name(b"FontDescriptor".to_vec()));
                descriptor.set("FontName", Object::Name(base_font.as_bytes().to_vec()));
                descriptor.set("Flags", Object::Integer(4));
                descriptor.set(
                    "FontBBox",
                    Object::Array(font.bbox.iter().map(|v| real(*v)).collect()),
                );
                descriptor.set("ItalicAngle", real(font.italic_angle));
                descriptor.set("Ascent", real(font.ascender));
                descriptor.set("Descent", real(font.descender));
                descriptor.set("CapHeight", real(font.cap_height));
                descriptor.set("StemV", Object::Integer(80));
                descriptor.set("FontFile2", Object::Reference(file_id));

                let mut widths = Vec::new();
                for (id, (width, _)) in &self.glyphs {
                    widths.push(Object::Integer(i64::from(*id)));
                    widths.push(Object::Array(vec![real(*width)]));
                }
                let mut system_info = Dictionary::new();
                system_info.set("Registry", Object::string_literal("Adobe"));
                system_info.set("Ordering", Object::string_literal("Identity"));
                system_info.set("Supplement", Object::Integer(0));
                let mut cid = Dictionary::new();
                cid.set("Type", Object::Name(b"Font".to_vec()));
                cid.set("Subtype", Object::Name(b"CIDFontType2".to_vec()));
                cid.set("BaseFont", Object::Name(base_font.as_bytes().to_vec()));
                cid.set("CIDSystemInfo", Object::Dictionary(system_info));
                cid.set("FontDescriptor", Object::Reference(descriptor_id));
                cid.set("DW", Object::Integer(0));
                cid.set("W", Object::Array(widths));
                cid.set("CIDToGIDMap", Object::Name(b"Identity".to_vec()));

                let mut font_dict = Dictionary::new();
                font_dict.set("Type", Object::Name(b"Font".to_vec()));
                font_dict.set("Subtype", Object::Name(b"Type0".to_vec()));
                font_dict.set("BaseFont", Object::Name(base_font.as_bytes().to_vec()));
                font_dict.set("Encoding", Object::Name(b"Identity-H".to_vec()));
                font_dict.set(
                    "DescendantFonts",
                    Object::Array(vec![Object::Reference(cid_id)]),
                );
                font_dict.set("ToUnicode", Object::Reference(to_unicode_id));

                let cmap = to_unicode_cmap(&self.glyphs);
                let mut cmap_dict = Dictionary::new();
                cmap_dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
                let cmap = Stream::new(cmap_dict, deflate(cmap.as_bytes()));

                Ok(vec![
                    (self.object_id, Object::Dictionary(font_dict)),
                    (file_id, Object::Stream(file)),
                    (descriptor_id, Object::Dictionary(descriptor)),
                    (cid_id, Object::Dictionary(cid)),
                    (to_unicode_id, Object::Stream(cmap)),
                ])
            }
        }
    }
}

fn real(value: f64) -> Object {
    #[allow(clippy::cast_possible_truncation, reason = "PDF reals")]
    Object::Real(value as f32)
}

pub(crate) fn deflate(data: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    let _ = encoder.write_all(data);
    encoder.finish().unwrap_or_default()
}

/// Six capital letters derived from the subset's glyphs (ISO 32000-1, 9.6.4).
fn subset_tag(glyphs: &BTreeMap<u16, (f64, String)>) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    for (id, (_, text)) in glyphs {
        hasher.update(id.to_be_bytes());
        hasher.update(text.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .take(6)
        .map(|b| char::from(b'A' + b % 26))
        .collect()
}

fn to_unicode_cmap(glyphs: &BTreeMap<u16, (f64, String)>) -> String {
    use std::fmt::Write as _;
    let mut out = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    let entries: Vec<_> = glyphs
        .iter()
        .filter(|(_, (_, text))| !text.is_empty())
        .collect();
    for chunk in entries.chunks(100) {
        let _ = writeln!(out, "{} beginbfchar", chunk.len());
        for (id, (_, text)) in chunk {
            let hex = text.encode_utf16().fold(String::new(), |mut acc, u| {
                let _ = write!(acc, "{u:04X}");
                acc
            });
            let _ = writeln!(out, "<{id:04X}> <{hex}>");
        }
        out.push_str("endbfchar\n");
    }
    out.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_standard_fonts_with_afm_metrics() {
        let font = load("Helvetica", "none").unwrap();
        let glyphs = font.glyphs("Hé€☃ ");
        let FontData::Type1(afm) = font.as_ref() else {
            panic!()
        };
        let names: Vec<_> = glyphs
            .iter()
            .map(|g| afm.glyphs[usize::from(g.id)].0.as_str())
            .collect();
        assert_eq!(names, ["H", "eacute", "Euro", "space", "space"]);
        assert_eq!(glyphs[0].width, 722.0);
        assert_eq!(font.ascender(), 718.0);
        assert_eq!(font.descender(), -207.0);
    }
}
