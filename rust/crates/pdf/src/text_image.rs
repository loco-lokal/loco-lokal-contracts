//! `Vips::Image.text(markup, width:, height:, wrap: :'word-char')` for the stamp image
//! (`Submitters::CreateStampAttachment`): Pango's layout of regular and bold runs, the
//! autofit search of `vips_text_autofit`, and a coverage mask the size of the ink
//! rectangle.
//!
//! Text is shaped with `harfrust` (`HarfBuzz`, as Pango does) with glyph positions rounded
//! to whole pixels, wrapped at spaces and hyphens, and at characters for words wider than
//! the line, and hinted as `hintslight` has `FreeType` do (`skrifa`'s light hinting). The
//! font is what fontconfig picks for `sans` in the Docker image, `GoNoto` Kurrent. The masks
//! are not byte-identical to Pango and `FreeType`'s: the rasterizer's anti-aliasing differs
//! slightly.

use std::{path::PathBuf, sync::OnceLock};

use ab_glyph_rasterizer::{Point, Rasterizer, point};
use skrifa::{
    FontRef, GlyphId, MetadataProvider as _,
    instance::{LocationRef, Size},
    outline::{
        DrawSettings, Engine, HintingInstance, HintingOptions, OutlinePen, SmoothMode, Target,
    },
    raw::TableProvider as _,
};

/// `<span size="…">` in 1/1024 pt, at `dpi` dots per inch: the font size in pixels.
#[must_use]
pub fn pixel_size(size_1024ths: f64, dpi: i32) -> f64 {
    size_1024ths / 1024.0 * f64::from(dpi) / 72.0
}

/// A run of text, bold (`<b>`) or not. Newlines start paragraphs.
#[derive(Clone, Debug)]
pub struct Run {
    pub text: String,
    pub bold: bool,
}

/// A coverage mask (`vips_text` without `rgba`): one byte per pixel, rows top to bottom.
#[derive(Clone, Debug)]
pub struct Mask {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// `autofit_dpi`.
    pub dpi: i32,
}

struct Face {
    data: Vec<u8>,
}

impl Face {
    fn load(path: &PathBuf) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        FontRef::new(&data).ok()?.head().ok()?;
        Some(Self { data })
    }

    fn font(&self) -> FontRef<'_> {
        FontRef::new(&self.data).expect("parsed at load")
    }

    fn units_per_em(&self) -> f64 {
        self.font()
            .head()
            .map_or(1000.0, |h| f64::from(h.units_per_em()))
    }

    fn glyph_index(&self, c: char) -> Option<u16> {
        let id = self.font().charmap().map(c)?.to_u32();
        u16::try_from(id).ok().filter(|id| *id != 0)
    }

    /// The ascender and descender (negative), in font units (`hhea`, or the typographic
    /// ones with `USE_TYPO_METRICS`).
    fn ascender_descender(&self) -> (f64, f64) {
        let metrics = self
            .font()
            .metrics(Size::unscaled(), LocationRef::default());
        (f64::from(metrics.ascent), f64::from(metrics.descent))
    }

    fn advance(&self, glyph: u16) -> Option<f64> {
        self.font()
            .glyph_metrics(Size::unscaled(), LocationRef::default())
            .advance_width(GlyphId::new(u32::from(glyph)))
            .map(f64::from)
    }

    /// Light hinting at `px` pixels per em, as `hintslight` has `FreeType` do for Pango:
    /// the TrueType interpreter (v40) for fonts with instructions, else the autohinter.
    fn hinting(&self, px: f64) -> Option<HintingInstance> {
        #[allow(clippy::cast_possible_truncation, reason = "pixels per em")]
        let size = Size::new(px as f32);
        let target = Target::Smooth {
            mode: SmoothMode::Light,
            symmetric_rendering: true,
            preserve_linear_metrics: false,
        };
        HintingInstance::new(
            &self.font().outline_glyphs(),
            size,
            LocationRef::default(),
            HintingOptions {
                engine: Engine::AutoFallback,
                target,
            },
        )
        .ok()
    }

    /// Draws the glyph in pixels (y up), hinted when `hinting` is set.
    fn draw(
        &self,
        glyph: u16,
        px: f64,
        hinting: Option<&HintingInstance>,
        pen: &mut impl OutlinePen,
    ) -> bool {
        let font = self.font();
        let Some(outline) = font.outline_glyphs().get(GlyphId::new(u32::from(glyph))) else {
            return false;
        };
        #[allow(clippy::cast_possible_truncation, reason = "pixels per em")]
        let settings = hinting.map_or_else(
            || DrawSettings::unhinted(Size::new(px as f32), LocationRef::default()),
            |instance| DrawSettings::hinted(instance, false),
        );
        outline.draw(settings, pen).is_ok()
    }
}

/// The stamp's fonts: regular and bold.
struct Fonts {
    regular: Face,
    bold: Face,
}

/// What fontconfig resolves `sans` (and `sans bold`) to in the Docker image: `GoNoto`
/// Kurrent from the fonts directory (its Liberation Sans is not a `sans` alias there).
fn fonts() -> Option<&'static Fonts> {
    static FONTS: OnceLock<Option<Fonts>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let load = |file: &str| Face::load(&crate::edit::fonts::fonts_dir().join(file));
            let regular = load("GoNotoKurrent-Regular.ttf")?;
            // Without a bold face, fontconfig falls back to the regular one.
            let bold = load("GoNotoKurrent-Bold.ttf").unwrap_or_else(|| Face {
                data: regular.data.clone(),
            });
            Some(Fonts { regular, bold })
        })
        .as_ref()
}

/// A shaped glyph: its face, id, pen position (pixels, baseline origin) and advance.
#[derive(Clone, Debug)]
struct Placed {
    face: usize,
    glyph: u16,
    x: f64,
    advance: f64,
    /// The character it came from (for line breaking).
    cluster_char: char,
}

struct LineOut {
    glyphs: Vec<Placed>,
    ascent: f64,
    descent: f64,
}

struct Layout {
    lines: Vec<LineOut>,
    faces: Vec<&'static Face>,
    px: f64,
}

impl Layout {
    /// Pango's ink rectangle in whole pixels (`pango_extents_to_pixels`, inclusive):
    /// `(left, top, width, height)`.
    fn ink(&self) -> Option<(i64, i64, i64, i64)> {
        let hinting: Vec<Option<HintingInstance>> =
            self.faces.iter().map(|f| f.hinting(self.px)).collect();
        let mut rect: Option<(f64, f64, f64, f64)> = None;
        let mut baseline = 0.0;
        for line in &self.lines {
            baseline += line.ascent;
            for g in &line.glyphs {
                let mut bounds = BoundsPen::default();
                if !self.faces[g.face].draw(g.glyph, self.px, hinting[g.face].as_ref(), &mut bounds)
                {
                    continue;
                }
                if let Some((x_min, y_min, x_max, y_max)) = bounds.rect {
                    let (x0, x1) = (g.x + x_min, g.x + x_max);
                    let (y0, y1) = (baseline - y_max, baseline - y_min);
                    rect = Some(match rect {
                        None => (x0, y0, x1, y1),
                        Some(r) => (r.0.min(x0), r.1.min(y0), r.2.max(x1), r.3.max(y1)),
                    });
                }
            }
            baseline += line.descent;
        }
        rect.map(|(x0, y0, x1, y1)| {
            #[allow(clippy::cast_possible_truncation, reason = "pixel extents")]
            let (l, t, r, b) = (
                x0.floor() as i64,
                y0.floor() as i64,
                x1.ceil() as i64,
                y1.ceil() as i64,
            );
            (l, t, r - l, b - t)
        })
    }

    fn render(&self, left: i64, top: i64, width: u32, height: u32) -> Vec<u8> {
        let mut coverage = vec![0f32; (width * height) as usize];
        let hinting: Vec<Option<HintingInstance>> =
            self.faces.iter().map(|f| f.hinting(self.px)).collect();
        let mut baseline = 0.0;
        for line in &self.lines {
            baseline += line.ascent;
            for g in &line.glyphs {
                let mut pen = Pen {
                    rasterizer: Rasterizer::new(width as usize, height as usize),
                    #[allow(clippy::cast_precision_loss, reason = "pixels")]
                    origin: (g.x - left as f64, baseline - top as f64),
                    last: point(0.0, 0.0),
                    start: point(0.0, 0.0),
                };
                if !self.faces[g.face].draw(g.glyph, self.px, hinting[g.face].as_ref(), &mut pen) {
                    continue;
                }
                pen.rasterizer.for_each_pixel(|i, v| {
                    coverage[i] = (coverage[i] + v).min(1.0);
                });
            }
            baseline += line.descent;
        }
        coverage
            .into_iter()
            .map(|v| {
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "0..=255"
                )]
                let byte = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                byte
            })
            .collect()
    }
}

/// The control box of an outline (`FT_Outline_Get_CBox`): all points, in pixels.
#[derive(Default)]
struct BoundsPen {
    rect: Option<(f64, f64, f64, f64)>,
}

impl BoundsPen {
    fn add(&mut self, x: f32, y: f32) {
        let (x, y) = (f64::from(x), f64::from(y));
        self.rect = Some(match self.rect {
            None => (x, y, x, y),
            Some(r) => (r.0.min(x), r.1.min(y), r.2.max(x), r.3.max(y)),
        });
    }
}

impl OutlinePen for BoundsPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.add(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.add(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.add(x1, y1);
        self.add(x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.add(x1, y1);
        self.add(x2, y2);
        self.add(x, y);
    }

    fn close(&mut self) {}
}

struct Pen {
    rasterizer: Rasterizer,
    origin: (f64, f64),
    last: Point,
    start: Point,
}

impl Pen {
    #[allow(clippy::cast_possible_truncation, reason = "f32 pixels")]
    fn map(&self, x: f32, y: f32) -> Point {
        point(
            (self.origin.0 + f64::from(x)) as f32,
            (self.origin.1 - f64::from(y)) as f32,
        )
    }
}

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.last = self.map(x, y);
        self.start = self.last;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.map(x, y);
        self.rasterizer.draw_line(self.last, p);
        self.last = p;
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (c, p) = (self.map(x1, y1), self.map(x, y));
        self.rasterizer.draw_quad(self.last, c, p);
        self.last = p;
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (c1, c2, p) = (self.map(x1, y1), self.map(x2, y2), self.map(x, y));
        self.rasterizer.draw_cubic(self.last, c1, c2, p);
        self.last = p;
    }

    fn close(&mut self) {
        if self.last != self.start {
            self.rasterizer.draw_line(self.last, self.start);
        }
        self.last = self.start;
    }
}

/// A character with the face that draws it.
struct Styled {
    c: char,
    face: usize,
}

/// Lays the runs out at `px` pixels with lines `width` pixels wide.
fn layout(fonts: &'static Fonts, runs: &[Run], px: f64, width: f64) -> Layout {
    let faces: Vec<&'static Face> = vec![&fonts.regular, &fonts.bold];
    // Paragraphs of characters with their faces.
    let mut paragraphs: Vec<Vec<Styled>> = vec![Vec::new()];
    for run in runs {
        let face = usize::from(run.bold);
        for c in run.text.chars() {
            if c == '\n' {
                paragraphs.push(Vec::new());
                continue;
            }
            paragraphs
                .last_mut()
                .expect("paragraph")
                .push(Styled { c, face });
        }
    }
    let metrics = |face: usize| {
        let f = faces[face];
        let scale = px / f.units_per_em();
        let (ascender, descender) = f.ascender_descender();
        // Hinted metrics (FreeType rounds the ascender up and the descender down).
        ((ascender * scale).ceil(), (-descender * scale).ceil())
    };
    let mut lines = Vec::new();
    for paragraph in paragraphs {
        let glyphs = shape(&faces, &paragraph, px);
        let mut start = 0;
        while start < glyphs.len() || (start == 0 && glyphs.is_empty()) {
            let hyphen_face = glyphs.get(start).map_or(0, |g| g.face);
            let hyphen = hyphen_glyph(&faces, hyphen_face, px);
            let (end, add_hyphen) = break_line(
                &glyphs,
                start,
                width,
                hyphen.as_ref().map_or(0.0, |h| h.advance),
            );
            let mut line: Vec<Placed> = glyphs[start..end].to_vec();
            let offset = line.first().map_or(0.0, |g| g.x);
            for g in &mut line {
                g.x -= offset;
            }
            if add_hyphen && let Some(mut h) = hyphen {
                h.x = line.last().map_or(0.0, |g| g.x + g.advance);
                line.push(h);
            }
            let used: Vec<usize> = if line.is_empty() {
                paragraph.first().map_or(vec![0], |s| vec![s.face])
            } else {
                line.iter().map(|g| g.face).collect()
            };
            let (ascent, descent) = used
                .iter()
                .map(|f| metrics(*f))
                .fold((0.0_f64, 0.0_f64), |a, m| (a.0.max(m.0), a.1.max(m.1)));
            lines.push(LineOut {
                glyphs: line,
                ascent,
                descent,
            });
            if glyphs.is_empty() {
                break;
            }
            start = end;
            // Spaces at the start of a wrapped line hang at the end of the previous one.
            while start < glyphs.len() && glyphs[start].cluster_char == ' ' {
                start += 1;
            }
        }
    }
    Layout { lines, faces, px }
}

/// The hyphen Pango adds at a break inside a word, shaped in the word's face.
fn hyphen_glyph(faces: &[&'static Face], face: usize, px: f64) -> Option<Placed> {
    let parsed = faces[face];
    let glyph = parsed.glyph_index('-')?;
    let scale = px / parsed.units_per_em();
    let advance = parsed.advance(glyph)? * scale;
    Some(Placed {
        face,
        glyph,
        x: 0.0,
        advance: advance.round(),
        cluster_char: '-',
    })
}

/// Zero-width formatting characters Pango draws nothing for.
fn is_format(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}')
}

/// Shapes a paragraph, a run per face, with advances rounded to whole pixels.
fn shape(faces: &[&'static Face], paragraph: &[Styled], px: f64) -> Vec<Placed> {
    let mut out = Vec::new();
    let mut x = 0.0;
    let mut i = 0;
    while i < paragraph.len() {
        let face_index = paragraph[i].face;
        let mut j = i;
        while j < paragraph.len() && paragraph[j].face == face_index {
            j += 1;
        }
        let text: String = paragraph[i..j]
            .iter()
            .map(|s| s.c)
            .filter(|c| !is_format(*c))
            .collect();
        let chars: Vec<char> = text.chars().collect();
        let face = faces[face_index];
        let scale = px / face.units_per_em();
        let font = face.font();
        let data = harfrust::ShaperData::new(&font);
        let shaper = data.shaper(&font).build();
        let mut buffer = harfrust::UnicodeBuffer::new();
        buffer.push_str(&text);
        buffer.guess_segment_properties();
        let output = shaper.shape(buffer, harfrust::ShapeOptions::new());
        let byte_to_char: Vec<usize> = {
            let mut map = vec![0; text.len() + 1];
            for (index, (byte, _)) in text.char_indices().enumerate() {
                map[byte] = index;
            }
            map
        };
        for (info, pos) in output.glyph_infos().iter().zip(output.glyph_positions()) {
            let advance = (f64::from(pos.x_advance) * scale).round();
            let cluster = byte_to_char[info.cluster as usize];
            out.push(Placed {
                face: face_index,
                #[allow(clippy::cast_possible_truncation, reason = "glyph ids")]
                glyph: info.glyph_id as u16,
                x: x + (f64::from(pos.x_offset) * scale).round(),
                advance,
                cluster_char: chars.get(cluster).copied().unwrap_or(' '),
            });
            x += advance;
        }
        i = j;
    }
    out
}

/// The end of the line starting at `start` (`PANGO_WRAP_WORD_CHAR`): after the last space
/// or hyphen that fits, else as many characters as fit with a hyphen added (Pango's
/// `insert_hyphens`) — at least one. Returns the end and whether to add the hyphen.
fn break_line(glyphs: &[Placed], start: usize, width: f64, hyphen: f64) -> (usize, bool) {
    let origin = glyphs.get(start).map_or(0.0, |g| g.x);
    let mut last_break = None;
    let mut end = start;
    while end < glyphs.len() {
        let g = &glyphs[end];
        let right = g.x + g.advance - origin;
        // Trailing spaces do not count towards the width.
        if g.cluster_char != ' ' && right > width {
            break;
        }
        end += 1;
        if matches!(g.cluster_char, ' ' | '-') {
            last_break = Some(end);
        }
    }
    if end == glyphs.len() {
        return (end, false);
    }
    if let Some(b) = last_break.filter(|b| *b > start) {
        return (b, false);
    }
    // A break inside a word: as many characters as fit together with the hyphen.
    while end > start + 1 {
        let last = &glyphs[end - 1];
        if last.x + last.advance - origin + hyphen <= width {
            break;
        }
        end -= 1;
    }
    (end.max(start + 1), true)
}

/// `vips_text` with `width`, `height` and the autofit search (from 72 dpi) for a
/// `<span size="size_1024ths">` around the runs.
#[must_use]
pub fn text(runs: &[Run], size_1024ths: f64, width: i32, height: i32) -> Option<Mask> {
    let fonts = fonts()?;
    let extents = |dpi: i32| {
        let lay = layout(fonts, runs, pixel_size(size_1024ths, dpi), f64::from(width));
        let ink = lay.ink();
        (lay, ink)
    };
    let fits = |ink: Option<(i64, i64, i64, i64)>| {
        ink.is_none_or(|(_, _, w, h)| w <= i64::from(width) && h <= i64::from(height))
    };
    // `vips_text_autofit`.
    let mut dpi = 72;
    let mut previous_dpi = -1;
    let mut previous_difference = 0;
    let mut difference;
    loop {
        difference = if fits(extents(dpi).1) { -1 } else { 1 };
        if previous_dpi == -1 {
            previous_dpi = dpi;
            previous_difference = difference;
        }
        if difference != previous_difference {
            break;
        }
        previous_difference = difference;
        previous_dpi = dpi;
        dpi = if difference < 0 { dpi * 2 } else { dpi / 2 };
        if !(2..=10000).contains(&dpi) {
            break;
        }
    }
    let (mut lower, mut upper) = if difference < 0 {
        (dpi, previous_dpi)
    } else {
        (previous_dpi, dpi)
    };
    while upper - lower > 1 && difference != 0 {
        dpi = i32::midpoint(upper, lower);
        difference = if fits(extents(dpi).1) { -1 } else { 1 };
        if difference < 0 {
            lower = dpi;
        } else {
            upper = dpi;
        }
    }
    let dpi = if difference == 0 { upper } else { lower };
    let (lay, ink) = extents(dpi);
    let (left, top, w, h) = ink?;
    if w <= 0 || h <= 0 {
        return None;
    }
    let (w, h) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    Some(Mask {
        width: w,
        height: h,
        pixels: lay.render(left, top, w, h),
        dpi,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(name: &str) -> Vec<Run> {
        vec![
            Run {
                text: "\u{200E}Digitally signed by:\n\u{200E}".to_owned(),
                bold: false,
            },
            Run {
                text: name.to_owned(),
                bold: true,
            },
            Run {
                text: "\n\u{200E}May 5, 2026 at 5:05 AM UTC".to_owned(),
                bold: false,
            },
        ]
    }

    #[test]
    fn autofits_like_vips() {
        let Some(mask) = text(&runs("Ann"), 90.0, 400, 200) else {
            return; // no fonts on this machine
        };
        // The doubling search stops above 10000 dpi: the text is always 22.5 px.
        assert_eq!(mask.dpi, 18432);
        assert!((pixel_size(90.0, mask.dpi) - 22.5).abs() < 1e-9);
        assert!(mask.width <= 400 && mask.height <= 200);
        assert_eq!(mask.pixels.len(), (mask.width * mask.height) as usize);
        assert!(mask.pixels.contains(&255));
    }

    #[test]
    fn breaks_long_words_with_a_hyphen() {
        let Some(fonts) = fonts() else { return };
        let long = "a".repeat(60);
        let lay = layout(fonts, &runs(&long), 22.5, 400.0);
        // The long word takes two lines; the first ends with the added hyphen.
        assert_eq!(lay.lines.len(), 5);
        assert_eq!(
            lay.lines[1].glyphs.last().map(|g| g.cluster_char),
            Some('-')
        );
        let right = lay.lines[1].glyphs.last().map_or(0.0, |g| g.x + g.advance);
        assert!(right <= 400.0, "{right}");
    }
}
