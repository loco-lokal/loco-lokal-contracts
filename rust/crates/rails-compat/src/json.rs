//! JSON as the `json` gem (2.21) and Active Support generate it: floats through the gem's
//! vendored `fpconv` (Grisu2, `.0` on whole numbers, plain decimals down to 1e-9), and
//! `<`, `>`, `&`, U+2028 and U+2029 escaped (`ActiveSupport.escape_html_entities_in_json`).
//!
//! The float conversion is ported from `ext/json/ext/vendor/fpconv.c`, which the gem took
//! from <https://github.com/night-shift/fpconv> (Boost Software License 1.0).

use serde_json::Value;

/// `value.to_json`.
#[must_use]
pub fn generate(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => out.push_str(&i.to_string()),
            (_, Some(u), _) => out.push_str(&u.to_string()),
            (_, _, Some(f)) => out.push_str(&float(f)),
            _ => out.push_str(&n.to_string()),
        },
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, s: &str) {
    let quoted = Value::String(s.to_owned()).to_string();
    for c in quoted.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
}

const NPOWERS: i32 = 87;
const STEPPOWERS: i32 = 8;
const FIRSTPOWER: i32 = -348;
const EXPMAX: i32 = -32;
const EXPMIN: i32 = -60;

#[derive(Clone, Copy)]
struct Fp {
    frac: u64,
    exp: i32,
}

#[rustfmt::skip]
const POWERS_TEN: [(u64, i32); 87] = [
    (18_054_884_314_459_144_840, -1220), (13_451_937_075_301_367_670, -1193),
    (10_022_474_136_428_063_862, -1166), (14_934_650_266_808_366_570, -1140),
    (11_127_181_549_972_568_877, -1113), (16_580_792_590_934_885_855, -1087),
    (12_353_653_155_963_782_858, -1060), (18_408_377_700_990_114_895, -1034),
    (13_715_310_171_984_221_708, -1007), (10_218_702_384_817_765_436, -980),
    (15_227_053_142_812_498_563, -954), (11_345_038_669_416_679_861, -927),
    (16_905_424_996_341_287_883, -901), (12_595_523_146_049_147_757, -874),
    (9_384_396_036_005_875_287, -847), (13_983_839_803_942_852_151, -821),
    (10_418_772_551_374_772_303, -794), (15_525_180_923_007_089_351, -768),
    (11_567_161_174_868_858_868, -741), (17_236_413_322_193_710_309, -715),
    (12_842_128_665_889_583_758, -688), (9_568_131_466_127_621_947, -661),
    (14_257_626_930_069_360_058, -635), (10_622_759_856_335_341_974, -608),
    (15_829_145_694_278_690_180, -582), (11_793_632_577_567_316_726, -555),
    (17_573_882_009_934_360_870, -529), (13_093_562_431_584_567_480, -502),
    (9_755_464_219_737_475_723, -475), (14_536_774_485_912_137_811, -449),
    (10_830_740_992_659_433_045, -422), (16_139_061_738_043_178_685, -396),
    (12_024_538_023_802_026_127, -369), (17_917_957_937_422_433_684, -343),
    (13_349_918_974_505_688_015, -316), (9_946_464_728_195_732_843, -289),
    (14_821_387_422_376_473_014, -263), (11_042_794_154_864_902_060, -236),
    (16_455_045_573_212_060_422, -210), (12_259_964_326_927_110_867, -183),
    (18_268_770_466_636_286_478, -157), (13_611_294_676_837_538_539, -130),
    (10_141_204_801_825_835_212, -103), (15_111_572_745_182_864_684, -77),
    (11_258_999_068_426_240_000, -50), (16_777_216_000_000_000_000, -24),
    (12_500_000_000_000_000_000, 3), (9_313_225_746_154_785_156, 30),
    (13_877_787_807_814_456_755, 56), (10_339_757_656_912_845_936, 83),
    (15_407_439_555_097_886_824, 109), (11_479_437_019_748_901_445, 136),
    (17_105_694_144_590_052_135, 162), (12_744_735_289_059_618_216, 189),
    (9_495_567_745_759_798_747, 216), (14_149_498_560_666_738_074, 242),
    (10_542_197_943_230_523_224, 269), (15_709_099_088_952_724_970, 295),
    (11_704_190_886_730_495_818, 322), (17_440_603_504_673_385_349, 348),
    (12_994_262_207_056_124_023, 375), (9_681_479_787_123_295_682, 402),
    (14_426_529_090_290_212_157, 428), (10_748_601_772_107_342_003, 455),
    (16_016_664_761_464_807_395, 481), (11_933_345_169_920_330_789, 508),
    (17_782_069_995_880_619_868, 534), (13_248_674_568_444_952_270, 561),
    (9_871_031_767_461_413_346, 588), (14_708_983_551_653_345_445, 614),
    (10_959_046_745_042_015_199, 641), (16_330_252_207_878_254_650, 667),
    (12_166_986_024_289_022_870, 694), (18_130_221_999_122_236_476, 720),
    (13_508_068_024_458_167_312, 747), (10_064_294_952_495_520_794, 774),
    (14_996_968_138_956_309_548, 800), (11_173_611_982_879_273_257, 827),
    (16_649_979_327_439_178_909, 853), (12_405_201_291_620_119_593, 880),
    (9_242_595_204_427_927_429, 907), (13_772_540_099_066_387_757, 933),
    (10_261_342_003_245_940_623, 960), (15_290_591_125_556_738_113, 986),
    (11_392_378_155_556_871_081, 1013), (16_975_966_327_722_178_521, 1039),
    (12_648_080_533_535_911_531, 1066),
];

const TENS: [u64; 20] = [
    10_000_000_000_000_000_000,
    1_000_000_000_000_000_000,
    100_000_000_000_000_000,
    10_000_000_000_000_000,
    1_000_000_000_000_000,
    100_000_000_000_000,
    10_000_000_000_000,
    1_000_000_000_000,
    100_000_000_000,
    10_000_000_000,
    1_000_000_000,
    100_000_000,
    10_000_000,
    1_000_000,
    100_000,
    10_000,
    1_000,
    100,
    10,
    1,
];

const FRACMASK: u64 = 0x000F_FFFF_FFFF_FFFF;
const EXPMASK: u64 = 0x7FF0_0000_0000_0000;
const HIDDENBIT: u64 = 0x0010_0000_0000_0000;
const SIGNMASK: u64 = 0x8000_0000_0000_0000;
const EXPBIAS: i32 = 1023 + 52;

fn find_cachedpow10(exp: i32) -> (Fp, i32) {
    const ONE_LOG_TEN: f64 = 0.301_029_995_663_981_14;
    #[allow(clippy::cast_possible_truncation, reason = "fpconv's (int) cast")]
    let approx = (-f64::from(exp + NPOWERS) * ONE_LOG_TEN) as i32;
    let mut idx = (approx - FIRSTPOWER) / STEPPOWERS;
    loop {
        let (frac, pexp) = POWERS_TEN[usize::try_from(idx).unwrap_or_default()];
        let current = exp + pexp + 64;
        if current < EXPMIN {
            idx += 1;
            continue;
        }
        if current > EXPMAX {
            idx -= 1;
            continue;
        }
        return (Fp { frac, exp: pexp }, FIRSTPOWER + idx * STEPPOWERS);
    }
}

fn build_fp(d: f64) -> Fp {
    let bits = d.to_bits();
    let mut fp = Fp {
        frac: bits & FRACMASK,
        exp: i32::try_from((bits & EXPMASK) >> 52).unwrap_or_default(),
    };
    if fp.exp == 0 {
        fp.exp = -EXPBIAS + 1;
    } else {
        fp.frac += HIDDENBIT;
        fp.exp -= EXPBIAS;
    }
    fp
}

fn normalize(fp: &mut Fp) {
    while fp.frac & HIDDENBIT == 0 {
        fp.frac <<= 1;
        fp.exp -= 1;
    }
    let shift = 64 - 52 - 1;
    fp.frac <<= shift;
    fp.exp -= shift;
}

fn normalized_boundaries(fp: &Fp) -> (Fp, Fp) {
    let mut upper = Fp {
        frac: (fp.frac << 1) + 1,
        exp: fp.exp - 1,
    };
    while upper.frac & (HIDDENBIT << 1) == 0 {
        upper.frac <<= 1;
        upper.exp -= 1;
    }
    let u_shift = 64 - 52 - 2;
    upper.frac <<= u_shift;
    upper.exp -= u_shift;

    let l_shift = if fp.frac == HIDDENBIT { 2 } else { 1 };
    let mut lower = Fp {
        frac: (fp.frac << l_shift) - 1,
        exp: fp.exp - l_shift,
    };
    lower.frac <<= lower.exp - upper.exp;
    lower.exp = upper.exp;
    (lower, upper)
}

#[allow(clippy::similar_names, reason = "fpconv's names")]
fn multiply(a: &Fp, b: &Fp) -> Fp {
    const LOMASK: u64 = 0x0000_0000_FFFF_FFFF;
    let ah_bl = (a.frac >> 32) * (b.frac & LOMASK);
    let al_bh = (a.frac & LOMASK) * (b.frac >> 32);
    let al_bl = (a.frac & LOMASK) * (b.frac & LOMASK);
    let ah_bh = (a.frac >> 32) * (b.frac >> 32);
    let mut tmp = (ah_bl & LOMASK) + (al_bh & LOMASK) + (al_bl >> 32);
    tmp += 1 << 31;
    Fp {
        frac: ah_bh + (ah_bl >> 32) + (al_bh >> 32) + (tmp >> 32),
        exp: a.exp + b.exp + 64,
    }
}

fn round_digit(digits: &mut [u8], ndigits: usize, delta: u64, mut rem: u64, kappa: u64, frac: u64) {
    while rem < frac
        && delta - rem >= kappa
        && (rem + kappa < frac || frac - rem > rem + kappa - frac)
    {
        digits[ndigits - 1] -= 1;
        rem += kappa;
    }
}

fn generate_digits(fp: &Fp, upper: &Fp, lower: &Fp, digits: &mut [u8; 18], k: &mut i32) -> usize {
    let wfrac = upper.frac - fp.frac;
    let mut delta = upper.frac - lower.frac;
    let shift = u32::try_from(-upper.exp).unwrap_or_default();
    let one_frac = 1u64 << shift;

    let mut part1 = upper.frac >> shift;
    let mut part2 = upper.frac & (one_frac - 1);

    let mut idx = 0;
    let mut kappa = 10;
    for &div in &TENS[10..] {
        if kappa <= 0 {
            break;
        }
        let digit = part1 / div;
        if digit != 0 || idx != 0 {
            digits[idx] = b'0' + u8::try_from(digit).unwrap_or_default();
            idx += 1;
        }
        part1 -= digit * div;
        kappa -= 1;
        let tmp = (part1 << shift) + part2;
        if tmp <= delta {
            *k += kappa;
            round_digit(digits, idx, delta, tmp, div << shift, wfrac);
            return idx;
        }
    }

    let mut unit = 18;
    loop {
        part2 = part2.wrapping_mul(10);
        delta = delta.wrapping_mul(10);
        kappa -= 1;
        let digit = part2 >> shift;
        if digit != 0 || idx != 0 {
            digits[idx] = b'0' + u8::try_from(digit).unwrap_or_default();
            idx += 1;
        }
        part2 &= one_frac - 1;
        if part2 < delta {
            *k += kappa;
            round_digit(
                digits,
                idx,
                delta,
                part2,
                one_frac,
                wfrac.wrapping_mul(TENS[unit]),
            );
            return idx;
        }
        unit -= 1;
    }
}

fn grisu2(d: f64, digits: &mut [u8; 18]) -> (usize, i32) {
    let mut w = build_fp(d);
    let (lower, upper) = normalized_boundaries(&w);
    normalize(&mut w);
    let (cp, k) = find_cachedpow10(upper.exp);
    let w = multiply(&w, &cp);
    let mut upper = multiply(&upper, &cp);
    let mut lower = multiply(&lower, &cp);
    lower.frac += 1;
    upper.frac -= 1;
    let mut big_k = -k;
    let ndigits = generate_digits(&w, &upper, &lower, digits, &mut big_k);
    (ndigits, big_k)
}

fn emit_digits(digits: &[u8], k: i32, neg: bool, out: &mut String) {
    let ndigits = i32::try_from(digits.len()).unwrap_or_default();
    let mut exp = (k + ndigits - 1).abs();
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();

    if k >= 0 && exp < 15 {
        out.push_str(&text(digits));
        out.extend(std::iter::repeat_n(
            '0',
            usize::try_from(k).unwrap_or_default(),
        ));
        out.push_str(".0");
        return;
    }
    if k < 0 && (k > -7 || exp < 10) {
        let offset = ndigits - k.abs();
        if offset <= 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n(
                '0',
                usize::try_from(-offset).unwrap_or_default(),
            ));
            out.push_str(&text(digits));
        } else {
            let offset = usize::try_from(offset).unwrap_or_default();
            out.push_str(&text(&digits[..offset]));
            out.push('.');
            out.push_str(&text(&digits[offset..]));
        }
        return;
    }

    let ndigits = digits.len().min(18 - usize::from(neg));
    out.push(char::from(digits[0]));
    if ndigits > 1 {
        out.push('.');
        out.push_str(&text(&digits[1..ndigits]));
    }
    out.push('e');
    out.push(if k + i32::try_from(ndigits).unwrap_or_default() - 1 < 0 {
        '-'
    } else {
        '+'
    });
    let mut cent = 0;
    if exp > 99 {
        cent = exp / 100;
        out.push(char::from(b'0' + u8::try_from(cent).unwrap_or_default()));
        exp -= cent * 100;
    }
    if exp > 9 {
        let dec = exp / 10;
        out.push(char::from(b'0' + u8::try_from(dec).unwrap_or_default()));
        exp -= dec * 10;
    } else if cent != 0 {
        out.push('0');
    }
    out.push(char::from(
        b'0' + u8::try_from(exp % 10).unwrap_or_default(),
    ));
}

/// A float as `JSON.generate` writes it (`fpconv_dtoa`). Infinite and NaN values, which
/// the gem refuses, come out as `Float#to_s` does.
#[must_use]
pub fn float(d: f64) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let mut out = String::new();
    let neg = d.to_bits() & SIGNMASK != 0;
    if neg {
        out.push('-');
    }
    if d == 0.0 {
        out.push_str("0.0");
        return out;
    }
    let mut digits = [0u8; 18];
    let (ndigits, k) = grisu2(d, &mut digits);
    emit_digits(&digits[..ndigits], k, neg, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_floats_like_the_json_gem() {
        // Output of `[…].to_json` in the Rails image (json 2.21.2).
        let cases: &[(f64, &str)] = &[
            (1.5e-05, "0.000015"),
            (0.0001, "0.0001"),
            (0.000_123_456_78, "0.00012345678"),
            (1e20, "1e+20"),
            (1e16, "1e+16"),
            (1e15, "1e+15"),
            (0.1 + 0.2, "0.30000000000000004"),
            (100.0, "100.0"),
            (-0.0, "-0.0"),
            (2.5e-7, "0.00000025"),
            (1e-10, "1e-10"),
            (1.234e-12, "1.234e-12"),
            (5e-324, "5e-324"),
            (1.5e-30, "1.5e-30"),
            (1e-17, "1e-17"),
            (1.25e-16, "1.25e-16"),
            (1e-15, "1e-15"),
            (12_345_678.9, "12345678.9"),
            (1e7, "10000000.0"),
            (123_456_789_012_345.6, "123456789012345.6"),
            (1_234_567.0, "1234567.0"),
            (0.1, "0.1"),
            (1.0 / 3.0, "0.3333333333333333"),
            (2.0 / 3.0 * 1e-5, "0.000006666666666666667"),
            (1e-7, "0.0000001"),
            (1.5e-8, "0.000000015"),
            (1e-8, "0.00000001"),
            (1.234_567_890_123_456_7e-8, "0.000000012345678901234567"),
            (1e-9, "0.000000001"),
            (9.99e-10, "9.99e-10"),
            (1.234_567_890_123_456_7e-9, "0.0000000012345678901234566"),
            (3.3e-9, "0.0000000033"),
            (1.5e16, "1.5e+16"),
            (9.5e15, "9.5e+15"),
            (1_234_567_890_123_456.7, "1234567890123456.7"),
        ];
        for &(value, expected) in cases {
            assert_eq!(float(value), expected, "{value:e}");
        }
    }

    #[test]
    fn escapes_html_entities() {
        let value = serde_json::json!(["<a & b>\u{2028}", [1, 0.5, -2]]);
        assert_eq!(
            generate(&value),
            r#"["\u003ca \u0026 b\u003e\u2028",[1,0.5,-2]]"#
        );
    }
}
