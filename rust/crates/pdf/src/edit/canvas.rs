//! Content stream operators for page overlays (`HexaPDF::Content::Canvas`).

use std::fmt::Write as _;

use lopdf::Object;

use super::layout::{Fitted, Item, LineItem};
use super::writer::{real, write_object};

/// A page overlay being drawn. Fonts and images are referenced by resource name.
#[derive(Default)]
pub struct Canvas {
    pub ops: Vec<u8>,
}

/// What an overlay needs to draw text and images.
pub trait Resources {
    /// The `TJ` operand for glyphs in the font registered as `font_key`.
    fn encode(&mut self, font_key: &str, glyphs: &[super::fonts::Glyph]) -> Object;
    /// The resource name, width and height of an image.
    fn image(&self, image: usize) -> String;
}

impl Canvas {
    fn push(&mut self, text: &str) {
        self.ops.extend_from_slice(text.as_bytes());
        self.ops.push(b'\n');
    }

    pub fn fill_rect(&mut self, color: [f64; 3], x: f64, y: f64, w: f64, h: f64) {
        self.push(&format!(
            "q {} rg {} {} {} {} re f Q",
            color_ops(color),
            real(x),
            real(y),
            real(w),
            real(h)
        ));
    }

    pub fn line(&mut self, color: [f64; 3], width: f64, from: (f64, f64), to: (f64, f64)) {
        self.push(&format!(
            "q {} RG {} w {} {} m {} {} l S Q",
            color_ops(color),
            real(width),
            real(from.0),
            real(from.1),
            real(to.0),
            real(to.1)
        ));
    }

    /// `canvas.image(io, at: [x, y], width:, height:)`.
    pub fn image(&mut self, name: &str, x: f64, y: f64, w: f64, h: f64) {
        self.push(&format!(
            "q {} 0 0 {} {} {} cm /{name} Do Q",
            real(w),
            real(h),
            real(x),
            real(y)
        ));
    }

    /// `canvas.xobject(form, at:)` under a transformation (annotation flattening).
    pub fn form(&mut self, name: &str, matrix: [f64; 6]) {
        let m: Vec<String> = matrix.iter().map(|v| real(*v)).collect();
        self.push(&format!("q {} cm /{name} Do Q", m.join(" ")));
    }

    /// `TextLayouter::Result#draw(canvas, x, y)`.
    pub fn draw(&mut self, fitted: &Fitted, x: f64, mut y: f64, resources: &mut dyn Resources) {
        self.push("q");
        for line in &fitted.lines {
            y -= line.y_offset;
            let mut item_x = x + line.x_offset;
            for item in &line.items {
                match item {
                    LineItem::Text(fragment) => {
                        let style = &fragment.style;
                        if style.white_underlay {
                            let (y_min, y_max) = (style.y_min(), style.y_max());
                            let height = y_max.max(0.0) - y_min.min(0.0);
                            self.fill_rect(
                                [1.0; 3],
                                item_x - 1.0,
                                y + y_min,
                                fragment.width() + 2.0,
                                height,
                            );
                        }
                        let mut array = Vec::new();
                        let mut run = Vec::new();
                        for glyph in &fragment.items {
                            match glyph {
                                Item::Glyph(glyph) => run.push(glyph.clone()),
                                Item::Adjust(value) => {
                                    if !run.is_empty() {
                                        array.push(resources.encode(&style.font_key, &run));
                                        run.clear();
                                    }
                                    #[allow(
                                        clippy::cast_possible_truncation,
                                        reason = "PDF reals"
                                    )]
                                    array.push(Object::Real(*value as f32));
                                }
                            }
                        }
                        if !run.is_empty() {
                            array.push(resources.encode(&style.font_key, &run));
                        }
                        if !array.is_empty() {
                            let mut text = String::new();
                            let _ = write!(
                                text,
                                "BT /{} {} Tf {} rg {} {} Td ",
                                style.font_key,
                                real(style.font_size),
                                color_ops(style.fill_color.unwrap_or([0.0; 3])),
                                real(item_x),
                                real(y)
                            );
                            self.ops.extend_from_slice(text.as_bytes());
                            write_object(&mut self.ops, &Object::Array(array));
                            self.push(" TJ ET");
                        }
                        item_x += fragment.width();
                    }
                    LineItem::Inline(inline) => {
                        let name = resources.image(inline.image);
                        self.image(
                            &name,
                            item_x + inline.margin[3],
                            y + inline.margin[2],
                            inline.content_width,
                            inline.draw_height,
                        );
                        item_x += inline.content_width + inline.margin[1] + inline.margin[3];
                    }
                }
            }
        }
        self.push("Q");
    }
}

fn color_ops(color: [f64; 3]) -> String {
    format!("{} {} {}", real(color[0]), real(color[1]), real(color[2]))
}

/// `HexaPDF::Content::ColorSpace.device_color_from_specification`: `rrggbb`, `rgb`, or a
/// CSS color name.
#[must_use]
pub fn parse_color(spec: &str) -> Option<[f64; 3]> {
    let spec = spec.trim();
    let hex = |s: &str| u8::from_str_radix(s, 16).ok().map(|v| f64::from(v) / 255.0);
    if spec.len() == 6 && spec.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some([hex(&spec[0..2])?, hex(&spec[2..4])?, hex(&spec[4..6])?]);
    }
    if spec.len() == 3 && spec.chars().all(|c| c.is_ascii_hexdigit()) {
        let d = |i: usize| hex(&spec[i..=i].repeat(2));
        return Some([d(0)?, d(1)?, d(2)?]);
    }
    let lower = spec.to_ascii_lowercase();
    super::colors::COLOR_NAMES
        .binary_search_by(|(name, _)| (*name).cmp(lower.as_str()))
        .ok()
        .map(|i| {
            super::colors::COLOR_NAMES[i]
                .1
                .map(|v| f64::from(v) / 255.0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colors() {
        assert_eq!(parse_color("ff0000"), Some([1.0, 0.0, 0.0]));
        assert_eq!(parse_color("0f0"), Some([0.0, 1.0, 0.0]));
        assert_eq!(parse_color("red"), Some([1.0, 0.0, 0.0]));
        assert_eq!(parse_color("nope"), None);
    }
}
