//! The JPEG files libvips writes for `write_to_buffer('.jpg', interlace: true, Q:)` with
//! libjpeg-turbo: the EXIF block libvips adds (no JFIF header), the standard quantization
//! tables scaled to `Q`, 4:2:0 chroma subsampling, the islow DCT, and libjpeg's simple
//! progression script with optimized Huffman tables (`jpeg_simple_progression`). The
//! output is byte for byte what Rails stores for lazily generated `preview_images`.
#![allow(
    clippy::similar_names,
    clippy::many_single_char_names,
    reason = "names follow libjpeg's"
)]

/// `jpeg_natural_order`: the zigzag index of each coefficient.
const NATURAL_ORDER: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// `std_luminance_quant_tbl` and `std_chrominance_quant_tbl` (natural order).
const LUMA_QUANT: [u32; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const CHROMA_QUANT: [u32; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// `MAX_CORR_BITS` of `jcphuff.c`.
const MAX_CORR_BITS: usize = 1000;

/// One scan of the progression: component indexes, spectral band and point transform.
struct Scan {
    components: &'static [usize],
    ss: usize,
    se: usize,
    ah: u32,
    al: u32,
}

/// `jpeg_simple_progression` for a YCbCr image.
const SCANS: [Scan; 10] = [
    Scan {
        components: &[0, 1, 2],
        ss: 0,
        se: 0,
        ah: 0,
        al: 1,
    },
    Scan {
        components: &[0],
        ss: 1,
        se: 5,
        ah: 0,
        al: 2,
    },
    Scan {
        components: &[2],
        ss: 1,
        se: 63,
        ah: 0,
        al: 1,
    },
    Scan {
        components: &[1],
        ss: 1,
        se: 63,
        ah: 0,
        al: 1,
    },
    Scan {
        components: &[0],
        ss: 6,
        se: 63,
        ah: 0,
        al: 2,
    },
    Scan {
        components: &[0],
        ss: 1,
        se: 63,
        ah: 2,
        al: 1,
    },
    Scan {
        components: &[0, 1, 2],
        ss: 0,
        se: 0,
        ah: 1,
        al: 0,
    },
    Scan {
        components: &[2],
        ss: 1,
        se: 63,
        ah: 1,
        al: 0,
    },
    Scan {
        components: &[1],
        ss: 1,
        se: 63,
        ah: 1,
        al: 0,
    },
    Scan {
        components: &[0],
        ss: 1,
        se: 63,
        ah: 1,
        al: 0,
    },
];

/// The quantized DCT blocks of one component, including libjpeg's dummy blocks.
struct Component {
    id: u8,
    h: usize,
    v: usize,
    /// Quantization and Huffman table number (0 for luma, 1 for chroma).
    table: usize,
    width_in_blocks: usize,
    height_in_blocks: usize,
    /// Blocks per row in `blocks` (padded to a whole MCU).
    stride: usize,
    blocks: Vec<[i32; 64]>,
}

/// Encodes RGBA rows (alpha ignored, as for the opaque page bitmaps) like libvips'
/// `jpegsave` with `interlace: true` and `Q: quality` (below 90, so 4:2:0).
#[must_use]
pub fn encode_rgba(rgba: &[u8], width: usize, height: usize, quality: u32) -> Vec<u8> {
    let (y, cb, cr) = rgb_to_ycc(rgba, 4, width, height);
    let luma_q = quant_table(&LUMA_QUANT, quality);
    let chroma_q = quant_table(&CHROMA_QUANT, quality);

    let mcus_across = width.div_ceil(16);
    let mcus_down = height.div_ceil(16);
    let components = [
        luma_component(&y, width, height, &luma_q, mcus_across, mcus_down),
        chroma_component(2, &cb, width, height, &chroma_q),
        chroma_component(3, &cr, width, height, &chroma_q),
    ];

    let mut out = vec![0xFF, 0xD8];
    out.extend_from_slice(&exif_segment(width, height));
    for (index, table) in [&luma_q, &chroma_q].into_iter().enumerate() {
        out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43]);
        out.push(u8::try_from(index).unwrap_or_default());
        out.extend(
            NATURAL_ORDER
                .iter()
                .map(|&i| u8::try_from(table[i]).unwrap_or(255)),
        );
    }
    out.extend_from_slice(&[0xFF, 0xC2, 0x00, 0x11, 0x08]);
    out.extend_from_slice(&u16::try_from(height).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(&u16::try_from(width).unwrap_or(u16::MAX).to_be_bytes());
    out.push(3);
    for c in &components {
        out.extend_from_slice(&[
            c.id,
            u8::try_from((c.h << 4) | c.v).unwrap_or_default(),
            u8::try_from(c.table).unwrap_or_default(),
        ]);
    }

    for scan in &SCANS {
        let mut stats = Entropy::new(scan, None);
        stats.encode(&components, mcus_across, mcus_down);
        let tables = stats.optimal_tables();

        let mut emitted = [false; 2];
        for &ci in scan.components {
            let table = components[ci].table;
            if let Some(huff) = &tables[table]
                && !emitted[table]
            {
                emitted[table] = true;
                let class = if scan.ss == 0 { 0x00 } else { 0x10 };
                huff.write_dht(&mut out, class | u8::try_from(table).unwrap_or_default());
            }
        }
        write_sos(&mut out, scan, &components);

        let mut entropy = Entropy::new(scan, Some(tables.map(|t| t.map(|t| t.derive()))));
        entropy.encode(&components, mcus_across, mcus_down);
        entropy.finish();
        out.extend_from_slice(&entropy.bytes);
    }
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

/// `std_huff_tables` (`jcparam.c`): code counts per length (1 to 16) and values of the
/// DC and AC tables, luma then chroma.
const STD_DC_BITS: [[u8; 16]; 2] = [
    [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0],
];
const STD_DC_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const STD_AC_BITS: [[u8; 16]; 2] = [
    [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125],
    [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119],
];
const STD_AC_VALUES: [[u8; 162]; 2] = [
    [
        0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61,
        0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52,
        0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25,
        0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
        0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
        0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83,
        0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
        0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
        0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3,
        0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8,
        0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ],
    [
        0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
        0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33,
        0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18,
        0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
        0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63,
        0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a,
        0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
        0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4,
        0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
        0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7,
        0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ],
];

fn std_table(bits: &[u8; 16], values: &[u8]) -> HuffTable {
    let mut table_bits = [0u8; 17];
    table_bits[1..].copy_from_slice(bits);
    HuffTable {
        bits: table_bits,
        values: values.to_vec(),
    }
}

/// Encodes gray (`bands` 1) or RGB (`bands` 3) samples like libvips' `jpegsave` with
/// `Q: quality, strip: true`: a baseline JPEG with the standard Huffman tables and no
/// APP segments, chroma subsampled 4:2:0 below `Q` 90 and not at all from 90 on.
#[must_use]
pub fn encode_baseline(
    samples: &[u8],
    width: usize,
    height: usize,
    bands: usize,
    quality: u32,
) -> Vec<u8> {
    let luma_q = quant_table(&LUMA_QUANT, quality);
    let chroma_q = quant_table(&CHROMA_QUANT, quality);
    let full = |id: u8, plane: &[u8], qtable: &[u32; 64], table: usize| {
        let (cols, rows) = (width.div_ceil(8), height.div_ceil(8));
        let sample = |x: usize, y: usize| plane[y.min(height - 1) * width + x.min(width - 1)];
        blocks_of(id, 1, qtable, table, (cols, rows), (cols, rows), sample)
    };
    let (components, mcus) = if bands == 1 {
        let plane: Vec<u8> = samples.iter().take(width * height).copied().collect();
        (
            vec![full(1, &plane, &luma_q, 0)],
            (width.div_ceil(8), height.div_ceil(8)),
        )
    } else {
        let (y, cb, cr) = rgb_to_ycc(samples, bands, width, height);
        if quality < 90 {
            let mcus = (width.div_ceil(16), height.div_ceil(16));
            (
                vec![
                    luma_component(&y, width, height, &luma_q, mcus.0, mcus.1),
                    chroma_component(2, &cb, width, height, &chroma_q),
                    chroma_component(3, &cr, width, height, &chroma_q),
                ],
                mcus,
            )
        } else {
            (
                vec![
                    full(1, &y, &luma_q, 0),
                    full(2, &cb, &chroma_q, 1),
                    full(3, &cr, &chroma_q, 1),
                ],
                (width.div_ceil(8), height.div_ceil(8)),
            )
        }
    };

    let mut out = vec![0xFF, 0xD8];
    let tables: &[&[u32; 64]] = if bands == 1 {
        &[&luma_q]
    } else {
        &[&luma_q, &chroma_q]
    };
    for (index, table) in tables.iter().enumerate() {
        out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43]);
        out.push(u8::try_from(index).unwrap_or_default());
        out.extend(
            NATURAL_ORDER
                .iter()
                .map(|&i| u8::try_from(table[i]).unwrap_or(255)),
        );
    }
    out.extend_from_slice(&[0xFF, 0xC0]);
    out.extend_from_slice(
        &u16::try_from(8 + 3 * components.len())
            .unwrap_or_default()
            .to_be_bytes(),
    );
    out.push(8);
    out.extend_from_slice(&u16::try_from(height).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(&u16::try_from(width).unwrap_or(u16::MAX).to_be_bytes());
    out.push(u8::try_from(components.len()).unwrap_or_default());
    for c in &components {
        out.extend_from_slice(&[
            c.id,
            u8::try_from((c.h << 4) | c.v).unwrap_or_default(),
            u8::try_from(c.table).unwrap_or_default(),
        ]);
    }
    let table_count = tables.len();
    let mut derived = Vec::new();
    for table in 0..table_count {
        let dc = std_table(&STD_DC_BITS[table], &STD_DC_VALUES);
        let ac = std_table(&STD_AC_BITS[table], &STD_AC_VALUES[table]);
        let index = u8::try_from(table).unwrap_or_default();
        dc.write_dht(&mut out, index);
        ac.write_dht(&mut out, 0x10 | index);
        derived.push((dc.derive(), ac.derive()));
    }
    out.extend_from_slice(&[0xFF, 0xDA]);
    out.extend_from_slice(
        &u16::try_from(6 + 2 * components.len())
            .unwrap_or_default()
            .to_be_bytes(),
    );
    out.push(u8::try_from(components.len()).unwrap_or_default());
    for c in &components {
        let table = u8::try_from(c.table).unwrap_or_default();
        out.extend_from_slice(&[c.id, (table << 4) | table]);
    }
    out.extend_from_slice(&[0, 63, 0]);

    let mut writer = BitWriter::default();
    let mut last_dc = vec![0i32; components.len()];
    for my in 0..mcus.1 {
        for mx in 0..mcus.0 {
            for (ci, c) in components.iter().enumerate() {
                let (dc, ac) = &derived[c.table];
                for y in 0..c.v {
                    for x in 0..c.h {
                        let block = &c.blocks[(my * c.v + y) * c.stride + mx * c.h + x];
                        writer.encode_block(block, &mut last_dc[ci], dc, ac);
                    }
                }
            }
        }
    }
    writer.emit_bits(0x7F, 7);
    out.extend_from_slice(&writer.bytes);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

/// `jchuff.c`'s bit buffer and `encode_one_block`.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    put_buffer: u32,
    put_bits: u32,
}

impl BitWriter {
    fn emit_bits(&mut self, code: u32, size: u32) {
        if size == 0 {
            return;
        }
        let mut buffer = code & ((1 << size) - 1);
        self.put_bits += size;
        buffer <<= 24 - self.put_bits;
        buffer |= self.put_buffer;
        while self.put_bits >= 8 {
            let c = u8::try_from((buffer >> 16) & 0xFF).unwrap_or_default();
            self.bytes.push(c);
            if c == 0xFF {
                self.bytes.push(0);
            }
            buffer <<= 8;
            self.put_bits -= 8;
        }
        self.put_buffer = buffer & 0x00FF_FFFF;
    }

    fn emit_symbol(&mut self, table: &Derived, symbol: usize) {
        self.emit_bits(table.code[symbol], u32::from(table.size[symbol]));
    }

    /// The low `nbits` of a coefficient, one less when it is negative.
    #[allow(clippy::cast_sign_loss, reason = "libjpeg emits the low bits of temp2")]
    fn emit_value(&mut self, value: i32, nbits: u32) {
        let bits = if value < 0 { value - 1 } else { value };
        self.emit_bits(bits as u32, nbits);
    }

    fn encode_block(&mut self, block: &[i32; 64], last_dc: &mut i32, dc: &Derived, ac: &Derived) {
        let diff = block[0] - *last_dc;
        *last_dc = block[0];
        let nbits = diff.unsigned_abs().bit_width();
        self.emit_symbol(dc, usize::try_from(nbits).unwrap_or_default());
        self.emit_value(diff, nbits);
        let mut r = 0;
        for &index in &NATURAL_ORDER[1..] {
            let coef = block[index];
            if coef == 0 {
                r += 1;
                continue;
            }
            while r > 15 {
                self.emit_symbol(ac, 0xF0);
                r -= 16;
            }
            let nbits = coef.unsigned_abs().bit_width();
            self.emit_symbol(ac, (r << 4) + usize::try_from(nbits).unwrap_or_default());
            self.emit_value(coef, nbits);
            r = 0;
        }
        if r > 0 {
            self.emit_symbol(ac, 0);
        }
    }
}

/// The APP1 segment libvips writes from the image's default metadata: orientation 1,
/// 25.4 dpi, centered chroma, and the pixel dimensions.
fn exif_segment(width: usize, height: usize) -> Vec<u8> {
    let mut tiff: Vec<u8> = b"Exif\0\0II*\0\x08\0\0\0".to_vec();
    let entry = |out: &mut Vec<u8>, tag: u16, kind: u16, count: u32, value: u32| {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    };
    tiff.extend_from_slice(&6u16.to_le_bytes());
    entry(&mut tiff, 0x0112, 3, 1, 1);
    entry(&mut tiff, 0x011A, 5, 1, 0x56);
    entry(&mut tiff, 0x011B, 5, 1, 0x5E);
    entry(&mut tiff, 0x0128, 3, 1, 2);
    entry(&mut tiff, 0x0213, 3, 1, 1);
    entry(&mut tiff, 0x8769, 4, 1, 0x66);
    tiff.extend_from_slice(&0u32.to_le_bytes());
    for _ in 0..2 {
        tiff.extend_from_slice(&25_400u32.to_le_bytes());
        tiff.extend_from_slice(&1000u32.to_le_bytes());
    }
    tiff.extend_from_slice(&6u16.to_le_bytes());
    entry(&mut tiff, 0x9000, 7, 4, u32::from_le_bytes(*b"0210"));
    entry(&mut tiff, 0x9101, 7, 4, u32::from_le_bytes([1, 2, 3, 0]));
    entry(&mut tiff, 0xA000, 7, 4, u32::from_le_bytes(*b"0100"));
    entry(&mut tiff, 0xA001, 3, 1, 0xFFFF);
    entry(
        &mut tiff,
        0xA002,
        4,
        1,
        u32::try_from(width).unwrap_or(u32::MAX),
    );
    entry(
        &mut tiff,
        0xA003,
        4,
        1,
        u32::try_from(height).unwrap_or(u32::MAX),
    );
    tiff.extend_from_slice(&0u32.to_le_bytes());

    let mut out = vec![0xFF, 0xE1];
    out.extend_from_slice(
        &u16::try_from(tiff.len() + 2)
            .unwrap_or(u16::MAX)
            .to_be_bytes(),
    );
    out.extend_from_slice(&tiff);
    out
}

/// `jpeg_set_quality(quality, force_baseline = TRUE)`.
fn quant_table(basic: &[u32; 64], quality: u32) -> [u32; 64] {
    let quality = quality.clamp(1, 100);
    let scale = if quality < 50 {
        5000 / quality
    } else {
        200 - quality * 2
    };
    basic.map(|v| ((v * scale + 50) / 100).clamp(1, 255))
}

/// `rgb_ycc_convert` (`jccolor.c`), with its fixed-point tables, of pixels `bands` bytes
/// apart.
fn rgb_to_ycc(
    pixels: &[u8],
    bands: usize,
    width: usize,
    height: usize,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    const SCALEBITS: i64 = 16;
    const ONE_HALF: i64 = 1 << (SCALEBITS - 1);
    const CBCR_OFFSET: i64 = 128 << SCALEBITS;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "libjpeg's FIX() macro"
    )]
    const fn fix(x: f64) -> i64 {
        (x * (1i64 << SCALEBITS) as f64 + 0.5) as i64
    }

    let n = width * height;
    let (mut y, mut cb, mut cr) = (vec![0; n], vec![0; n], vec![0; n]);
    for (i, px) in pixels.chunks_exact(bands).take(n).enumerate() {
        let (r, g, b) = (i64::from(px[0]), i64::from(px[1]), i64::from(px[2]));
        let luma = fix(0.299) * r + fix(0.587) * g + fix(0.114) * b + ONE_HALF;
        let blue =
            -fix(0.168_74) * r - fix(0.331_26) * g + fix(0.5) * b + CBCR_OFFSET + ONE_HALF - 1;
        let red = fix(0.5) * r - fix(0.418_69) * g - fix(0.081_31) * b + CBCR_OFFSET + ONE_HALF - 1;
        y[i] = u8::try_from(luma >> SCALEBITS).unwrap_or(255);
        cb[i] = u8::try_from(blue >> SCALEBITS).unwrap_or(255);
        cr[i] = u8::try_from(red >> SCALEBITS).unwrap_or(255);
    }
    (y, cb, cr)
}

/// The full-size luma plane: rows padded with the last row (`expand_bottom_edge`),
/// blocks at the right and bottom of the last MCUs as `compress_first_pass` fills them.
fn luma_component(
    plane: &[u8],
    width: usize,
    height: usize,
    qtable: &[u32; 64],
    mcus_across: usize,
    mcus_down: usize,
) -> Component {
    let width_in_blocks = width.div_ceil(8);
    let height_in_blocks = height.div_ceil(8);
    let sample = |x: usize, y: usize| plane[y.min(height - 1) * width + x.min(width - 1)];
    blocks_of(
        1,
        2,
        qtable,
        0,
        (width_in_blocks, height_in_blocks),
        (mcus_across * 2, mcus_down * 2),
        sample,
    )
}

/// A chroma plane downsampled 2×2 (`h2v2_downsample`, with its alternating 1/2 bias),
/// from rows padded to an even count and columns padded to whole blocks; the output is
/// padded to whole blocks with its last row.
fn chroma_component(
    id: u8,
    plane: &[u8],
    width: usize,
    height: usize,
    qtable: &[u32; 64],
) -> Component {
    let width_in_blocks = width.div_ceil(16);
    let height_in_blocks = height.div_ceil(16);
    let out_cols = width_in_blocks * 8;
    let out_rows = height.div_ceil(2);
    let sample =
        |x: usize, y: usize| u32::from(plane[y.min(height - 1) * width + x.min(width - 1)]);
    let mut down = vec![0u8; out_cols * out_rows];
    for oy in 0..out_rows {
        for ox in 0..out_cols {
            let bias = if ox % 2 == 0 { 1 } else { 2 };
            let (x, y) = (ox * 2, oy * 2);
            let sum = sample(x, y) + sample(x + 1, y) + sample(x, y + 1) + sample(x + 1, y + 1);
            down[oy * out_cols + ox] = u8::try_from((sum + bias) >> 2).unwrap_or(255);
        }
    }
    blocks_of(
        id,
        1,
        qtable,
        1,
        (width_in_blocks, height_in_blocks),
        (width_in_blocks, height_in_blocks),
        |x, y| down[y.min(out_rows - 1) * out_cols + x.min(out_cols - 1)],
    )
}

fn blocks_of(
    id: u8,
    samp: usize,
    qtable: &[u32; 64],
    table: usize,
    (width_in_blocks, height_in_blocks): (usize, usize),
    (stride, rows): (usize, usize),
    sample: impl Fn(usize, usize) -> u8,
) -> Component {
    let mut blocks = vec![[0i32; 64]; stride * rows];
    for by in 0..height_in_blocks {
        for bx in 0..width_in_blocks {
            let mut data = [0i32; 64];
            for (i, value) in data.iter_mut().enumerate() {
                *value = i32::from(sample(bx * 8 + i % 8, by * 8 + i / 8)) - 128;
            }
            fdct_islow(&mut data);
            blocks[by * stride + bx] = quantize(&data, qtable);
        }
        // Dummy blocks at the right edge: zero, with the last block's DC.
        let dc = blocks[by * stride + width_in_blocks - 1][0];
        for bx in width_in_blocks..stride {
            blocks[by * stride + bx][0] = dc;
        }
    }
    // Dummy block rows at the bottom: the DC of the MCU's last block above.
    for by in height_in_blocks..rows {
        for mcu in 0..stride / samp {
            let dc = blocks[(by - 1) * stride + mcu * samp + samp - 1][0];
            for bi in 0..samp {
                blocks[by * stride + mcu * samp + bi] = [0; 64];
                blocks[by * stride + mcu * samp + bi][0] = dc;
            }
        }
    }
    Component {
        id,
        h: samp,
        v: samp,
        table,
        width_in_blocks,
        height_in_blocks,
        stride,
        blocks,
    }
}

/// `jpeg_fdct_islow` (`jfdctint.c`): output scaled up by 8.
fn fdct_islow(data: &mut [i32; 64]) {
    const CONST_BITS: i32 = 13;
    const PASS1_BITS: i32 = 2;
    const F_0_298: i64 = 2446;
    const F_0_390: i64 = 3196;
    const F_0_541: i64 = 4433;
    const F_0_765: i64 = 6270;
    const F_0_899: i64 = 7373;
    const F_1_175: i64 = 9633;
    const F_1_501: i64 = 12299;
    const F_1_847: i64 = 15137;
    const F_1_961: i64 = 16069;
    const F_2_053: i64 = 16819;
    const F_2_562: i64 = 20995;
    const F_3_072: i64 = 25172;
    #[allow(clippy::cast_possible_truncation, reason = "DCT values fit in 32 bits")]
    fn descale(x: i64, n: i32) -> i32 {
        ((x + (1 << (n - 1))) >> n) as i32
    }

    let pass = |d: &mut [i32; 64], index: &dyn Fn(usize) -> usize, first: bool| {
        let (even_shift, odd_shift) = if first {
            (0, CONST_BITS - PASS1_BITS)
        } else {
            (PASS1_BITS, CONST_BITS + PASS1_BITS)
        };
        let v = |k: usize| i64::from(d[index(k)]);
        let (tmp0, tmp7) = (v(0) + v(7), v(0) - v(7));
        let (tmp1, tmp6) = (v(1) + v(6), v(1) - v(6));
        let (tmp2, tmp5) = (v(2) + v(5), v(2) - v(5));
        let (tmp3, tmp4) = (v(3) + v(4), v(3) - v(4));
        let (tmp10, tmp13) = (tmp0 + tmp3, tmp0 - tmp3);
        let (tmp11, tmp12) = (tmp1 + tmp2, tmp1 - tmp2);

        let mut out = [0i32; 8];
        if first {
            #[allow(clippy::cast_possible_truncation, reason = "DCT values fit in 32 bits")]
            {
                out[0] = ((tmp10 + tmp11) << PASS1_BITS) as i32;
                out[4] = ((tmp10 - tmp11) << PASS1_BITS) as i32;
            }
        } else {
            out[0] = descale(tmp10 + tmp11, even_shift);
            out[4] = descale(tmp10 - tmp11, even_shift);
        }
        let z1 = (tmp12 + tmp13) * F_0_541;
        out[2] = descale(z1 + tmp13 * F_0_765, odd_shift);
        out[6] = descale(z1 - tmp12 * F_1_847, odd_shift);

        let z1 = tmp4 + tmp7;
        let z2 = tmp5 + tmp6;
        let z3 = tmp4 + tmp6;
        let z4 = tmp5 + tmp7;
        let z5 = (z3 + z4) * F_1_175;
        let (tmp4, tmp5, tmp6, tmp7) = (
            tmp4 * F_0_298,
            tmp5 * F_2_053,
            tmp6 * F_3_072,
            tmp7 * F_1_501,
        );
        let (z1, z2) = (-z1 * F_0_899, -z2 * F_2_562);
        let (z3, z4) = (-z3 * F_1_961 + z5, -z4 * F_0_390 + z5);
        out[7] = descale(tmp4 + z1 + z3, odd_shift);
        out[5] = descale(tmp5 + z2 + z4, odd_shift);
        out[3] = descale(tmp6 + z2 + z3, odd_shift);
        out[1] = descale(tmp7 + z1 + z4, odd_shift);
        for (k, value) in out.into_iter().enumerate() {
            d[index(k)] = value;
        }
    };
    for row in 0..8 {
        pass(data, &|k| row * 8 + k, true);
    }
    for col in 0..8 {
        pass(data, &|k| k * 8 + col, false);
    }
}

/// `quantize` (`jcdctmgr.c`): rounds half away from zero by the table value times 8.
fn quantize(data: &[i32; 64], qtable: &[u32; 64]) -> [i32; 64] {
    let mut out = [0i32; 64];
    for i in 0..64 {
        let q = i32::try_from(qtable[i] << 3).unwrap_or(i32::MAX);
        let value = data[i];
        out[i] = if value < 0 {
            -((-value + (q >> 1)) / q)
        } else {
            (value + (q >> 1)) / q
        };
    }
    out
}

/// A Huffman table as `jpeg_gen_optimal_table` builds it.
#[derive(Clone)]
struct HuffTable {
    bits: [u8; 17],
    values: Vec<u8>,
}

impl HuffTable {
    /// `jpeg_gen_optimal_table` (`jchuff.c`).
    fn optimal(counts: &[u64; 257]) -> Self {
        const MAX_CLEN: usize = 32;
        let mut freq = *counts;
        let mut bits = [0u8; MAX_CLEN + 1];
        let mut codesize = [0usize; 257];
        let mut others = [-1i32; 257];
        freq[256] = 1;
        loop {
            let mut c1 = None;
            let mut v = u64::MAX;
            for (i, &f) in freq.iter().enumerate() {
                if f != 0 && f <= v {
                    v = f;
                    c1 = Some(i);
                }
            }
            let mut c2 = None;
            v = u64::MAX;
            for (i, &f) in freq.iter().enumerate() {
                if f != 0 && f <= v && Some(i) != c1 {
                    v = f;
                    c2 = Some(i);
                }
            }
            let (Some(mut c1), Some(mut c2)) = (c1, c2) else {
                break;
            };
            freq[c1] += freq[c2];
            freq[c2] = 0;
            codesize[c1] += 1;
            while let Ok(next) = usize::try_from(others[c1]) {
                c1 = next;
                codesize[c1] += 1;
            }
            others[c1] = i32::try_from(c2).unwrap_or(-1);
            codesize[c2] += 1;
            while let Ok(next) = usize::try_from(others[c2]) {
                c2 = next;
                codesize[c2] += 1;
            }
        }
        for &size in &codesize {
            if size > 0 {
                bits[size.min(MAX_CLEN)] += 1;
            }
        }
        for i in (17..=MAX_CLEN).rev() {
            while bits[i] > 0 {
                let mut j = i - 2;
                while bits[j] == 0 {
                    j -= 1;
                }
                bits[i] -= 2;
                bits[i - 1] += 1;
                bits[j + 1] += 2;
                bits[j] -= 1;
            }
        }
        let mut i = 16;
        while bits[i] == 0 {
            i -= 1;
        }
        bits[i] -= 1;

        let mut values = Vec::new();
        for size in 1..=MAX_CLEN {
            for (symbol, &s) in codesize.iter().take(256).enumerate() {
                if s == size {
                    values.push(u8::try_from(symbol).unwrap_or_default());
                }
            }
        }
        let mut table_bits = [0u8; 17];
        table_bits.copy_from_slice(&bits[..17]);
        Self {
            bits: table_bits,
            values,
        }
    }

    /// `jpeg_make_c_derived_tbl`: code and length per symbol.
    fn derive(&self) -> Derived {
        let mut code = [0u32; 256];
        let mut size = [0u8; 256];
        let mut next = 0u32;
        let mut p = 0;
        for length in 1..=16u8 {
            for _ in 0..self.bits[usize::from(length)] {
                let symbol = usize::from(self.values[p]);
                code[symbol] = next;
                size[symbol] = length;
                next += 1;
                p += 1;
            }
            next <<= 1;
        }
        Derived { code, size }
    }

    fn write_dht(&self, out: &mut Vec<u8>, index: u8) {
        out.extend_from_slice(&[0xFF, 0xC4]);
        let len = u16::try_from(self.values.len() + 2 + 1 + 16).unwrap_or(u16::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.push(index);
        out.extend_from_slice(&self.bits[1..]);
        out.extend_from_slice(&self.values);
    }
}

#[derive(Clone)]
struct Derived {
    code: [u32; 256],
    size: [u8; 256],
}

fn write_sos(out: &mut Vec<u8>, scan: &Scan, components: &[Component; 3]) {
    out.extend_from_slice(&[0xFF, 0xDA]);
    let count = scan.components.len();
    out.extend_from_slice(
        &u16::try_from(2 * count + 6)
            .unwrap_or_default()
            .to_be_bytes(),
    );
    out.push(u8::try_from(count).unwrap_or_default());
    for &ci in scan.components {
        let table = u8::try_from(components[ci].table).unwrap_or_default();
        let tables = if scan.ss == 0 {
            if scan.ah == 0 { table << 4 } else { 0 }
        } else {
            table
        };
        out.extend_from_slice(&[components[ci].id, tables]);
    }
    out.extend_from_slice(&[
        u8::try_from(scan.ss).unwrap_or_default(),
        u8::try_from(scan.se).unwrap_or_default(),
        u8::try_from((scan.ah << 4) | scan.al).unwrap_or_default(),
    ]);
}

/// `jcphuff.c`: one pass over a scan, gathering symbol counts (without `tables`) or
/// writing the entropy-coded data.
struct Entropy<'s> {
    scan: &'s Scan,
    tables: Option<[Option<Derived>; 2]>,
    counts: [[u64; 257]; 2],
    used: [bool; 2],
    bytes: Vec<u8>,
    put_buffer: u32,
    put_bits: u32,
    last_dc: [i32; 3],
    eobrun: u32,
    /// Correction bits (`bit_buffer`), the first `be` of them pending with `eobrun`.
    bit_buffer: Vec<u8>,
    be: usize,
    /// The table of the scan's (single) AC component.
    ac_table: usize,
}

impl<'s> Entropy<'s> {
    fn new(scan: &'s Scan, tables: Option<[Option<Derived>; 2]>) -> Self {
        Self {
            scan,
            tables,
            counts: [[0; 257]; 2],
            used: [false; 2],
            bytes: Vec::new(),
            put_buffer: 0,
            put_bits: 0,
            last_dc: [0; 3],
            eobrun: 0,
            bit_buffer: Vec::with_capacity(MAX_CORR_BITS),
            be: 0,
            ac_table: 0,
        }
    }

    fn encode(&mut self, components: &[Component; 3], mcus_across: usize, mcus_down: usize) {
        let scan = self.scan;
        if scan.components.len() > 1 {
            for my in 0..mcus_down {
                for mx in 0..mcus_across {
                    for &ci in scan.components {
                        let c = &components[ci];
                        for y in 0..c.v {
                            for x in 0..c.h {
                                let block = &c.blocks[(my * c.v + y) * c.stride + mx * c.h + x];
                                self.encode_dc(ci, c.table, block);
                            }
                        }
                    }
                }
            }
            return;
        }
        let ci = scan.components[0];
        let c = &components[ci];
        self.ac_table = c.table;
        for by in 0..c.height_in_blocks {
            for bx in 0..c.width_in_blocks {
                let block = &c.blocks[by * c.stride + bx];
                match (scan.ss, scan.ah) {
                    (0, _) => self.encode_dc(ci, c.table, block),
                    (_, 0) => self.encode_ac_first(block),
                    _ => self.encode_ac_refine(block),
                }
            }
        }
    }

    fn emit_bits(&mut self, code: u32, size: u32) {
        if self.tables.is_none() || size == 0 {
            return;
        }
        let mut buffer = code & ((1 << size) - 1);
        self.put_bits += size;
        buffer <<= 24 - self.put_bits;
        buffer |= self.put_buffer;
        while self.put_bits >= 8 {
            let c = u8::try_from((buffer >> 16) & 0xFF).unwrap_or_default();
            self.bytes.push(c);
            if c == 0xFF {
                self.bytes.push(0);
            }
            buffer <<= 8;
            self.put_bits -= 8;
        }
        self.put_buffer = buffer & 0x00FF_FFFF;
    }

    fn emit_symbol(&mut self, table: usize, symbol: usize) {
        match &self.tables {
            None => {
                self.counts[table][symbol] += 1;
                self.used[table] = true;
            }
            Some(tables) => {
                let derived = tables[table]
                    .as_ref()
                    .expect("table gathered for this scan");
                let (code, size) = (derived.code[symbol], u32::from(derived.size[symbol]));
                self.emit_bits(code, size);
            }
        }
    }

    fn emit_buffered_bits(&mut self, start: usize, count: usize) {
        for i in start..start + count {
            let bit = u32::from(self.bit_buffer[i]);
            self.emit_bits(bit, 1);
        }
    }

    fn emit_eobrun(&mut self) {
        if self.eobrun == 0 {
            return;
        }
        let nbits = u32::BITS - 1 - self.eobrun.leading_zeros();
        self.emit_symbol(
            self.ac_table,
            usize::try_from(nbits << 4).unwrap_or_default(),
        );
        self.emit_bits(self.eobrun, nbits);
        self.eobrun = 0;
        self.emit_buffered_bits(0, self.be);
        self.be = 0;
    }

    fn encode_dc(&mut self, ci: usize, table: usize, block: &[i32; 64]) {
        if self.scan.ah != 0 {
            #[allow(
                clippy::cast_sign_loss,
                reason = "the low bit of a two's complement value"
            )]
            self.emit_bits((block[0] >> self.scan.al) as u32, 1);
            return;
        }
        let value = block[0] >> self.scan.al;
        let diff = value - self.last_dc[ci];
        self.last_dc[ci] = value;
        let nbits = diff.unsigned_abs().bit_width();
        self.emit_symbol(table, usize::try_from(nbits).unwrap_or_default());
        #[allow(clippy::cast_sign_loss, reason = "libjpeg emits the low bits of temp2")]
        let bits = if diff < 0 {
            (diff - 1) as u32
        } else {
            diff as u32
        };
        self.emit_bits(bits, nbits);
    }

    fn encode_ac_first(&mut self, block: &[i32; 64]) {
        let (al, table) = (self.scan.al, self.ac_table);
        let mut r = 0;
        for k in self.scan.ss..=self.scan.se {
            let coef = block[NATURAL_ORDER[k]];
            let magnitude = coef.unsigned_abs() >> al;
            if magnitude == 0 {
                r += 1;
                continue;
            }
            let bits = if coef < 0 { !magnitude } else { magnitude };
            self.emit_eobrun();
            while r > 15 {
                self.emit_symbol(table, 0xF0);
                r -= 16;
            }
            let nbits = magnitude.bit_width();
            self.emit_symbol(table, (r << 4) + usize::try_from(nbits).unwrap_or_default());
            self.emit_bits(bits, nbits);
            r = 0;
        }
        if r > 0 {
            self.eobrun += 1;
            if self.eobrun == 0x7FFF {
                self.emit_eobrun();
            }
        }
    }

    fn encode_ac_refine(&mut self, block: &[i32; 64]) {
        let (al, table) = (self.scan.al, self.ac_table);
        let (ss, se) = (self.scan.ss, self.scan.se);
        let mut absvalues = [0u32; 64];
        let mut eob = 0;
        for k in ss..=se {
            let magnitude = block[NATURAL_ORDER[k]].unsigned_abs() >> al;
            absvalues[k] = magnitude;
            if magnitude == 1 {
                eob = k;
            }
        }

        let mut r = 0;
        let mut br = 0;
        let mut br_start = self.be;
        self.bit_buffer.truncate(self.be);
        for k in ss..=se {
            let magnitude = absvalues[k];
            if magnitude == 0 {
                r += 1;
                continue;
            }
            while r > 15 && k <= eob {
                self.emit_eobrun();
                self.emit_symbol(table, 0xF0);
                r -= 16;
                self.emit_buffered_bits(br_start, br);
                self.bit_buffer.clear();
                br_start = 0;
                br = 0;
            }
            if magnitude > 1 {
                self.bit_buffer
                    .push(u8::try_from(magnitude & 1).unwrap_or_default());
                br += 1;
                continue;
            }
            self.emit_eobrun();
            self.emit_symbol(table, (r << 4) + 1);
            self.emit_bits(u32::from(block[NATURAL_ORDER[k]] >= 0), 1);
            self.emit_buffered_bits(br_start, br);
            self.bit_buffer.clear();
            br_start = 0;
            br = 0;
            r = 0;
        }
        if r > 0 || br > 0 {
            self.eobrun += 1;
            self.be += br;
            if self.eobrun == 0x7FFF || self.be > MAX_CORR_BITS - 64 + 1 {
                self.emit_eobrun();
                self.bit_buffer.clear();
            }
        }
    }

    /// `finish_pass_phuff`: the pending EOB run, then the last byte padded with ones.
    fn finish(&mut self) {
        self.emit_eobrun();
        self.emit_bits(0x7F, 7);
        self.put_buffer = 0;
        self.put_bits = 0;
    }

    /// `finish_pass_gather_phuff`: a table for each table number the scan used.
    fn optimal_tables(mut self) -> [Option<HuffTable>; 2] {
        self.emit_eobrun();
        let dc_refine = self.scan.ss == 0 && self.scan.ah != 0;
        [0, 1].map(|t| (self.used[t] && !dc_refine).then(|| HuffTable::optimal(&self.counts[t])))
    }
}
