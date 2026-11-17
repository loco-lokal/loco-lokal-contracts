//! `TextUtils.maybe_rtl_reverse`: text with Hebrew or Arabic letters is put in visual
//! order for drawing. Arabic letters are first replaced by their positional presentation
//! forms (`ArabicLetterConnector.transform`), then the text is reordered with `TwitterCldr`'s
//! `Bidi` (`from_string(text, direction: :RTL).reorder_visually!`).
//!
//! `Bidi` is `TwitterCldr`'s own port of an early Unicode bidi algorithm, not the current
//! one (no isolates, its own weak type rules), so it is ported as it is, quirks included.
//! The bidi classes are `TwitterCldr`'s (`rust/data/bidi_classes.txt`, dumped by
//! `rust/scripts/dump-bidi-classes.sh`).

use std::{collections::HashMap, sync::OnceLock};

const DATA: &str = include_str!("../../../data/bidi_classes.txt");

/// `Bidi::MAX_DEPTH`.
const MAX_DEPTH: i64 = 62;

/// A Unicode bidi class (`CodePoint#bidi_class`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    L,
    R,
    AL,
    EN,
    ES,
    ET,
    AN,
    CS,
    NSM,
    BN,
    B,
    S,
    WS,
    ON,
    LRE,
    LRO,
    RLE,
    RLO,
    PDF,
    LRI,
    RLI,
    FSI,
    PDI,
}

impl Class {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "L" => Self::L,
            "R" => Self::R,
            "AL" => Self::AL,
            "EN" => Self::EN,
            "ES" => Self::ES,
            "ET" => Self::ET,
            "AN" => Self::AN,
            "CS" => Self::CS,
            "NSM" => Self::NSM,
            "BN" => Self::BN,
            "B" => Self::B,
            "S" => Self::S,
            "WS" => Self::WS,
            "ON" => Self::ON,
            "LRE" => Self::LRE,
            "LRO" => Self::LRO,
            "RLE" => Self::RLE,
            "RLO" => Self::RLO,
            "PDF" => Self::PDF,
            "LRI" => Self::LRI,
            "RLI" => Self::RLI,
            "FSI" => Self::FSI,
            "PDI" => Self::PDI,
            _ => return None,
        })
    }
}

/// A character `TwitterCldr` has no data for (`CodePoint.get` is nil), on which
/// `maybe_rtl_reverse` raises in Rails.
#[derive(Debug, thiserror::Error)]
#[error("no bidi class for U+{0:04X}")]
pub struct UnknownCodePoint(pub u32);

struct Tables {
    classes: Vec<(u32, u32, Class)>,
    rtl: Vec<(u32, u32)>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut classes = Vec::new();
        let mut rtl = Vec::new();
        for line in DATA.lines().filter(|l| !l.starts_with('#')) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let hex = |s: &str| u32::from_str_radix(s, 16).ok();
            match parts.as_slice() {
                ["rtl", first, last] => {
                    if let (Some(first), Some(last)) = (hex(first), hex(last)) {
                        rtl.push((first, last));
                    }
                }
                [first, last, class] => {
                    if let (Some(first), Some(last), Some(class)) =
                        (hex(first), hex(last), Class::parse(class))
                    {
                        classes.push((first, last, class));
                    }
                }
                _ => {}
            }
        }
        Tables { classes, rtl }
    })
}

fn find<T: Copy>(ranges: &[(u32, u32, T)], code_point: u32) -> Option<T> {
    let index = ranges.partition_point(|(_, last, _)| *last < code_point);
    ranges
        .get(index)
        .filter(|(first, _, _)| *first <= code_point)
        .map(|(_, _, value)| *value)
}

/// `TwitterCldr`'s bidi class of a character.
#[must_use]
pub fn bidi_class(c: char) -> Option<Class> {
    find(&tables().classes, u32::from(c))
}

/// `TextUtils::RTL_REGEXP` (`/[\p{Hebrew}\p{Arabic}]/`) matches the text.
#[must_use]
pub fn is_rtl(text: &str) -> bool {
    let rtl = &tables().rtl;
    text.chars().any(|c| {
        let code_point = u32::from(c);
        let index = rtl.partition_point(|(_, last)| *last < code_point);
        rtl.get(index)
            .is_some_and(|(first, _)| *first <= code_point)
    })
}

/// `TextUtils.maybe_rtl_reverse(text)`.
pub fn maybe_rtl_reverse(text: &str) -> Result<String, UnknownCodePoint> {
    if !is_rtl(text) {
        return Ok(text.to_owned());
    }
    let mut bidi = Bidi::from_string(&connect_arabic_letters(text))?;
    bidi.reorder_visually();
    Ok(bidi.string.into_iter().collect())
}

/// One letter of `ArabicLetterConnector`'s table: its isolated, final, initial and medial
/// forms, and whether it connects to the next letter.
struct Letter {
    forms: [char; 4],
    connects: bool,
}

fn letters() -> &'static HashMap<char, Letter> {
    static LETTERS: OnceLock<HashMap<char, Letter>> = OnceLock::new();
    LETTERS.get_or_init(|| {
        #[rustfmt::skip]
        let table: [(u32, [u32; 4], bool); 37] = [
            (0x0627, [0xFE8D, 0xFE8E, 0xFE8D, 0xFE8E], false), // Alef
            (0x0628, [0xFE8F, 0xFE90, 0xFE91, 0xFE92], true),  // Ba2
            (0x062A, [0xFE95, 0xFE96, 0xFE97, 0xFE98], true),  // Ta2
            (0x062B, [0xFE99, 0xFE9A, 0xFE9B, 0xFE9C], true),  // Tha2
            (0x062C, [0xFE9D, 0xFE9E, 0xFE9F, 0xFEA0], true),  // Jeem
            (0x062D, [0xFEA1, 0xFEA2, 0xFEA3, 0xFEA4], true),  // 7a2
            (0x062E, [0xFEA5, 0xFEA6, 0xFEA7, 0xFEA8], true),  // 7'a2
            (0x062F, [0xFEA9, 0xFEAA, 0xFEA9, 0xFEAA], false), // Dal
            (0x0630, [0xFEAB, 0xFEAC, 0xFEAB, 0xFEAC], false), // Thal
            (0x0631, [0xFEAD, 0xFEAE, 0xFEAD, 0xFEAE], false), // Ra2
            (0x0632, [0xFEAF, 0xFEB0, 0xFEAF, 0xFEB0], false), // Zain
            (0x0633, [0xFEB1, 0xFEB2, 0xFEB3, 0xFEB4], true),  // Seen
            (0x0634, [0xFEB5, 0xFEB6, 0xFEB7, 0xFEB8], true),  // Sheen
            (0x0635, [0xFEB9, 0xFEBA, 0xFEBB, 0xFEBC], true),  // 9ad
            (0x0636, [0xFEBD, 0xFEBE, 0xFEBF, 0xFEC0], true),  // 9'ad
            (0x0637, [0xFEC1, 0xFEC2, 0xFEC3, 0xFEC4], true),  // 6a2
            (0x0638, [0xFEC5, 0xFEC6, 0xFEC7, 0xFEC8], true),  // 6'a2
            (0x0639, [0xFEC9, 0xFECA, 0xFECB, 0xFECC], true),  // 3ain
            (0x063A, [0xFECD, 0xFECE, 0xFECF, 0xFED0], true),  // 3'ain
            (0x0641, [0xFED1, 0xFED2, 0xFED3, 0xFED4], true),  // Fa2
            (0x0642, [0xFED5, 0xFED6, 0xFED7, 0xFED8], true),  // Qaf
            (0x0643, [0xFED9, 0xFEDA, 0xFEDB, 0xFEDC], true),  // Kaf
            (0x0644, [0xFEDD, 0xFEDE, 0xFEDF, 0xFEE0], true),  // Lam
            (0x0645, [0xFEE1, 0xFEE2, 0xFEE3, 0xFEE4], true),  // Meem
            (0x0646, [0xFEE5, 0xFEE6, 0xFEE7, 0xFEE8], true),  // Noon
            (0x0647, [0xFEE9, 0xFEEA, 0xFEEB, 0xFEEC], true),  // Ha2
            (0x0648, [0xFEED, 0xFEEE, 0xFEED, 0xFEEE], false), // Waw
            (0x064A, [0xFEF1, 0xFEF2, 0xFEF3, 0xFEF4], true),  // Ya2
            (0x0621, [0xFE80, 0xFE80, 0xFE80, 0xFE80], false), // Hamza
            (0x0622, [0xFE81, 0xFE82, 0xFE81, 0xFE82], false), // Alef Madda
            (0x0623, [0xFE83, 0xFE84, 0xFE83, 0xFE84], false), // Alef Hamza Above
            (0x0624, [0xFE85, 0xFE86, 0xFE85, 0xFE86], false), // Waw Hamza
            (0x0625, [0xFE87, 0xFE88, 0xFE87, 0xFE88], false), // Alef Hamza Below
            (0x0626, [0xFE89, 0xFE8A, 0xFE8B, 0xFE8C], true),  // Ya2 Hamza
            (0x0629, [0xFE93, 0xFE94, 0xFE93, 0xFE94], false), // Ta2 Marbu6a
            (0x0640, [0x0640, 0x0640, 0x0640, 0x0640], true),  // Tatweel
            (0x0649, [0xFEEF, 0xFEF0, 0xFEEF, 0xFEF0], false), // Alef Layyina
        ];
        table
            .into_iter()
            .filter_map(|(common, forms, connects)| {
                let forms = forms.map(|f| char::from_u32(f).unwrap_or_default());
                Some((char::from_u32(common)?, Letter { forms, connects }))
            })
            .collect()
    })
}

/// `ArabicLetterConnector.transform(text)`: each Arabic letter in its isolated, final,
/// initial or medial form, by whether the letters around it are Arabic letters and
/// whether the previous one connects.
#[must_use]
pub fn connect_arabic_letters(text: &str) -> String {
    const ISOLATED: usize = 0;
    const FINAL: usize = 1;
    const INITIAL: usize = 2;
    const MEDIAL: usize = 3;
    let letters = letters();
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, c) in chars.iter().enumerate() {
        let Some(letter) = letters.get(c) else {
            out.push(*c);
            continue;
        };
        let previous = i.checked_sub(1).and_then(|p| letters.get(&chars[p]));
        let next = chars.get(i + 1).and_then(|n| letters.get(n));
        // `determine_form(previous_char, next_char)`.
        let form = match (previous, next) {
            (Some(previous), Some(_)) => {
                if previous.connects {
                    MEDIAL
                } else {
                    INITIAL
                }
            }
            (Some(previous), None) => {
                if previous.connects {
                    FINAL
                } else {
                    ISOLATED
                }
            }
            (None, Some(_)) => INITIAL,
            (None, None) => ISOLATED,
        };
        out.push(letter.forms[form]);
    }
    out
}

/// `TwitterCldr::Shared::Bidi`.
struct Bidi {
    string: Vec<char>,
    types: Vec<Class>,
    levels: Vec<i64>,
    runs: Vec<usize>,
    formatter_indices: Vec<usize>,
    base_embedding: i64,
    length: usize,
}

impl Bidi {
    /// `Bidi.from_string(str, direction: :RTL)`, which runs the algorithm.
    fn from_string(text: &str) -> Result<Self, UnknownCodePoint> {
        let string: Vec<char> = text.chars().collect();
        let types = string
            .iter()
            .map(|c| bidi_class(*c).ok_or(UnknownCodePoint(u32::from(*c))))
            .collect::<Result<Vec<_>, _>>()?;
        let length = types.len();
        let mut bidi = Self {
            string,
            types,
            levels: vec![0; length],
            runs: Vec::new(),
            formatter_indices: Vec::new(),
            // `compute_paragraph_embedding_level` with `direction: :RTL`.
            base_embedding: 1,
            length,
        };
        bidi.compute_explicit_levels();
        bidi.compute_runs();
        bidi.resolve_weak_types();
        bidi.resolve_neutral_types();
        bidi.resolve_implicit_levels();
        bidi.reinsert_formatting_codes();
        Ok(bidi)
    }

    /// `reorder_visually!`.
    fn reorder_visually(&mut self) {
        let mut max = 0;
        let mut lowest_odd = MAX_DEPTH + 1;
        for &level in &self.levels {
            max = max.max(level);
            if level % 2 != 0 {
                lowest_odd = lowest_odd.min(level);
            }
        }
        let size = self.levels.len();
        let mut depth = max;
        while depth >= lowest_odd {
            let mut start = 0;
            while start < size {
                while start < size && self.levels[start] < depth {
                    start += 1;
                }
                if start == size {
                    break;
                }
                let mut finish = start + 1;
                while finish < size && self.levels[finish] >= depth {
                    finish += 1;
                }
                for i in 0..(finish - start) / 2 {
                    self.levels.swap(finish - i - 1, start + i);
                    self.string.swap(finish - i - 1, start + i);
                }
                start = finish + 1;
            }
            depth -= 1;
        }
    }

    fn compute_explicit_levels(&mut self) {
        let mut current_embedding = self.base_embedding;
        let mut directional_override: Option<Class> = None;
        let mut embedding_stack: Vec<i64> = Vec::new();
        let mut sp = 0;
        for i in 0..self.length {
            let mut is_special = true;
            let kind = self.types[i];
            let is_ltr = matches!(kind, Class::LRE | Class::LRO);
            match kind {
                Class::RLE | Class::RLO | Class::LRE | Class::LRO => {
                    let new_embedding = if is_ltr {
                        (current_embedding & !1) + 2
                    } else {
                        (current_embedding + 1) | 1
                    };
                    if new_embedding < MAX_DEPTH {
                        if directional_override.is_some() {
                            current_embedding |= -0x80;
                        }
                        if sp < embedding_stack.len() {
                            embedding_stack[sp] = current_embedding;
                        } else {
                            embedding_stack.push(current_embedding);
                        }
                        current_embedding = new_embedding;
                        sp += 1;
                        directional_override = match kind {
                            Class::LRO => Some(Class::L),
                            Class::RLO => Some(Class::R),
                            _ => None,
                        };
                    }
                }
                Class::PDF => {
                    if sp > 0 {
                        sp -= 1;
                        let new_embedding = embedding_stack[sp];
                        current_embedding = new_embedding & 0x7f;
                        directional_override =
                            (new_embedding < 0).then_some(if new_embedding & 1 == 0 {
                                Class::L
                            } else {
                                Class::R
                            });
                    }
                }
                _ => is_special = false,
            }
            self.levels[i] = current_embedding;
            if is_special {
                self.formatter_indices.push(i);
            } else if let Some(class) = directional_override {
                self.types[i] = class;
            }
        }

        // Drop the formatting codes from `levels` and `types` (the arrays keep their size).
        let mut output = 0;
        let mut input = 0;
        let size = self.formatter_indices.len();
        for i in 0..=size {
            let next = if i == size {
                self.length
            } else {
                self.formatter_indices[i]
            };
            let len = next - input;
            self.levels.copy_within(input..input + len, output);
            self.types.copy_within(input..input + len, output);
            output += len;
            input = next + 1;
        }
        self.length -= size;
    }

    fn compute_runs(&mut self) {
        let mut current_embedding = self.base_embedding;
        let mut last_run_start = 0;
        self.runs.clear();
        for i in 0..self.length {
            if self.levels[i] != current_embedding {
                self.runs.push(last_run_start);
                last_run_start = i;
                current_embedding = self.levels[i];
            }
        }
        self.runs.push(last_run_start);
    }

    fn run_level(&self, run: usize) -> Option<i64> {
        self.levels.get(self.runs[run]).copied()
    }

    fn run_limit(&self, run: usize) -> usize {
        if run == self.runs.len() - 1 {
            self.length
        } else {
            self.runs[run + 1]
        }
    }

    fn direction(level: i64) -> Class {
        if level % 2 == 0 { Class::L } else { Class::R }
    }

    fn resolve_weak_types(&mut self) {
        let run_count = self.runs.len();
        let mut previous_level = self.base_embedding;
        for run in 0..run_count {
            let start = self.runs[run];
            let finish = self.run_limit(run);
            let level = self.run_level(run).unwrap_or(0);
            let sor = Self::direction(previous_level.max(level));
            let next_level = if run == run_count - 1 {
                self.base_embedding
            } else {
                self.run_level(run + 1).unwrap_or(0)
            };
            let eor = Self::direction(level.max(next_level));
            let mut prev_type = sor;
            let mut prev_strong_type = sor;
            for i in start..finish {
                let next_type = if i == finish - 1 {
                    eor
                } else {
                    self.types[i + 1]
                };
                if self.types[i] == Class::NSM {
                    self.types[i] = prev_type;
                } else {
                    prev_type = self.types[i];
                }
                if self.types[i] == Class::EN {
                    if prev_strong_type == Class::AL {
                        self.types[i] = Class::AN;
                    }
                } else if matches!(self.types[i], Class::L | Class::R | Class::AL) {
                    prev_strong_type = self.types[i];
                }
                if self.types[i] == Class::AL {
                    self.types[i] = Class::R;
                }
                if prev_type == Class::EN && next_type == Class::EN {
                    // `@types[i] = nextType` (a NameError in Ruby), never reached: `prev_type`
                    // is this character's type, which is not EN when it is ES or CS.
                    if matches!(self.types[i], Class::ES | Class::CS) {
                        self.types[i] = next_type;
                    }
                } else if prev_type == Class::AN
                    && next_type == Class::AN
                    && self.types[i] == Class::CS
                {
                    self.types[i] = next_type;
                }
                if matches!(self.types[i], Class::ET | Class::BN) {
                    if prev_type == Class::EN {
                        self.types[i] = prev_type;
                    } else {
                        // `while j < finish && @types[j] == :ET || @types[j] == :BN`.
                        let mut j = i + 1;
                        while (j < finish && self.types.get(j) == Some(&Class::ET))
                            || self.types.get(j) == Some(&Class::BN)
                        {
                            j += 1;
                        }
                        if j < finish && self.types[j] == Class::EN {
                            for k in i..j {
                                self.types[k] = Class::EN;
                            }
                        }
                    }
                }
                if matches!(self.types[i], Class::ET | Class::CS | Class::BN) {
                    self.types[i] = Class::ON;
                }
                if prev_strong_type == Class::L && self.types[i] == Class::EN {
                    self.types[i] = prev_strong_type;
                }
            }
            previous_level = level;
        }
    }

    fn resolve_neutral_types(&mut self) {
        let run_count = self.runs.len();
        let mut previous_level = self.base_embedding;
        for run in 0..run_count {
            let start = self.runs[run];
            let finish = self.run_limit(run);
            let Some(level) = self.run_level(run) else {
                continue;
            };
            let embedding_direction = Self::direction(level);
            let sor = Self::direction(previous_level.max(level));
            let next_level = if run == run_count - 1 {
                Some(self.base_embedding)
            } else {
                self.run_level(run + 1)
            };
            // `[level, nil].max` raises in Ruby; a run always has a level here.
            let eor = Self::direction(level.max(next_level.unwrap_or(level)));
            let mut prev_strong = sor;
            let mut neutral_start: Option<usize> = None;
            for i in start..=finish {
                let this_type = if i == finish { eor } else { self.types[i] };
                let new_strong = match this_type {
                    Class::L => Some(Class::L),
                    Class::R | Class::AN | Class::EN => Some(Class::R),
                    Class::BN | Class::ON | Class::S | Class::B | Class::WS => {
                        neutral_start.get_or_insert(i);
                        None
                    }
                    _ => None,
                };
                if let Some(new_strong) = new_strong {
                    if let Some(neutral) = neutral_start {
                        let value = if prev_strong == new_strong {
                            prev_strong
                        } else {
                            embedding_direction
                        };
                        for j in neutral..i {
                            self.types[j] = value;
                        }
                    }
                    prev_strong = new_strong;
                    neutral_start = None;
                }
            }
            previous_level = level;
        }
        let _ = previous_level;
    }

    fn resolve_implicit_levels(&mut self) {
        for i in 0..self.length {
            let kind = self.types[i];
            if self.levels[i] & 1 == 0 {
                if kind == Class::R {
                    self.levels[i] += 1;
                } else if matches!(kind, Class::AN | Class::EN) {
                    self.levels[i] += 2;
                }
            } else if matches!(kind, Class::L | Class::AN | Class::EN) {
                self.levels[i] += 1;
            }
        }
    }

    fn reinsert_formatting_codes(&mut self) {
        let size = self.levels.len();
        let mut input = self.length;
        let mut output = size;
        for index in (0..self.formatter_indices.len()).rev() {
            let next = self.formatter_indices[index];
            let len = output - next - 1;
            output = next;
            input -= len;
            if next + 1 < size {
                self.levels.copy_within(input..input + len, next + 1);
            }
            let right_level = if output == size - 1 {
                self.base_embedding
            } else {
                self.levels.get(output + 1).copied().unwrap_or(0)
            };
            let left_level = if input == 0 {
                self.base_embedding
            } else {
                self.levels.get(input).copied().unwrap_or(0)
            };
            self.levels[output] = left_level.max(right_level);
        }
        self.length = size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_like_twitter_cldr() {
        assert_eq!(bidi_class('a'), Some(Class::L));
        assert_eq!(bidi_class('א'), Some(Class::R));
        assert_eq!(bidi_class('ب'), Some(Class::AL));
        assert_eq!(bidi_class('1'), Some(Class::EN));
        assert_eq!(bidi_class(' '), Some(Class::WS));
        assert_eq!(bidi_class('\u{0378}'), None, "unassigned");
        assert_eq!(
            bidi_class('\u{4E00}'),
            Some(Class::L),
            "a <CJK…, First> range"
        );
    }

    #[test]
    fn matches_the_rtl_regexp() {
        assert!(is_rtl("שלום"));
        assert!(is_rtl("abc مرحبا"));
        assert!(!is_rtl("abc"));
        assert!(
            !is_rtl("\u{060C}"),
            "the Arabic comma is Common, not Arabic"
        );
    }

    #[test]
    fn connects_arabic_letters() {
        // بيت: initial ba, medial ya, final ta.
        assert_eq!(connect_arabic_letters("بيت"), "\u{FE91}\u{FEF4}\u{FE96}");
        // دار: dal and alef do not connect, so alef and ra stay isolated.
        assert_eq!(connect_arabic_letters("دار"), "\u{FEA9}\u{FE8D}\u{FEAD}");
        assert_eq!(connect_arabic_letters("a ب"), "a \u{FE8F}");
    }

    #[test]
    fn reorders_visually() {
        let reversed = |s: &str| maybe_rtl_reverse(s).unwrap();
        assert_eq!(reversed("hello"), "hello");
        assert_eq!(reversed("שלום"), "םולש");
        assert_eq!(reversed("שלום world"), "world םולש");
        assert_eq!(reversed("שלום 123"), "123 םולש");
        assert!(maybe_rtl_reverse("שלום \u{0378}").is_err());
    }
}
