//! Laying out new documents: a port of the parts of `HexaPDF::Composer` and
//! `HexaPDF::Layout` that `Submissions::GenerateAuditTrail` uses. Boxes (text, images,
//! a top border, single columns, and tables) are fitted into a [`Frame`] the way
//! `Frame#fit` places them, and break across pages as `Composer#draw_box` does.
//!
//! A frame's shape is kept as horizontal bands of x-intervals instead of a general
//! polygon: every area HexaPDF removes from it is an axis-aligned rectangle.

use std::sync::Arc;

use lopdf::ObjectId;

use super::{
    EditError, Editor,
    layout::{Fitted, Input, Layouter, Segment, Status, TextStyle, segment},
    writer::real,
};

/// `HexaPDF::Utils::EPSILON`.
const EPSILON: f64 = 1e-6;

fn float_equal(a: f64, b: f64) -> bool {
    (a - b).abs() < EPSILON
}

/// `float_compare(a, b) > 0`.
fn exceeds(a: f64, b: f64) -> bool {
    !float_equal(a, b) && a > b
}

/// Top, right, bottom and left values (`Style::Quad`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Quad {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

impl Quad {
    /// `Quad.new([top, right, bottom, left])`.
    #[must_use]
    pub fn new(top: f64, right: f64, bottom: f64, left: f64) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }
}

/// `style.position`: in the flow (masking the frame's full width), or floating (masking
/// only the box).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Position {
    #[default]
    Default,
    Float,
}

/// `style.align`: where a box narrower than the region sits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoxAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// The box properties of a `Style`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BoxStyle {
    /// `None` when no margin was set (`style.margin?`).
    pub margin: Option<Quad>,
    pub padding: Quad,
    pub position: Position,
    pub align: BoxAlign,
}

impl BoxStyle {
    #[must_use]
    pub fn padding(mut self, padding: Quad) -> Self {
        self.padding = padding;
        self
    }

    #[must_use]
    pub fn margin(mut self, margin: Quad) -> Self {
        self.margin = Some(margin);
        self
    }

    #[must_use]
    pub fn float(mut self) -> Self {
        self.position = Position::Float;
        self
    }

    fn reserved_width(&self) -> f64 {
        self.padding.left + self.padding.right
    }

    fn reserved_height(&self) -> f64 {
        self.padding.top + self.padding.bottom
    }
}

/// Text in a box (`TextBox`).
#[derive(Clone)]
pub struct TextBox {
    pub style: BoxStyle,
    /// Fixed width and height; 0 to size the box to its text.
    pub width: f64,
    pub height: f64,
    layouter: Layouter,
    empty: Arc<TextStyle>,
    items: Vec<Segment>,
}

impl TextBox {
    /// `items` laid out by `layouter`; `empty` is the box's own text style.
    #[must_use]
    pub fn new(
        items: Vec<Input>,
        layouter: Layouter,
        empty: Arc<TextStyle>,
        style: BoxStyle,
    ) -> Self {
        Self {
            style,
            width: 0.0,
            height: 0.0,
            layouter,
            empty,
            items: segment(items),
        }
    }

    #[must_use]
    pub fn width(mut self, width: f64) -> Self {
        self.width = width;
        self
    }
}

/// An image (`ImageBox`) of a fixed size.
#[derive(Clone)]
pub struct ImageBox {
    pub style: BoxStyle,
    /// The image's index in the editor.
    pub image: usize,
    pub width: f64,
    pub height: f64,
}

/// An empty box `height` high with a top border (the audit trail's divider).
#[derive(Clone)]
pub struct RuleBox {
    pub style: BoxStyle,
    pub height: f64,
    pub border_width: f64,
    pub color: [f64; 3],
}

/// A one-column `ColumnBox`: its children are fitted into a frame of their own, so
/// floats can sit side by side.
#[derive(Clone)]
pub struct ColumnBox {
    pub style: BoxStyle,
    pub children: Vec<Content>,
}

/// A table (`TableBox`) of equal-width columns and borderless cells, each holding one
/// box, that breaks between rows.
#[derive(Clone)]
pub struct TableBox {
    pub style: BoxStyle,
    pub cell_padding: Quad,
    pub rows: Vec<Vec<Option<Content>>>,
    start: usize,
}

impl TableBox {
    #[must_use]
    pub fn new(rows: Vec<Vec<Option<Content>>>, cell_padding: Quad) -> Self {
        Self {
            style: BoxStyle::default(),
            cell_padding,
            rows,
            start: 0,
        }
    }
}

/// A box to lay out.
#[derive(Clone)]
pub enum Content {
    Text(TextBox),
    Image(ImageBox),
    Rule(RuleBox),
    Column(ColumnBox),
    Table(TableBox),
}

impl Content {
    fn style(&self) -> &BoxStyle {
        match self {
            Self::Text(b) => &b.style,
            Self::Image(b) => &b.style,
            Self::Rule(b) => &b.style,
            Self::Column(b) => &b.style,
            Self::Table(b) => &b.style,
        }
    }

    fn initial_size(&self) -> (f64, f64) {
        match self {
            Self::Text(b) => (b.width, b.height),
            Self::Image(b) => (b.width, b.height),
            Self::Rule(b) => (0.0, b.height),
            Self::Column(_) | Self::Table(_) => (0.0, 0.0),
        }
    }
}

/// Something to draw, relative to the bottom-left corner of its box.
enum Paint {
    /// Text whose first line hangs from `top`.
    Text { fitted: Fitted, x: f64, top: f64 },
    Image {
        image: usize,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
    /// `Border#draw_complex_border` for a top border only.
    TopBorder {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        border_width: f64,
        color: [f64; 3],
    },
}

impl Paint {
    fn translate(&mut self, dx: f64, dy: f64) {
        match self {
            Self::Text { x, top, .. } => {
                *x += dx;
                *top += dy;
            }
            Self::Image { x, y, .. } | Self::TopBorder { x, y, .. } => {
                *x += dx;
                *y += dy;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FitStatus {
    Success,
    Overflow,
    Failure,
}

/// A box fitted into a frame (`FitResult`): its size, what to draw, and, once placed,
/// its position and the area it takes from the frame.
struct Fit {
    status: FitStatus,
    width: f64,
    height: f64,
    paints: Vec<Paint>,
    /// What did not fit, to continue elsewhere (`split_content`).
    rest: Option<Content>,
    x: f64,
    y: f64,
    mask: Mask,
}

impl Fit {
    fn failure() -> Self {
        Self {
            status: FitStatus::Failure,
            width: 0.0,
            height: 0.0,
            paints: Vec::new(),
            rest: None,
            x: 0.0,
            y: 0.0,
            mask: Mask::rectangle(0.0, 0.0, 0.0, 0.0),
        }
    }

    /// The paints at the box's position.
    fn placed(self) -> Vec<Paint> {
        if self.width == 0.0 || self.height == 0.0 {
            return Vec::new();
        }
        let (x, y) = (self.x, self.y);
        self.paints
            .into_iter()
            .map(|mut p| {
                p.translate(x, y);
                p
            })
            .collect()
    }
}

/// An area removed from a frame: a `Geom2D::Rectangle`, or a polygon of the same shape
/// (which `remove_area` treats differently).
#[derive(Clone, Copy, Debug)]
struct Mask {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    is_rectangle: bool,
}

impl Mask {
    fn rectangle(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
            is_rectangle: true,
        }
    }

    /// `create_rectangle(blx, bly, trx, try)`.
    fn corners(blx: f64, bly: f64, trx: f64, try_: f64) -> Self {
        Self::rectangle(blx, bly, trx - blx, try_ - bly)
    }
}

/// A horizontal band of the shape: the x-intervals covered between `y0` and `y1`.
#[derive(Clone, Debug)]
struct Band {
    y0: f64,
    y1: f64,
    xs: Vec<(f64, f64)>,
}

/// A horizontal edge of the shape (`Geom2D::Segment`).
#[derive(Clone, Copy, Debug)]
struct Edge {
    y: f64,
    x0: f64,
    x1: f64,
}

impl Edge {
    fn length(&self) -> f64 {
        self.x1 - self.x0
    }
}

fn subtract_interval(xs: &[(f64, f64)], from: f64, to: f64) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for &(a, b) in xs {
        if to <= a + EPSILON || from >= b - EPSILON {
            out.push((a, b));
            continue;
        }
        if from > a + EPSILON {
            out.push((a, from));
        }
        if to < b - EPSILON {
            out.push((to, b));
        }
    }
    out
}

fn subtract_intervals(xs: &[(f64, f64)], other: &[(f64, f64)]) -> Vec<(f64, f64)> {
    other
        .iter()
        .fold(xs.to_vec(), |acc, &(a, b)| subtract_interval(&acc, a, b))
}

fn same_intervals(a: &[(f64, f64)], b: &[(f64, f64)]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(p, q)| float_equal(p.0, q.0) && float_equal(p.1, q.1))
}

/// A frame's shape.
#[derive(Clone, Debug)]
struct Shape {
    bands: Vec<Band>,
    /// Still a `Geom2D::Rectangle` (only shrunk from the top).
    rectangle: bool,
}

impl Shape {
    fn rectangle(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            bands: vec![Band {
                y0: y,
                y1: y + height,
                xs: vec![(x, x + width)],
            }],
            rectangle: true,
        }
    }

    /// The rectangle's `x, y, width, height`.
    fn bounds(&self) -> Option<(f64, f64, f64, f64)> {
        let band = self.bands.first()?;
        let &(x0, x1) = band.xs.first()?;
        Some((x0, band.y0, x1 - x0, band.y1 - band.y0))
    }

    fn subtract(&mut self, mask: &Mask) {
        let (rx0, ry0, rx1, ry1) = (mask.x, mask.y, mask.x + mask.width, mask.y + mask.height);
        let mut out = Vec::with_capacity(self.bands.len() + 2);
        for band in self.bands.drain(..) {
            if band.y1 <= ry0 + EPSILON || band.y0 >= ry1 - EPSILON {
                out.push(band);
                continue;
            }
            if band.y0 < ry0 - EPSILON {
                out.push(Band {
                    y0: band.y0,
                    y1: ry0,
                    xs: band.xs.clone(),
                });
            }
            let xs = subtract_interval(&band.xs, rx0, rx1);
            if !xs.is_empty() {
                out.push(Band {
                    y0: band.y0.max(ry0),
                    y1: band.y1.min(ry1),
                    xs,
                });
            }
            if band.y1 > ry1 + EPSILON {
                out.push(Band {
                    y0: ry1,
                    y1: band.y1,
                    xs: band.xs,
                });
            }
        }
        let mut merged: Vec<Band> = Vec::with_capacity(out.len());
        for band in out {
            if let Some(last) = merged.last_mut()
                && float_equal(last.y1, band.y0)
                && same_intervals(&last.xs, &band.xs)
            {
                last.y1 = band.y1;
                continue;
            }
            merged.push(band);
        }
        self.bands = merged;
        self.rectangle = false;
    }

    /// `sorted_horizontal_segments`: by y, and right to left at the same y.
    fn edges(&self) -> Vec<Edge> {
        let mut ys: Vec<f64> = self.bands.iter().flat_map(|b| [b.y0, b.y1]).collect();
        ys.sort_by(f64::total_cmp);
        ys.dedup_by(|a, b| float_equal(*a, *b));
        let mut edges = Vec::new();
        for y in ys {
            let above = self
                .bands
                .iter()
                .find(|b| float_equal(b.y0, y))
                .map(|b| b.xs.as_slice())
                .unwrap_or_default();
            let below = self
                .bands
                .iter()
                .find(|b| float_equal(b.y1, y))
                .map(|b| b.xs.as_slice())
                .unwrap_or_default();
            for (x0, x1) in subtract_intervals(above, below)
                .into_iter()
                .chain(subtract_intervals(below, above))
            {
                edges.push(Edge { y, x0, x1 });
            }
        }
        edges.sort_by(|a, b| {
            if float_equal(a.y, b.y) {
                b.x0.total_cmp(&a.x0)
            } else {
                a.y.total_cmp(&b.y)
            }
        });
        edges
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Region {
    MaxWidth,
    MaxHeight,
    TrimShape,
}

/// A region boxes are placed into (`HexaPDF::Layout::Frame`).
#[derive(Clone, Debug)]
pub struct Frame {
    left: f64,
    bottom: f64,
    width: f64,
    height: f64,
    shape: Shape,
    x: f64,
    y: f64,
    available_width: f64,
    available_height: f64,
    region: Region,
}

impl Frame {
    #[must_use]
    pub fn new(left: f64, bottom: f64, width: f64, height: f64) -> Self {
        Self {
            left,
            bottom,
            width,
            height,
            shape: Shape::rectangle(left, bottom, width, height),
            x: left,
            y: bottom + height,
            available_width: width,
            available_height: height,
            region: Region::MaxHeight,
        }
    }

    fn full(&self) -> bool {
        self.available_width == 0.0
    }

    /// `Frame#fit`: fits the box into the current region and places it.
    fn fit(&self, content: &Content) -> Fit {
        if self.full() {
            return Fit::failure();
        }
        let style = *content.style();
        let margin = style.margin;
        let mut aw = self.available_width;
        let mut ah = self.available_height;
        let (mut margin_top, mut margin_right, mut margin_bottom, mut margin_left) =
            (0.0, 0.0, 0.0, 0.0);
        if let Some(m) = margin {
            if !float_equal(self.x + aw, self.left + self.width) {
                margin_right = m.right;
                aw -= margin_right;
            }
            if !float_equal(self.x, self.left) {
                margin_left = m.left;
                aw -= margin_left;
            }
            if !float_equal(self.y - ah, self.bottom) {
                margin_bottom = m.bottom;
                ah -= margin_bottom;
            }
            if !float_equal(self.y, self.bottom + self.height) {
                margin_top = m.top;
                ah -= margin_top;
            }
        }
        let _ = margin_bottom;
        let mut fit = fit_box(content, aw, ah, self);
        if fit.status == FitStatus::Failure {
            return fit;
        }
        let x = match style.align {
            BoxAlign::Left => self.x + margin_left,
            BoxAlign::Right => self.x + margin_left + aw - fit.width,
            BoxAlign::Center => {
                let max_margin = margin_left.max(margin_right);
                if self.available_width - fit.width >= 2.0 * max_margin {
                    self.x + (self.available_width - fit.width) / 2.0
                } else {
                    self.x + margin_left + (aw - fit.width) / 2.0
                }
            }
        };
        let y = self.y - margin_top - fit.height;
        let m = margin.unwrap_or_default();
        fit.mask = match style.position {
            Position::Default => Mask::corners(
                self.left,
                self.bottom.max(y - m.bottom),
                self.left + self.width,
                self.y,
            ),
            Position::Float if margin.is_some() => Mask::corners(
                self.left.max(x - m.left),
                self.bottom.max(y - m.bottom),
                (self.left + self.width).min(x + fit.width + m.right),
                (self.bottom + self.height).min(y + fit.height + m.top),
            ),
            Position::Float => Mask::rectangle(x, y, fit.width, fit.height),
        };
        fit.x = x;
        fit.y = y;
        fit
    }

    /// `Frame#remove_area`.
    fn remove_area(&mut self, mask: &Mask) {
        if mask.is_rectangle && (mask.width == 0.0 || mask.height == 0.0) {
            return;
        }
        match self.shape.bounds() {
            Some((sx, sy, sw, sh))
                if self.shape.rectangle
                    && mask.is_rectangle
                    && float_equal(sx, mask.x)
                    && float_equal(sw, mask.width)
                    && float_equal(sy + sh, mask.y + mask.height) =>
            {
                if float_equal(sh, mask.height) {
                    self.shape = Shape {
                        bands: Vec::new(),
                        rectangle: false,
                    };
                } else {
                    self.shape = Shape::rectangle(sx, sy, sw, sh - mask.height);
                }
            }
            _ => self.shape.subtract(mask),
        }
        self.region = Region::MaxWidth;
        self.find_next_region();
    }

    /// `Frame#find_next_region`: false when the frame is full.
    fn find_next_region(&mut self) -> bool {
        match self.region {
            Region::MaxWidth => {
                if self.shape.rectangle
                    && let Some((sx, sy, sw, sh)) = self.shape.bounds()
                {
                    self.x = sx;
                    self.y = sy + sh;
                    self.available_width = sw;
                    self.available_height = sh;
                    self.region = Region::TrimShape;
                } else {
                    self.find_max_width_region();
                    self.region = Region::MaxHeight;
                }
            }
            Region::MaxHeight => {
                let before = (self.x, self.y, self.available_width, self.available_height);
                self.find_max_height_region();
                if before == (self.x, self.y, self.available_width, self.available_height) {
                    self.trim_shape();
                } else {
                    self.region = Region::TrimShape;
                }
            }
            Region::TrimShape => self.trim_shape(),
        }
        self.available_width != 0.0
    }

    fn find_starting_point(&mut self) -> Option<Vec<Edge>> {
        let mut edges = self.shape.edges();
        let Some(top) = edges.pop() else {
            self.x = 0.0;
            self.y = 0.0;
            self.available_width = 0.0;
            self.available_height = 0.0;
            return None;
        };
        self.x = top.x0;
        self.y = top.y;
        self.available_width = top.length();
        Some(edges)
    }

    fn find_max_width_region(&mut self) {
        let Some(mut edges) = self.find_starting_point() else {
            return;
        };
        let x_right = self.x + self.available_width;
        edges.retain(|e| e.x1 > self.x + EPSILON && e.x0 < x_right - EPSILON);
        if let Some(last) = edges.last() {
            self.available_height = self.y - last.y;
        }
    }

    fn find_max_height_region(&mut self) {
        let Some(mut edges) = self.find_starting_point() else {
            return;
        };
        let x = self.x;
        let Some(index) = edges
            .iter()
            .rposition(|e| e.x0 <= x + EPSILON && x < e.x1 - EPSILON)
        else {
            return;
        };
        let y1 = edges[index].y;
        self.available_height = self.y - y1;
        let top = self.y;
        edges.retain(|e| e.x0 > x + EPSILON && y1 <= e.y + EPSILON && e.y <= top + EPSILON);
        if let Some(edge) = edges.iter().min_by(|a, b| a.x0.total_cmp(&b.x0)) {
            self.available_width = edge.x0 - x;
        }
    }

    fn trim_shape(&mut self) {
        self.x = 0.0;
        self.y = 0.0;
        self.available_width = 0.0;
        self.available_height = 0.0;
        if self.shape.rectangle {
            return;
        }
        let Some(edges) = self.find_starting_point() else {
            return;
        };
        let top = self.y;
        let Some(edge) = edges.iter().rev().find(|e| e.y < top - EPSILON).copied() else {
            return;
        };
        let mask = if float_equal(edge.x0, self.x) {
            Mask {
                x: self.x,
                y: edge.y,
                width: edge.length(),
                height: top - edge.y,
                is_rectangle: false,
            }
        } else {
            Mask {
                x: self.left,
                y: edge.y,
                width: self.width,
                height: top - edge.y,
                is_rectangle: false,
            }
        };
        self.remove_area(&mask);
    }
}

/// `Box#fit` followed by the box's `fit_content`.
fn fit_box(content: &Content, available_width: f64, available_height: f64, frame: &Frame) -> Fit {
    let style = *content.style();
    let (initial_width, initial_height) = content.initial_size();
    let mut width = if initial_width > 0.0 {
        initial_width
    } else {
        available_width
    };
    let mut height = if initial_height > 0.0 {
        initial_height
    } else {
        available_height
    };
    let (rw, rh) = (style.reserved_width(), style.reserved_height());
    if exceeds(width, available_width)
        || exceeds(height, available_height)
        || width - rw < 0.0
        || height - rh < 0.0
    {
        return Fit::failure();
    }
    let mut fit = Fit::failure();
    match content {
        Content::Text(text) => {
            let fitted = text.layouter.fit_segments(
                text.items.clone(),
                width - rw,
                height - rh,
                &text.empty,
            );
            if text.layouter.align == super::layout::Align::Left && initial_width == 0.0 {
                width = fitted
                    .lines
                    .iter()
                    .map(super::layout::Line::width)
                    .fold(0.0, f64::max)
                    + rw;
            }
            if initial_height == 0.0 {
                height = fitted.height() + rh;
            }
            fit.status = match fitted.status {
                Status::Success => FitStatus::Success,
                Status::Height if !fitted.lines.is_empty() => FitStatus::Overflow,
                _ => FitStatus::Failure,
            };
            if fit.status == FitStatus::Overflow {
                let mut rest = text.clone();
                rest.items.clone_from(&fitted.remaining);
                fit.rest = Some(Content::Text(rest));
            }
            if !fitted.lines.is_empty() {
                fit.paints.push(Paint::Text {
                    fitted,
                    x: style.padding.left,
                    top: height - style.padding.top,
                });
            }
        }
        Content::Image(image) => {
            if !exceeds(width, available_width) && !exceeds(height, available_height) {
                fit.status = FitStatus::Success;
            }
            fit.paints.push(Paint::Image {
                image: image.image,
                x: style.padding.left,
                y: style.padding.bottom,
                width: width - rw,
                height: height - rh,
            });
        }
        Content::Rule(rule) => {
            fit.status = FitStatus::Success;
            fit.paints.push(Paint::TopBorder {
                x: 0.0,
                y: 0.0,
                width,
                height,
                border_width: rule.border_width,
                color: rule.color,
            });
        }
        Content::Column(column) => {
            let column_width = width - rw;
            if column_width <= 0.0 {
                return Fit::failure();
            }
            let left = frame.x + style.padding.left;
            let top = frame.y - style.padding.top;
            let column_height = height - rh;
            let mut fitter = BoxFitter::new(Frame::new(
                left,
                top - column_height,
                column_width,
                column_height,
            ));
            for child in &column.children {
                fitter.fit(child.clone());
            }
            height = fitter.content_height + rh;
            fit.status = if fitter.remaining.is_empty() {
                FitStatus::Success
            } else if !fitter.paints.is_empty() {
                FitStatus::Overflow
            } else {
                FitStatus::Failure
            };
            if fit.status == FitStatus::Overflow {
                let mut rest = column.clone();
                rest.children = fitter.remaining;
                fit.rest = Some(Content::Column(rest));
            }
            // Children were placed in frame coordinates; the box is expected at
            // `(frame.x, frame.y - height)`.
            let (ox, oy) = (frame.x, frame.y - height);
            fit.paints = fitter.paints;
            for paint in &mut fit.paints {
                paint.translate(-ox, -oy);
            }
        }
        Content::Table(table) => {
            fit_table(table, width - rw, height - rh, &mut fit);
            if fit.status != FitStatus::Failure {
                height = fit.height + rh;
                for paint in &mut fit.paints {
                    paint.translate(style.padding.left, style.padding.bottom);
                }
            }
        }
    }
    fit.width = width;
    fit.height = height;
    fit
}

/// `TableBox#fit_content` with `Cells#fit_rows`; sets `fit.height` to the rows' height.
fn fit_table(table: &TableBox, width: f64, height: f64, fit: &mut Fit) {
    let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 || width <= 0.0 {
        return;
    }
    #[allow(clippy::cast_precision_loss, reason = "column counts")]
    let column_width = width / columns as f64;
    let padding = table.cell_padding;
    let (prw, prh) = (padding.left + padding.right, padding.top + padding.bottom);
    let mut available = height;
    // Per fitted row: its height and each cell's column, child paints (relative to the
    // child frame's origin), and the child frame's height (`@used_height`).
    let mut fitted_rows: Vec<(f64, Vec<FittedCell>)> = Vec::new();
    for row in &table.rows[table.start..] {
        let mut row_height: f64 = 0.0;
        let mut cells = Vec::new();
        let mut row_fit = true;
        for (index, cell) in row.iter().enumerate() {
            // `Cell#fit`: `Box#fit` checks, then the child in a frame of the content area.
            let (cw, ch) = (column_width - prw, available - prh);
            if cw <= 0.0 || ch <= 0.0 {
                row_fit = false;
                break;
            }
            let (paints, preferred_height) = match cell {
                Some(child) => {
                    let child_fit = Frame::new(0.0, 0.0, cw, ch).fit(child);
                    if child_fit.status != FitStatus::Success {
                        row_fit = false;
                        break;
                    }
                    let height = child_fit.height;
                    (child_fit.placed(), height + prh)
                }
                None => (Vec::new(), prh),
            };
            row_height = row_height.max(preferred_height);
            cells.push((index, paints, ch));
        }
        if !row_fit {
            break;
        }
        available -= row_height;
        fitted_rows.push((row_height, cells));
    }
    let last = table.start + fitted_rows.len();
    fit.status = if last == table.rows.len() {
        FitStatus::Success
    } else if !fitted_rows.is_empty() {
        FitStatus::Overflow
    } else {
        FitStatus::Failure
    };
    if fit.status == FitStatus::Failure {
        return;
    }
    if fit.status == FitStatus::Overflow {
        let mut rest = table.clone();
        rest.start = last;
        fit.rest = Some(Content::Table(rest));
    }
    let used = height - available;
    fit.height = used;
    // `draw_rows` and `Cell#draw_content`: a cell's child keeps its place relative to the
    // top of the cell's content area.
    let mut top = 0.0;
    for (row_height, cells) in fitted_rows {
        for (index, paints, used_height) in cells {
            #[allow(clippy::cast_precision_loss, reason = "column counts")]
            let cell_x = index as f64 * column_width;
            let cell_y = used - top - row_height;
            let content_height = row_height - prh;
            let dx = cell_x + padding.left;
            let dy = cell_y + padding.bottom - (used_height - content_height);
            for mut paint in paints {
                paint.translate(dx, dy);
                fit.paints.push(paint);
            }
        }
        top += row_height;
    }
}

/// `BoxFitter` over one frame.
struct BoxFitter {
    frame: Frame,
    initial_y: f64,
    content_height: f64,
    paints: Vec<Paint>,
    remaining: Vec<Content>,
}

impl BoxFitter {
    fn new(frame: Frame) -> Self {
        Self {
            initial_y: frame.y,
            frame,
            content_height: 0.0,
            paints: Vec::new(),
            remaining: Vec::new(),
        }
    }

    fn fit(&mut self, content: Content) {
        if !self.remaining.is_empty() {
            self.remaining.push(content);
            return;
        }
        let mut content = Some(content);
        while let Some(current) = content.take() {
            if self.frame.full() {
                self.remaining.push(current);
                return;
            }
            let mut fit = self.frame.fit(&current);
            match fit.status {
                FitStatus::Success => {
                    self.place(fit);
                }
                FitStatus::Overflow => {
                    let rest = fit.rest.take();
                    self.place(fit);
                    content = rest;
                }
                FitStatus::Failure => {
                    if !self.frame.find_next_region() {
                        self.remaining.push(current);
                        return;
                    }
                    content = Some(current);
                }
            }
        }
    }

    fn place(&mut self, fit: Fit) {
        self.frame.remove_area(&fit.mask);
        self.content_height = self.content_height.max(self.initial_y - fit.mask.y);
        self.paints.extend(fit.placed());
    }
}

/// A table cell fitted into its row: its column, the child's paints and the child frame's
/// height.
type FittedCell = (usize, Vec<Paint>, f64);

/// Draws a page's fixed content (`page_style`'s template) when the page is created.
type PageTemplate<'a> = Box<dyn FnMut(&mut Editor, ObjectId) -> Result<(), EditError> + 'a>;

/// `Composer`: lays boxes out page after page.
pub struct Composer<'a> {
    pub editor: &'a mut Editor,
    page_size: (f64, f64),
    margin: f64,
    template: PageTemplate<'a>,
    page: Option<ObjectId>,
    frame: Frame,
}

impl<'a> Composer<'a> {
    /// Pages of `page_size` with a frame `margin` from each edge; `template` draws each
    /// new page's decorations (`page_style`).
    pub fn new(
        editor: &'a mut Editor,
        page_size: (f64, f64),
        margin: f64,
        template: impl FnMut(&mut Editor, ObjectId) -> Result<(), EditError> + 'a,
    ) -> Self {
        Self {
            editor,
            page_size,
            margin,
            template: Box::new(template),
            page: None,
            frame: Frame::new(0.0, 0.0, 0.0, 0.0),
        }
    }

    /// `Composer#new_page`.
    pub fn new_page(&mut self) -> Result<ObjectId, EditError> {
        let (width, height) = self.page_size;
        let page = self.editor.add_page(width, height)?;
        (self.template)(self.editor, page)?;
        self.page = Some(page);
        self.frame = Frame::new(
            self.margin,
            self.margin,
            width - 2.0 * self.margin,
            height - 2.0 * self.margin,
        );
        Ok(page)
    }

    /// `Composer#draw_box`.
    pub fn draw_box(&mut self, content: Content) -> Result<(), EditError> {
        if self.page.is_none() {
            self.new_page()?;
        }
        let mut drawn_on_page = true;
        let mut content = content;
        loop {
            let mut fit = self.frame.fit(&content);
            if fit.status == FitStatus::Success {
                self.place(fit)?;
                return Ok(());
            }
            if self.frame.full() {
                self.new_page()?;
                drawn_on_page = false;
                continue;
            }
            if fit.status == FitStatus::Overflow {
                let rest = fit.rest.take();
                self.place(fit)?;
                drawn_on_page = true;
                match rest {
                    Some(rest) => content = rest,
                    None => return Ok(()),
                }
            } else if !self.frame.find_next_region() {
                if !drawn_on_page {
                    return Err(EditError::Invalid(
                        "Box didn't fit multiple times, even on empty page".to_owned(),
                    ));
                }
                self.new_page()?;
                drawn_on_page = false;
            }
        }
    }

    fn place(&mut self, fit: Fit) -> Result<(), EditError> {
        let page = self
            .page
            .ok_or_else(|| EditError::Invalid("no page".to_owned()))?;
        self.frame.remove_area(&fit.mask);
        for paint in fit.placed() {
            draw_paint(self.editor, page, paint);
        }
        Ok(())
    }
}

fn draw_paint(editor: &mut Editor, page: ObjectId, paint: Paint) {
    match paint {
        Paint::Text { fitted, x, top } => editor.draw_text(page, &fitted, x, top),
        Paint::Image {
            image,
            x,
            y,
            width,
            height,
        } => {
            let name = editor.use_image(page, image);
            editor.overlay(page).image(&name, x, y, width, height);
        }
        Paint::TopBorder {
            x,
            y,
            width,
            height,
            border_width,
            color,
        } => {
            let (left, right, top) = (x, x + width, y + height);
            let inner_top = top - border_width;
            let color = format!("{} {} {}", real(color[0]), real(color[1]), real(color[2]));
            editor.overlay(page).raw(&format!(
                "q q {l} {t} m {r} {t} l {r} {it} l {l} {it} l h W n {color} RG {w} w 0 J [] 0 d \
                 {l} {mid} m {r} {mid} l S Q Q",
                l = real(left),
                r = real(right),
                t = real(top),
                it = real(inner_top),
                w = real(border_width),
                mid = real(top - border_width / 2.0),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_side_by_side_then_fill_below() {
        let mut frame = Frame::new(0.0, 0.0, 500.0, 300.0);
        // A 40x40 float at the top left.
        frame.remove_area(&Mask::rectangle(0.0, 260.0, 40.0, 40.0));
        assert!((frame.x - 40.0).abs() < 1e-9);
        assert!((frame.y - 300.0).abs() < 1e-9);
        assert!((frame.available_width - 460.0).abs() < 1e-9);
        assert!((frame.available_height - 300.0).abs() < 1e-9);
        // A 100 wide, 30 high float next to it.
        frame.remove_area(&Mask::rectangle(40.0, 270.0, 100.0, 30.0));
        assert!((frame.x - 140.0).abs() < 1e-9);
        assert!((frame.available_width - 360.0).abs() < 1e-9);
        // The rest of the top row.
        frame.remove_area(&Mask::rectangle(140.0, 280.0, 360.0, 20.0));
        assert!((frame.x - 140.0).abs() < 1e-9);
        assert!((frame.y - 280.0).abs() < 1e-9);
    }

    #[test]
    fn shrinks_a_rectangle_from_the_top() {
        let mut frame = Frame::new(50.0, 50.0, 400.0, 700.0);
        frame.remove_area(&Mask::rectangle(50.0, 700.0, 400.0, 50.0));
        assert!(frame.shape.rectangle);
        assert!((frame.y - 700.0).abs() < 1e-9);
        assert!((frame.available_height - 650.0).abs() < 1e-9);
        // Another fit result's mask leaves it a rectangle; the next region is trimmed.
        assert!(!frame.find_next_region());
        assert!(frame.full());
    }
}
