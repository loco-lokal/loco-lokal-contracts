//! Text layout: a port of the parts of `HexaPDF::Layout::TextLayouter` the result
//! documents and the audit trail use (simple segmentation, fixed-width line wrapping,
//! proportional line spacing, text and baseline-aligned inline boxes, and
//! left/center/right/justify alignment).

use std::sync::Arc;

use super::fonts::{FontData, Glyph};

/// A run of glyphs in one style (`TextFragment`).
#[derive(Clone)]
pub struct Fragment {
    pub style: Arc<TextStyle>,
    pub items: Vec<Item>,
}

/// A glyph, or a `TJ` adjustment in 1/1000 em (positive moves left, as in PDF).
#[derive(Clone, Debug)]
pub enum Item {
    Glyph(Glyph),
    Adjust(f64),
}

/// The style of a fragment.
pub struct TextStyle {
    pub font: Arc<FontData>,
    /// Resource key the font is registered under in the document.
    pub font_key: String,
    pub font_size: f64,
    pub line_height: Option<f64>,
    pub fill_color: Option<[f64; 3]>,
    /// A white rectangle behind the text (the Document ID footer's underlay).
    pub white_underlay: bool,
    pub underline: bool,
    /// A URI the text links to (a `[:link, {uri:}]` overlay).
    pub link: Option<String>,
}

impl TextStyle {
    fn scale(&self) -> f64 {
        self.font_size / 1000.0
    }

    #[must_use]
    pub fn y_max(&self) -> f64 {
        self.font.ascender() * self.scale() * self.line_height.unwrap_or(self.font_size)
            / self.font_size
    }

    #[must_use]
    pub fn y_min(&self) -> f64 {
        self.font.descender() * self.scale() * self.line_height.unwrap_or(self.font_size)
            / self.font_size
    }
}

impl Fragment {
    /// `TextFragment.create(text, style)`.
    #[must_use]
    pub fn new(text: &str, style: &Arc<TextStyle>) -> Self {
        Self {
            style: style.clone(),
            items: style
                .font
                .glyphs(text)
                .into_iter()
                .map(Item::Glyph)
                .collect(),
        }
    }

    fn with_items(&self, items: Vec<Item>) -> Self {
        Self {
            style: self.style.clone(),
            items,
        }
    }

    fn item_width(&self, item: &Item) -> f64 {
        match item {
            Item::Glyph(glyph) => glyph.width * self.style.scale(),
            Item::Adjust(value) => -value * self.style.scale(),
        }
    }

    #[must_use]
    pub fn width(&self) -> f64 {
        self.items.iter().map(|item| self.item_width(item)).sum()
    }

    fn height(&self) -> f64 {
        self.style.y_max().max(0.0) - self.style.y_min().min(0.0)
    }
}

/// A box drawn inline with the text, aligned on the baseline (`InlineBox`): the paperclip
/// icon in front of attachment names.
#[derive(Clone, Debug)]
pub struct InlineImage {
    /// The image's key in the document.
    pub image: usize,
    /// Content width; the height follows the image's aspect ratio.
    pub content_width: f64,
    pub content_height: f64,
    /// The drawn image's height (its aspect ratio at `content_width`).
    pub draw_height: f64,
    /// Margins: top, right, bottom, left.
    pub margin: [f64; 4],
}

impl InlineImage {
    fn width(&self) -> f64 {
        self.content_width + self.margin[1] + self.margin[3]
    }

    fn height(&self) -> f64 {
        self.content_height + self.margin[0] + self.margin[2]
    }
}

/// One item of a line.
#[derive(Clone)]
pub enum LineItem {
    Text(Fragment),
    Inline(InlineImage),
}

impl LineItem {
    fn width(&self) -> f64 {
        match self {
            Self::Text(fragment) => fragment.width(),
            Self::Inline(inline) => inline.width(),
        }
    }

    fn height(&self) -> f64 {
        match self {
            Self::Text(fragment) => fragment.height(),
            Self::Inline(inline) => inline.height(),
        }
    }
}

/// What to lay out: text fragments and inline boxes.
#[derive(Clone)]
pub enum Input {
    Text(Fragment),
    Inline(InlineImage),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
    Justify,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Valign {
    Top,
    Center,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Success,
    Height,
    BoxTooWide,
}

/// A laid-out line.
#[derive(Clone)]
pub struct Line {
    pub items: Vec<LineItem>,
    pub x_offset: f64,
    pub y_offset: f64,
    ignore_justification: bool,
}

impl Line {
    fn new(items: Vec<LineItem>) -> Self {
        // `Line#add` merges consecutive fragments of the same style.
        let mut merged: Vec<LineItem> = Vec::with_capacity(items.len());
        for item in items {
            match (merged.last_mut(), item) {
                (Some(LineItem::Text(last)), LineItem::Text(fragment))
                    if Arc::ptr_eq(&last.style, &fragment.style) =>
                {
                    last.items.extend(fragment.items);
                }
                (_, item) => merged.push(item),
            }
        }
        Self {
            items: merged,
            x_offset: 0.0,
            y_offset: 0.0,
            ignore_justification: false,
        }
    }

    #[must_use]
    pub fn width(&self) -> f64 {
        self.items.iter().map(LineItem::width).sum()
    }

    /// `HeightCalculator#result`: `(y_min, y_max)`.
    fn dimensions(&self) -> (f64, f64) {
        let (mut text_min, mut text_max, mut base): (f64, f64, f64) = (0.0, 0.0, 0.0);
        for item in &self.items {
            match item {
                LineItem::Text(fragment) => {
                    text_min = text_min.min(fragment.style.y_min());
                    text_max = text_max.max(fragment.style.y_max());
                }
                LineItem::Inline(inline) => base = base.max(inline.height()),
            }
        }
        (text_min, text_min.max(base).max(text_max))
    }

    #[must_use]
    pub fn y_min(&self) -> f64 {
        self.dimensions().0
    }

    #[must_use]
    pub fn y_max(&self) -> f64 {
        self.dimensions().1
    }

    #[must_use]
    pub fn height(&self) -> f64 {
        let (min, max) = self.dimensions();
        max - min
    }

    #[must_use]
    pub fn starts_with_inline(&self) -> bool {
        matches!(self.items.first(), Some(LineItem::Inline(_)))
    }
}

/// The result of `TextLayouter#fit`.
pub struct Fitted {
    pub status: Status,
    pub lines: Vec<Line>,
    /// What did not fit (`remaining_items`), to continue in another box.
    pub remaining: Vec<Segment>,
}

impl Fitted {
    #[must_use]
    pub fn lines_height(&self) -> f64 {
        self.lines.iter().map(Line::height).sum()
    }

    /// `Result#height`: the baseline offsets plus the last line's descent.
    #[must_use]
    pub fn height(&self) -> f64 {
        self.lines.iter().map(|l| l.y_offset).sum::<f64>()
            - self.lines.last().map_or(0.0, Line::y_min)
    }
}

/// A text segment: a box, glue, or penalty of `SimpleTextSegmentation`.
#[derive(Clone)]
pub enum Segment {
    Box(LineItem),
    Glue(Fragment),
    Penalty {
        penalty: i64,
        width: f64,
        item: Option<Fragment>,
    },
}

const INFINITY: i64 = 1000;
const PARAGRAPH_BREAK: i64 = -INFINITY - 1_000_000;
const LINE_BREAK: i64 = -INFINITY - 1_000_001;

impl Segment {
    fn width(&self) -> f64 {
        match self {
            Self::Box(item) => item.width(),
            Self::Glue(fragment) => fragment.width(),
            Self::Penalty { width, .. } => *width,
        }
    }

    fn height(&self) -> f64 {
        match self {
            Self::Box(item) => item.height(),
            Self::Glue(fragment) => fragment.height(),
            Self::Penalty { item, .. } => item.as_ref().map_or(0.0, Fragment::height),
        }
    }
}

fn is_break(text: &str) -> bool {
    matches!(
        text,
        " " | "\n"
            | "\u{B}"
            | "\u{C}"
            | "\r"
            | "\u{85}"
            | "\u{2028}"
            | "\u{2029}"
            | "\t"
            | "\u{200B}"
            | "\u{AD}"
            | "\u{A0}"
            | "-"
    )
}

/// `SimpleTextSegmentation`.
#[must_use]
pub fn segment(items: Vec<Input>) -> Vec<Segment> {
    let mut result = Vec::new();
    for input in items {
        let fragment = match input {
            Input::Inline(inline) => {
                result.push(Segment::Box(LineItem::Inline(inline)));
                continue;
            }
            Input::Text(fragment) => fragment,
        };
        let glyph_text = |item: &Item| match item {
            Item::Glyph(glyph) => Some(glyph.text.clone()),
            Item::Adjust(_) => None,
        };
        let mut i = 0;
        let items = &fragment.items;
        while i < items.len() {
            let mut box_items = Vec::new();
            while let Some(item) = items.get(i) {
                if glyph_text(item).is_some_and(|text| is_break(&text)) {
                    break;
                }
                box_items.push(item.clone());
                i += 1;
            }
            let current = items.get(i);
            let text = current.and_then(glyph_text);
            if text.as_deref() == Some("-") {
                box_items.push(current.cloned().expect("current item"));
            }
            if !box_items.is_empty() {
                result.push(Segment::Box(LineItem::Text(fragment.with_items(box_items))));
            }
            if let (Some(item), Some(text)) = (current, text) {
                let single = |text: &str| {
                    fragment.with_items(
                        fragment
                            .style
                            .font
                            .glyphs(text)
                            .into_iter()
                            .map(Item::Glyph)
                            .collect(),
                    )
                };
                match text.as_str() {
                    " " => result.push(Segment::Glue(fragment.with_items(vec![item.clone()]))),
                    "\n" | "\u{B}" | "\u{C}" | "\u{85}" | "\u{2029}" => {
                        result.push(Segment::Penalty {
                            penalty: PARAGRAPH_BREAK,
                            width: 0.0,
                            item: None,
                        });
                    }
                    "\u{2028}" => result.push(Segment::Penalty {
                        penalty: LINE_BREAK,
                        width: 0.0,
                        item: None,
                    }),
                    "\r" => {
                        let next_is_newline = items
                            .get(i + 1)
                            .and_then(glyph_text)
                            .is_some_and(|t| t == "\n");
                        if !next_is_newline {
                            result.push(Segment::Penalty {
                                penalty: PARAGRAPH_BREAK,
                                width: 0.0,
                                item: None,
                            });
                        }
                    }
                    "-" => result.push(Segment::Penalty {
                        penalty: 50,
                        width: 0.0,
                        item: None,
                    }),
                    "\t" => result.push(Segment::Glue(single("        "))),
                    "\u{AD}" => {
                        let frag = single("-");
                        result.push(Segment::Penalty {
                            penalty: 50,
                            width: frag.width(),
                            item: Some(frag),
                        });
                    }
                    "\u{A0}" => {
                        let frag = single(" ");
                        result.push(Segment::Penalty {
                            penalty: INFINITY,
                            width: frag.width(),
                            item: Some(frag),
                        });
                    }
                    "\u{200B}" => result.push(Segment::Penalty {
                        penalty: 0,
                        width: 0.0,
                        item: None,
                    }),
                    _ => {}
                }
            }
            i += 1;
        }
    }
    result
}

/// `SimpleLineWrapping#fixed_width_wrapping`, with the line callback of `fit`.
struct Wrapper<'a> {
    items: &'a [Segment],
    available: f64,
    line_items: Vec<LineItem>,
    glue_items: Vec<LineItem>,
    width: f64,
    beginning: usize,
    last_breakpoint: usize,
    last_breakpoint_line_items: usize,
    break_prohibited: bool,
}

enum Yield {
    Continue,
    Stop,
}

impl Wrapper<'_> {
    fn add_box(&mut self, item: &LineItem) -> bool {
        if self.width + item.width() > self.available {
            return false;
        }
        self.line_items.append(&mut self.glue_items);
        self.line_items.push(item.clone());
        self.width += item.width();
        true
    }

    fn add_glue(&mut self, item: &Fragment, index: usize) -> bool {
        if self.width + item.width() > self.available {
            return false;
        }
        if !self.line_items.is_empty() {
            self.glue_items.push(LineItem::Text(item.clone()));
            self.width += item.width();
            self.update_last_breakpoint(index);
        }
        true
    }

    fn update_last_breakpoint(&mut self, index: usize) {
        self.break_prohibited = false;
        self.last_breakpoint = index;
        self.last_breakpoint_line_items = self.line_items.len();
    }

    fn reset_to_last_breakpoint(&mut self) -> usize {
        self.line_items.truncate(self.last_breakpoint_line_items);
        self.break_prohibited = false;
        self.last_breakpoint
    }

    fn take_line(&mut self, justify: bool) -> Line {
        let mut line = Line::new(std::mem::take(&mut self.line_items));
        line.ignore_justification = !justify;
        line
    }

    fn reset_after_break(&mut self, index: usize) {
        self.beginning = index;
        self.line_items.clear();
        self.width = 0.0;
        self.glue_items.clear();
        self.last_breakpoint = index;
        self.last_breakpoint_line_items = 0;
        self.break_prohibited = false;
    }

    /// Returns the index of the first remaining item (`items.len()` when all were used).
    fn run(&mut self, mut on_line: impl FnMut(Line, Option<&Segment>) -> Yield) -> usize {
        let mut index = 0;
        while let Some(item) = self.items.get(index) {
            match item {
                Segment::Box(line_item) => {
                    if !self.add_box(line_item) {
                        if self.break_prohibited {
                            index = self.reset_to_last_breakpoint();
                        }
                        let line = self.take_line(true);
                        if let Yield::Stop = on_line(line, self.items.get(index)) {
                            return self.beginning;
                        }
                        self.reset_after_break(index);
                        continue; // redo
                    }
                }
                Segment::Glue(fragment) => {
                    if !self.add_glue(fragment, index) {
                        let line = self.take_line(true);
                        if let Yield::Stop = on_line(line, Some(item)) {
                            return self.beginning;
                        }
                        self.reset_after_break(index + 1);
                    }
                }
                Segment::Penalty {
                    penalty,
                    width,
                    item: frag,
                } => {
                    if *penalty <= -INFINITY {
                        if *width > 0.0
                            && let Some(frag) = frag
                        {
                            self.add_box(&LineItem::Text(frag.clone()));
                        }
                        let line = self.take_line(false);
                        if let Yield::Stop = on_line(line, Some(item)) {
                            return self.beginning;
                        }
                        self.reset_after_break(index + 1);
                    } else if *penalty >= INFINITY {
                        self.break_prohibited = true;
                        if *width > 0.0
                            && let Some(frag) = frag
                        {
                            self.add_box(&LineItem::Text(frag.clone()));
                        }
                    } else if *width > 0.0 {
                        if self.width + width <= self.available {
                            let mut next_index = index + 1;
                            while matches!(
                                self.items.get(next_index),
                                Some(Segment::Penalty { .. })
                            ) {
                                next_index += 1;
                            }
                            if let Some(next) = self.items.get(next_index)
                                && self.width + next.width() > self.available
                            {
                                self.line_items.append(&mut self.glue_items);
                                if let Some(frag) = frag {
                                    self.line_items.push(LineItem::Text(frag.clone()));
                                }
                                self.width += width;
                            }
                            self.update_last_breakpoint(index);
                        } else {
                            self.break_prohibited = true;
                        }
                    } else {
                        self.update_last_breakpoint(index);
                    }
                }
            }
            index += 1;
        }
        let line = self.take_line(false);
        if line.items.is_empty() {
            return self.items.len();
        }
        match on_line(line, None) {
            Yield::Continue => self.items.len(),
            Yield::Stop => self.beginning,
        }
    }
}

/// `TextLayouter` with a style of `text_align`/`text_valign` and proportional
/// `line_spacing`.
#[derive(Clone, Copy, Debug)]
pub struct Layouter {
    pub align: Align,
    pub valign: Valign,
    /// Proportional line spacing (1 is single).
    pub line_spacing: f64,
}

impl Layouter {
    #[must_use]
    pub fn new(align: Align, valign: Valign) -> Self {
        Self {
            align,
            valign,
            line_spacing: 1.0,
        }
    }

    #[must_use]
    pub fn with_line_spacing(mut self, line_spacing: f64) -> Self {
        self.line_spacing = line_spacing;
        self
    }

    /// `fit(items, width, height)`; `empty` is the style a blank line's empty fragment
    /// takes (the layouter's own font and size).
    #[must_use]
    pub fn fit(
        &self,
        items: Vec<Input>,
        width: f64,
        height: f64,
        empty: &Arc<TextStyle>,
    ) -> Fitted {
        self.fit_segments(segment(items), width, height, empty)
    }

    /// `fit` for items already segmented (`remaining_items` of an earlier fit).
    #[must_use]
    pub fn fit_segments(
        &self,
        mut rest: Vec<Segment>,
        width: f64,
        height: f64,
        empty: &Arc<TextStyle>,
    ) -> Fitted {
        let mut lines: Vec<Line> = Vec::new();
        let mut actual_height = 0.0;
        let mut previous: Option<(f64, f64)> = None;
        let status;
        loop {
            let mut too_wide: Option<usize> = None;
            let mut wrapper = Wrapper {
                items: &rest,
                available: width,
                line_items: Vec::new(),
                glue_items: Vec::new(),
                width: 0.0,
                beginning: 0,
                last_breakpoint: 0,
                last_breakpoint_line_items: 0,
                break_prohibited: false,
            };
            let segments = &rest;
            let remaining = wrapper.run(|mut line, item| {
                let is_box = matches!(item, Some(Segment::Box(_)));
                if !is_box && line.items.is_empty() {
                    line.items.push(LineItem::Text(Fragment {
                        style: empty.clone(),
                        items: Vec::new(),
                    }));
                }
                if line.items.is_empty() {
                    // A box that does not fit on an empty line.
                    let item = item.expect("box");
                    if actual_height + item.height() <= height {
                        too_wide = segments.iter().position(|s| std::ptr::eq(s, item));
                    }
                    return Yield::Stop;
                }
                let (y_min, y_max) = line.dimensions();
                // `LineSpacing#gap` and `#baseline_distance` (proportional).
                let distance = previous.map(|(prev_min, _): (f64, f64)| prev_min.abs() + y_max);
                let gap = distance.map_or(0.0, |d| d * (self.line_spacing - 1.0));
                let new_height = actual_height + (y_max - y_min) + gap;
                if new_height > height {
                    return Yield::Stop;
                }
                line.x_offset = self.horizontal_offset(&mut line, width);
                line.y_offset = distance.map_or(0.0, |d| d * self.line_spacing);
                previous = Some((y_min, y_max));
                actual_height = new_height;
                lines.push(line);
                Yield::Continue
            });
            if let Some(index) = too_wide
                && let Some(Segment::Box(LineItem::Text(fragment))) = rest.get(index)
                && fragment.items.len() > 1
            {
                let split: Vec<Segment> = fragment
                    .items
                    .iter()
                    .map(|item| {
                        Segment::Box(LineItem::Text(fragment.with_items(vec![item.clone()])))
                    })
                    .collect();
                let tail = rest.split_off(index + 1);
                rest = split;
                rest.extend(tail);
                continue;
            }
            rest = rest.split_off(remaining.min(rest.len()));
            status = if too_wide.is_some() {
                Status::BoxTooWide
            } else if rest.is_empty() {
                Status::Success
            } else {
                Status::Height
            };
            break;
        }
        if let Some(first) = lines.first_mut() {
            let y_max = first.y_max();
            first.y_offset += match self.valign {
                Valign::Top => y_max,
                Valign::Center => (height - actual_height) / 2.0 + y_max,
                Valign::Bottom => (height - actual_height) + y_max,
            };
        }
        Fitted {
            status,
            lines,
            remaining: rest,
        }
    }

    fn horizontal_offset(self, line: &mut Line, available: f64) -> f64 {
        match self.align {
            Align::Left => 0.0,
            Align::Center => (available - line.width()) / 2.0,
            Align::Right => available - line.width(),
            Align::Justify => {
                justify(line, available);
                0.0
            }
        }
    }
}

/// `justify_line`: widens the spaces so the line fills the width.
fn justify(line: &mut Line, width: f64) {
    if line.ignore_justification || (width - line.width()).abs() < 0.001 {
        return;
    }
    let mut sum = 0.0;
    for item in &line.items {
        if let LineItem::Text(fragment) = item {
            for glyph in &fragment.items {
                if let Item::Glyph(glyph) = glyph
                    && glyph.text == " "
                {
                    sum += glyph.width * fragment.style.scale();
                }
            }
        }
    }
    if sum <= 0.0 {
        return;
    }
    let adjustment = (width - line.width()) / sum;
    for item in &mut line.items {
        if let LineItem::Text(fragment) = item {
            let mut items = Vec::with_capacity(fragment.items.len());
            for glyph in fragment.items.drain(..) {
                if let Item::Glyph(g) = &glyph
                    && g.text == " "
                {
                    items.push(Item::Adjust(-g.width * adjustment));
                }
                items.push(glyph);
            }
            fragment.items = items;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::fonts;

    fn style(size: f64) -> Arc<TextStyle> {
        Arc::new(TextStyle {
            font: fonts::load("Helvetica", "none").unwrap(),
            font_key: "F1".to_owned(),
            font_size: size,
            line_height: None,
            fill_color: None,
            white_underlay: false,
            underline: false,
            link: None,
        })
    }

    #[test]
    fn spaces_lines_proportionally() {
        let s = style(10.0);
        let layouter = Layouter::new(Align::Left, Valign::Top).with_line_spacing(1.3);
        let fitted = layouter.fit(
            vec![Input::Text(Fragment::new("a\nb", &s))],
            100.0,
            100.0,
            &s,
        );
        assert_eq!(fitted.lines.len(), 2);
        assert!((fitted.lines[1].y_offset - (2.07 + 7.18) * 1.3).abs() < 1e-9);
        assert!((fitted.height() - (7.18 + (2.07 + 7.18) * 1.3 + 2.07)).abs() < 1e-9);

        let fitted = layouter.fit(
            vec![Input::Text(Fragment::new("a\nb", &s))],
            100.0,
            12.0,
            &s,
        );
        assert_eq!(fitted.status, Status::Height);
        assert_eq!(fitted.remaining.len(), 1);
    }

    #[test]
    fn wraps_words_and_reports_overflow() {
        let s = style(10.0);
        let layouter = Layouter::new(Align::Left, Valign::Top);
        let fitted = layouter.fit(
            vec![Input::Text(Fragment::new("hello world again", &s))],
            50.0,
            100.0,
            &s,
        );
        assert_eq!(fitted.status, Status::Success);
        assert_eq!(fitted.lines.len(), 2);
        // Helvetica: ascender 718, descender -207.
        assert!((fitted.lines[0].y_offset - 7.18).abs() < 1e-9);
        assert!((fitted.lines[1].y_offset - (2.07 + 7.18)).abs() < 1e-9);

        let fitted = layouter.fit(
            vec![Input::Text(Fragment::new("hello world again", &s))],
            50.0,
            12.0,
            &s,
        );
        assert_eq!(fitted.status, Status::Height);
        assert_eq!(fitted.lines.len(), 1);
    }

    #[test]
    fn splits_words_wider_than_the_line() {
        let s = style(10.0);
        let layouter = Layouter::new(Align::Center, Valign::Center);
        let fitted = layouter.fit(
            vec![Input::Text(Fragment::new("abcdefghij", &s))],
            20.0,
            100.0,
            &s,
        );
        assert_eq!(fitted.status, Status::Success);
        assert!(fitted.lines.len() > 1);
    }

    #[test]
    fn keeps_blank_lines() {
        let s = style(10.0);
        let layouter = Layouter::new(Align::Left, Valign::Top);
        let fitted = layouter.fit(
            vec![Input::Text(Fragment::new("a\n\nb", &s))],
            100.0,
            100.0,
            &s,
        );
        assert_eq!(fitted.lines.len(), 3);
    }
}
