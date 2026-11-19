//! `Templates::ImageToFields.call` for the v2 model (`call_v2`): trim, letterbox, run the
//! model, then `postprocess_outputs_v2`, `nms`, `nmm` and `build_fields_from_detections`.
//!
//! Rails keeps the detections in `Numo::SFloat` arrays: arithmetic with Ruby numbers
//! casts them to single precision first, and `sort_index` is Numo's quicksort, whose
//! order of equal scores decides which boxes survive. Both are reproduced here.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    reason = "Ruby and Numo conversions and comparisons"
)]

use crate::DetectError;
use crate::image::{self, Rgb};
use crate::model::{Model, Outputs};

/// `ID_TO_CLASS`.
const ID_TO_CLASS: [&str; 2] = ["text", "checkbox"];

/// `Templates::ImageToFields::Field`, in fractions of the image.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub kind: Option<&'static str>,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub confidence: f64,
}

impl Field {
    #[must_use]
    pub fn endx(&self) -> f64 {
        self.x + self.w
    }

    #[must_use]
    pub fn endy(&self) -> f64 {
        self.y + self.h
    }
}

/// The options `ImageToFields.call` gets (`split_page: false`, `aspect_ratio: true`; the
/// v2 model ignores `temperature`).
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub confidence: f64,
    pub nms: f64,
    pub nmm: f64,
    pub padding: Option<i64>,
}

/// Detections as Numo arrays: `xyxy`, `confidence`, `class_id`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Detections {
    pub xyxy: Vec<[f32; 4]>,
    pub confidence: Vec<f32>,
    pub class_id: Vec<i64>,
}

impl Detections {
    fn select(&self, keep: &[usize]) -> Self {
        Self {
            xyxy: keep.iter().map(|&i| self.xyxy[i]).collect(),
            confidence: keep.iter().map(|&i| self.confidence[i]).collect(),
            class_id: keep.iter().map(|&i| self.class_id[i]).collect(),
        }
    }
}

/// `ImageToFields.call(image, …)` with a v2 model.
pub fn call(model: &Model, image: &Rgb, options: Options) -> Result<Vec<Field>, DetectError> {
    let (trimmed, offset_x, offset_y) = trim_image_with_padding(image, options.padding);
    let (planar, transform) = preprocess_image_v2(&trimmed, model.resolution);
    let outputs = model.predict(planar)?;
    let detections =
        postprocess_outputs_v2(&outputs, offset_x, offset_y, options.confidence, transform);
    let detections = apply_nms_nmm(&detections, options.nms, options.nmm, 0.3);
    Ok(build_fields_from_detections(&detections, image))
}

/// `trim_image_with_padding`: the content area with `padding` pixels around it, and its
/// offset in the image.
#[must_use]
pub fn trim_image_with_padding(image: &Rgb, padding: Option<i64>) -> (Rgb, i64, i64) {
    let Some(padding) = padding else {
        return (image.clone(), 0, 0);
    };
    let [left, top, trim_width, trim_height] = image::find_trim(image, 10.0);
    let (w, h) = (image.width as i64, image.height as i64);
    // `[trim_width, image.width - (left * 1.9)].max` can be a Float.
    let trim_width = (trim_width as f64).max(w as f64 - left as f64 * 1.9);
    let padded_left = (left - padding).max(0);
    let padded_top = (top - padding).max(0);
    let padded_right = (left as f64 + trim_width + padding as f64).min(w as f64);
    let padded_bottom = (top + trim_height + padding).min(h);
    // `crop` truncates Float sizes.
    let width = (padded_right - padded_left as f64) as i64;
    let height = padded_bottom - padded_top;
    (
        image.crop(
            padded_left as usize,
            padded_top as usize,
            width.max(0) as usize,
            height.max(0) as usize,
        ),
        padded_left,
        padded_top,
    )
}

/// `transform_info` of `preprocess_image_v2`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub ratio: f64,
    pub pad_w: i64,
    pub pad_h: i64,
}

/// `preprocess_image_v2`: scale to fit `resolution` (linear kernel), center on white,
/// divide by 255. Returns the `[3, r, r]` input and the transform.
#[must_use]
pub fn preprocess_image_v2(image: &Rgb, resolution: usize) -> (Vec<f32>, Transform) {
    let r = resolution as f64;
    let ratio = (r / image.width as f64).min(r / image.height as f64);
    let new_width = (image.width as f64 * ratio) as i64;
    let new_height = (image.height as f64 * ratio) as i64;
    let pad_w = (resolution as i64 - new_width).div_euclid(2);
    let pad_h = (resolution as i64 - new_height).div_euclid(2);
    let resized = if ratio == 1.0 {
        image.clone()
    } else {
        let regions = image::embedded_regions(
            image,
            ratio,
            resolution,
            pad_w.max(0) as usize,
            pad_h.max(0) as usize,
        );
        image::resize(image, ratio, regions)
    };
    let padded = resized.embed_white(
        pad_w.max(0) as usize,
        pad_h.max(0) as usize,
        resolution,
        resolution,
    );
    (
        image::to_planar_f32(&padded),
        Transform {
            ratio,
            pad_w,
            pad_h,
        },
    )
}

/// `postprocess_outputs_v2`: scores above `confidence`, boxes mapped back to the image.
#[must_use]
pub fn postprocess_outputs_v2(
    outputs: &Outputs,
    offset_x: i64,
    offset_y: i64,
    confidence: f64,
    transform: Transform,
) -> Detections {
    let threshold = confidence as f32;
    let (ratio, pad_w, pad_h) = (
        transform.ratio as f32,
        transform.pad_w as f32,
        transform.pad_h as f32,
    );
    let (ox, oy) = (offset_x as f32, offset_y as f32);
    let mut detections = Detections::default();
    for (i, &score) in outputs.scores.iter().enumerate() {
        if score > threshold {
            let b = outputs.boxes[i];
            detections.xyxy.push([
                ((b[0] - pad_w) / ratio) + ox,
                ((b[1] - pad_h) / ratio) + oy,
                ((b[2] - pad_w) / ratio) + ox,
                ((b[3] - pad_h) / ratio) + oy,
            ]);
            detections.confidence.push(score);
            detections.class_id.push(outputs.labels[i]);
        }
    }
    detections
}

/// `apply_nms_nmm`.
#[must_use]
pub fn apply_nms_nmm(detections: &Detections, nms: f64, nmm: f64, confidence: f64) -> Detections {
    if detections.xyxy.is_empty() {
        return detections.clone();
    }
    let kept = non_max_suppression(detections, nms);
    non_max_merge(&kept, nmm, confidence)
}

/// `Numo::SFloat.maximum`.
fn maximum(x: f32, y: f32) -> f32 {
    if x >= y || y.is_nan() { x } else { y }
}

/// `Numo::SFloat.minimum`.
fn minimum(x: f32, y: f32) -> f32 {
    if x <= y || y.is_nan() { x } else { y }
}

/// The intersections of box `i` with the boxes `rest`.
fn intersections(boxes: &[[f32; 4]], i: usize, rest: &[usize]) -> Vec<f32> {
    let b = boxes[i];
    rest.iter()
        .map(|&j| {
            let o = boxes[j];
            let (xx1, yy1) = (maximum(b[0], o[0]), maximum(b[1], o[1]));
            let (xx2, yy2) = (minimum(b[2], o[2]), minimum(b[3], o[3]));
            maximum(0.0, xx2 - xx1) * maximum(0.0, yy2 - yy1)
        })
        .collect()
}

fn areas(boxes: &[[f32; 4]]) -> Vec<f32> {
    boxes
        .iter()
        .map(|b| (b[2] - b[0]) * (b[3] - b[1]))
        .collect()
}

/// `nms(detections, iou_threshold)`.
#[must_use]
pub fn non_max_suppression(detections: &Detections, iou_threshold: f64) -> Detections {
    let threshold = iou_threshold as f32;
    let areas = areas(&detections.xyxy);
    let mut order = sort_index(&detections.confidence);
    order.reverse();
    let mut keep = Vec::new();
    while let Some(&i) = order.first() {
        keep.push(i);
        if order.len() == 1 {
            break;
        }
        let rest = &order[1..];
        let inter = intersections(&detections.xyxy, i, rest);
        order = rest
            .iter()
            .zip(inter)
            .filter(|&(&j, inter)| inter / ((areas[i] + areas[j]) - inter) <= threshold)
            .map(|(&j, _)| j)
            .collect();
    }
    detections.select(&keep)
}

/// `nmm(detections, overlap_threshold, confidence:)`: merges the boxes of the same class
/// that a larger confident box mostly covers into it. Like Rails, the merged box and
/// score replace the larger box's.
#[must_use]
pub fn non_max_merge(
    detections: &Detections,
    overlap_threshold: f64,
    confidence: f64,
) -> Detections {
    let threshold = overlap_threshold as f32;
    let mut boxes = detections.xyxy.clone();
    let mut scores = detections.confidence.clone();
    let classes = &detections.class_id;
    let areas = areas(&boxes);
    let mut order = sort_index(&areas);
    order.reverse();
    let mut keep = Vec::new();
    while let Some(&i) = order.first() {
        keep.push(i);
        if order.len() == 1 {
            break;
        }
        let rest = order[1..].to_vec();
        if f64::from(scores[i]) > confidence {
            let inter = intersections(&boxes, i, &rest);
            let merge: Vec<bool> = rest
                .iter()
                .zip(inter)
                .map(|(&j, inter)| inter / areas[j] > threshold && classes[j] == classes[i])
                .collect();
            let candidates: Vec<usize> = rest
                .iter()
                .zip(&merge)
                .filter(|&(_, &m)| m)
                .map(|(&j, _)| j)
                .collect();
            if !candidates.is_empty() {
                let best = numo_max(candidates.iter().map(|&j| scores[j]));
                scores[i] = ruby_max(f64::from(scores[i]), f64::from(best)) as f32;
                let fold = |current: f32, values: &mut dyn Iterator<Item = f32>, max: bool| {
                    let extreme = if max {
                        numo_max(values)
                    } else {
                        numo_min(values)
                    };
                    let (a, b) = (f64::from(current), f64::from(extreme));
                    (if max { ruby_max(a, b) } else { ruby_min(a, b) }) as f32
                };
                let mut b = boxes[i];
                b[0] = fold(b[0], &mut candidates.iter().map(|&j| boxes[j][0]), false);
                b[1] = fold(b[1], &mut candidates.iter().map(|&j| boxes[j][1]), false);
                b[2] = fold(b[2], &mut candidates.iter().map(|&j| boxes[j][2]), true);
                b[3] = fold(b[3], &mut candidates.iter().map(|&j| boxes[j][3]), true);
                boxes[i] = b;
            }
            order = rest
                .iter()
                .zip(&merge)
                .filter(|&(_, &m)| !m)
                .map(|(&j, _)| j)
                .collect();
        } else {
            order = rest;
        }
    }
    Detections {
        xyxy: keep.iter().map(|&i| boxes[i]).collect(),
        confidence: keep.iter().map(|&i| scores[i]).collect(),
        class_id: keep.iter().map(|&i| classes[i]).collect(),
    }
}

/// `SFloat#max`: the largest value, skipping NaN.
fn numo_max(values: impl Iterator<Item = f32>) -> f32 {
    values
        .filter(|v| !v.is_nan())
        .reduce(|y, x| if x > y { x } else { y })
        .unwrap_or(f32::NAN)
}

/// `SFloat#min`.
fn numo_min(values: impl Iterator<Item = f32>) -> f32 {
    values
        .filter(|v| !v.is_nan())
        .reduce(|y, x| if x < y { x } else { y })
        .unwrap_or(f32::NAN)
}

/// `[a, b].max` on Ruby floats.
fn ruby_max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// `[a, b].min`.
fn ruby_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

/// `build_fields_from_detections`: boxes as fractions of the image, dropping the ones
/// that start outside it.
#[must_use]
pub fn build_fields_from_detections(detections: &Detections, image: &Rgb) -> Vec<Field> {
    let (w, h) = (image.width as f64, image.height as f64);
    let mut fields = Vec::new();
    for (i, b) in detections.xyxy.iter().enumerate() {
        let x0 = f64::from(b[0]) / w;
        let y0 = f64::from(b[1]) / h;
        let mut x1 = f64::from(b[2]) / w;
        let mut y1 = f64::from(b[3]) / h;
        if x1 > 1.0 {
            x1 = 1.0;
        }
        if y1 > 1.0 {
            y1 = 1.0;
        }
        if x0 < 0.0 || x0 > 1.0 || y0 < 0.0 || y0 > 1.0 {
            continue;
        }
        // `ID_TO_CLASS[class_id]`, negative indexes counting from the end.
        let class = detections.class_id[i];
        let kind = if class < 0 {
            usize::try_from(class + ID_TO_CLASS.len() as i64)
                .ok()
                .and_then(|i| ID_TO_CLASS.get(i))
        } else {
            usize::try_from(class).ok().and_then(|i| ID_TO_CLASS.get(i))
        };
        fields.push(Field {
            kind: kind.copied(),
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
            confidence: f64::from(detections.confidence[i]),
        });
    }
    fields
}

/// `SFloat#sort_index` (`nan: false`): Numo's Bentley-McIlroy quicksort of the indexes,
/// NaN sorted last.
#[must_use]
pub fn sort_index(values: &[f32]) -> Vec<usize> {
    let mut index: Vec<usize> = (0..values.len()).collect();
    if index.len() > 1 {
        qsort(&mut index, values);
    }
    index
}

/// `cmp_ignan`.
fn cmp(values: &[f32], a: usize, b: usize) -> i32 {
    let (x, y) = (values[a], values[b]);
    match (x.is_nan(), y.is_nan()) {
        (true, true) => 0,
        (true, false) => 1,
        (false, true) => -1,
        _ if x == y => 0,
        _ if x > y => 1,
        _ => -1,
    }
}

/// `cmpgt_ignan`.
fn gt(values: &[f32], a: usize, b: usize) -> bool {
    let (x, y) = (values[a], values[b]);
    (x.is_nan() && !y.is_nan()) || x > y
}

/// `DEF_TYPED_QSORT_FUNC` on a slice of indexes (`es` is one element).
fn qsort(items: &mut [usize], values: &[f32]) {
    let mut a = items;
    loop {
        let n = a.len();
        let gt = |a: &[usize], i: usize, j: usize| gt(values, a[i], a[j]);
        if n < 7 {
            for pm in 1..n {
                let mut pl = pm;
                while pl > 0 && gt(a, pl - 1, pl) {
                    a.swap(pl, pl - 1);
                    pl -= 1;
                }
            }
            return;
        }
        if (1..n).all(|pm| !gt(a, pm - 1, pm)) {
            return;
        }
        let med3 = |a: &[usize], x: usize, y: usize, z: usize| {
            if gt(a, y, x) {
                if gt(a, z, y) {
                    y
                } else if gt(a, z, x) {
                    z
                } else {
                    x
                }
            } else if gt(a, y, z) {
                y
            } else if gt(a, z, x) {
                x
            } else {
                z
            }
        };
        let mut pm = n / 2;
        if n > 7 {
            let mut pl = 0;
            let mut pn = n - 1;
            if n > 40 {
                let d = n / 8;
                pl = med3(a, pl, pl + d, pl + 2 * d);
                pm = med3(a, pm - d, pm, pm + d);
                pn = med3(a, pn - 2 * d, pn - d, pn);
            }
            pm = med3(a, pl, pm, pn);
        }
        a.swap(0, pm);
        let (mut pa, mut pb) = (1isize, 1isize);
        let (mut pc, mut pd) = (n as isize - 1, n as isize - 1);
        while pb <= pc {
            while pb <= pc {
                let r = cmp(values, a[pb as usize], a[0]);
                if r > 0 {
                    break;
                }
                if r == 0 {
                    a.swap(pa as usize, pb as usize);
                    pa += 1;
                }
                pb += 1;
            }
            while pb <= pc {
                let r = cmp(values, a[pc as usize], a[0]);
                if r < 0 {
                    break;
                }
                if r == 0 {
                    a.swap(pc as usize, pd as usize);
                    pd -= 1;
                }
                pc -= 1;
            }
            if pb > pc {
                break;
            }
            a.swap(pb as usize, pc as usize);
            pb += 1;
            pc -= 1;
        }
        let pn = n as isize;
        let r = pa.min(pb - pa);
        vecswap(a, 0, (pb - r) as usize, r as usize);
        let r = (pd - pc).min(pn - pd - 1);
        vecswap(a, pb as usize, (pn - r) as usize, r as usize);
        let left = (pb - pa) as usize;
        if left > 1 {
            qsort(&mut a[..left], values);
        }
        let right = (pd - pc) as usize;
        if right > 1 {
            let start = n - right;
            a = &mut a[start..];
        } else {
            return;
        }
    }
}

/// `vecswap`: swaps `n` elements at `i` and `j`, one after the other.
fn vecswap(a: &mut [usize], i: usize, j: usize, n: usize) {
    for k in 0..n {
        a.swap(i + k, j + k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_like_numo() {
        // Checked against `Numo::SFloat#sort_index` in the Rails image: ties keep
        // the quicksort's order.
        let values = [0.5, 0.25, 0.5, 0.75, 0.25, 0.5, 0.5, 0.1, 0.5, 0.25];
        assert_eq!(sort_index(&values), vec![7, 1, 9, 4, 8, 5, 2, 0, 6, 3]);
        let values: Vec<f32> = (0..50).map(|i| ((i * 37) % 11) as f32 / 4.0).collect();
        assert_eq!(
            sort_index(&values),
            vec![
                22, 44, 33, 0, 11, 36, 14, 25, 47, 3, 6, 17, 28, 39, 31, 9, 20, 42, 23, 12, 1, 45,
                34, 15, 48, 26, 37, 4, 29, 7, 40, 18, 10, 32, 43, 21, 24, 35, 46, 2, 13, 5, 16, 38,
                49, 27, 19, 41, 30, 8
            ]
        );
        let values: Vec<f32> = (0..60).map(|i| ((i * 13) % 7) as f32 / 2.0).collect();
        assert_eq!(
            sort_index(&values),
            vec![
                0, 21, 42, 7, 35, 56, 28, 14, 49, 48, 34, 20, 41, 6, 13, 27, 55, 33, 40, 54, 12,
                19, 47, 26, 5, 25, 32, 53, 11, 46, 18, 39, 4, 59, 3, 10, 17, 24, 31, 38, 45, 52,
                30, 44, 58, 23, 16, 51, 9, 2, 37, 36, 43, 1, 15, 29, 57, 22, 8, 50
            ]
        );
        assert_eq!(sort_index(&[f32::NAN, 1.0, 0.5]), vec![2, 1, 0]);
    }

    #[test]
    fn suppresses_and_merges() {
        let detections = Detections {
            xyxy: vec![
                [0.0, 0.0, 10.0, 10.0],
                [1.0, 1.0, 10.0, 10.0],
                [20.0, 20.0, 30.0, 30.0],
                [21.0, 21.0, 22.0, 22.0],
            ],
            confidence: vec![0.9, 0.8, 0.7, 0.6],
            class_id: vec![0, 0, 0, 0],
        };
        let kept = non_max_suppression(&detections, 0.1);
        assert_eq!(kept.confidence, vec![0.9, 0.7, 0.6]);
        // Largest first (equal areas in the quicksort's order); the small box inside
        // the second one merges into it.
        let merged = non_max_merge(&kept, 0.5, 0.3);
        assert_eq!(merged.confidence, vec![0.7, 0.9]);
        assert_eq!(
            merged.xyxy,
            vec![[20.0, 20.0, 30.0, 30.0], [0.0, 0.0, 10.0, 10.0]]
        );
    }
}
