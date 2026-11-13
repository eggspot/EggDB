use super::Image;
use super::jpeg_decode::{Cosines, ZIGZAG, cosines};

const LUMINANCE: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109,
    103, 77, 24, 35, 55, 64, 81, 104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const CHROMINANCE: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99, 47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];
const DC_LUMINANCE_COUNTS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_LUMINANCE_SYMBOLS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const DC_CHROMINANCE_COUNTS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_CHROMINANCE_SYMBOLS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMINANCE_COUNTS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMINANCE_SYMBOLS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42,
    0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35,
    0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67,
    0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98,
    0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7,
    0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4,
    0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];
const AC_CHROMINANCE_COUNTS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMINANCE_SYMBOLS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1,
    0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a,
    0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66,
    0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96,
    0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4,
    0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];

struct Codes {
    by_symbol: [(u16, u8); 256],
}

fn codes(counts: &[u8; 16], symbols: &[u8]) -> Codes {
    let mut by_symbol = [(0u16, 0u8); 256];
    let mut code: u16 = 0;
    let mut at = 0;
    for (length, &count) in counts.iter().enumerate() {
        for _ in 0..count {
            by_symbol[usize::from(symbols[at])] = (code, length as u8 + 1);
            code += 1;
            at += 1;
        }
        code <<= 1;
    }
    Codes { by_symbol }
}

struct Writer {
    out: Vec<u8>,
    held: u32,
    count: u8,
}

impl Writer {
    fn put(&mut self, value: u32, size: u8) {
        for bit in (0..size).rev() {
            self.held = (self.held << 1) | ((value >> bit) & 1);
            self.count += 1;
            if self.count == 8 {
                let byte = self.held as u8;
                self.out.push(byte);
                if byte == 0xFF {
                    self.out.push(0x00);
                }
                self.held = 0;
                self.count = 0;
            }
        }
    }

    fn code(&mut self, codes: &Codes, symbol: u8) {
        let (code, size) = codes.by_symbol[usize::from(symbol)];
        self.put(u32::from(code), size);
    }

    fn finish(&mut self) {
        if self.count > 0 {
            let missing = 8 - self.count;
            self.put((1 << missing) - 1, missing);
        }
    }
}

fn scaled(table: &[u8; 64], quality: u8) -> [u8; 64] {
    let quality = u32::from(quality.clamp(1, 100));
    let factor = if quality < 50 { 5000 / quality } else { 200 - quality * 2 };
    let mut out = [0u8; 64];
    for (slot, &value) in out.iter_mut().zip(table) {
        *slot = ((u32::from(value) * factor + 50) / 100).clamp(1, 255) as u8;
    }
    out
}

fn magnitude(value: i32) -> (u8, u32) {
    let size = (32 - value.unsigned_abs().leading_zeros()) as u8;
    let bits = if value < 0 { (value - 1) as u32 & ((1u32 << size) - 1) } else { value as u32 };
    (size, bits)
}

fn fdct(samples: &[f32; 64], cosines: &Cosines) -> [f32; 64] {
    let mut rows = [0f32; 64];
    for y in 0..8 {
        for u in 0..8 {
            rows[y * 8 + u] = (0..8).map(|x| cosines[x][u] * samples[y * 8 + x]).sum::<f32>() / 2.0;
        }
    }
    let mut out = [0f32; 64];
    for u in 0..8 {
        for v in 0..8 {
            out[v * 8 + u] = (0..8).map(|y| cosines[y][v] * rows[y * 8 + u]).sum::<f32>() / 2.0;
        }
    }
    out
}

struct Encoder<'e> {
    writer: Writer,
    cosines: Cosines,
    tables: [&'e [u8; 64]; 2],
    dc: [Codes; 2],
    ac: [Codes; 2],
    previous: [i32; 3],
}

impl Encoder<'_> {
    fn block(&mut self, samples: &[f32; 64], component: usize) {
        let table = if component == 0 { 0 } else { 1 };
        let transformed = fdct(samples, &self.cosines);
        let mut quantized = [0i32; 64];
        for (position, &natural) in ZIGZAG.iter().enumerate() {
            quantized[position] = (transformed[natural] / f32::from(self.tables[table][position])).round() as i32;
        }
        let (size, bits) = magnitude(quantized[0] - self.previous[component]);
        self.previous[component] = quantized[0];
        self.writer.code(&self.dc[table], size);
        self.writer.put(bits, size);
        let mut run = 0;
        for &value in &quantized[1..] {
            if value == 0 {
                run += 1;
                continue;
            }
            while run > 15 {
                self.writer.code(&self.ac[table], 0xF0);
                run -= 16;
            }
            let (size, bits) = magnitude(value.clamp(-1023, 1023));
            self.writer.code(&self.ac[table], ((run as u8) << 4) | size);
            self.writer.put(bits, size);
            run = 0;
        }
        if run > 0 {
            self.writer.code(&self.ac[table], 0x00);
        }
    }
}

fn segment(out: &mut Vec<u8>, marker: u8, data: &[u8]) {
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(data);
}

/// `image` as a baseline JFIF JPEG at `quality` (1 to 100) with 4:2:0 chroma subsampling and
/// the example tables of ITU T.81 Annex K; transparent pixels are laid over white.
pub(crate) fn encode(image: &Image, quality: u8) -> Vec<u8> {
    let luminance = scaled(&LUMINANCE, quality);
    let chrominance = scaled(&CHROMINANCE, quality);
    let natural = |table: &[u8; 64]| {
        let mut zigzagged = [0u8; 64];
        for (position, &at) in ZIGZAG.iter().enumerate() {
            zigzagged[position] = table[at];
        }
        zigzagged
    };
    let (luminance_zigzag, chrominance_zigzag) = (natural(&luminance), natural(&chrominance));
    let mut out = vec![0xFF, 0xD8];
    segment(&mut out, 0xE0, b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00");
    let mut quantization = vec![0u8];
    quantization.extend_from_slice(&luminance_zigzag);
    quantization.push(1);
    quantization.extend_from_slice(&chrominance_zigzag);
    segment(&mut out, 0xDB, &quantization);
    let mut frame = vec![8];
    frame.extend_from_slice(&(image.height as u16).to_be_bytes());
    frame.extend_from_slice(&(image.width as u16).to_be_bytes());
    frame.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    segment(&mut out, 0xC0, &frame);
    let mut huffman = Vec::new();
    for (class_index, counts, symbols) in [
        (0x00u8, &DC_LUMINANCE_COUNTS, &DC_LUMINANCE_SYMBOLS[..]),
        (0x10, &AC_LUMINANCE_COUNTS, &AC_LUMINANCE_SYMBOLS[..]),
        (0x01, &DC_CHROMINANCE_COUNTS, &DC_CHROMINANCE_SYMBOLS[..]),
        (0x11, &AC_CHROMINANCE_COUNTS, &AC_CHROMINANCE_SYMBOLS[..]),
    ] {
        huffman.push(class_index);
        huffman.extend_from_slice(counts);
        huffman.extend_from_slice(symbols);
    }
    segment(&mut out, 0xC4, &huffman);
    segment(&mut out, 0xDA, &[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
    let mut encoder = Encoder {
        writer: Writer { out: Vec::new(), held: 0, count: 0 },
        cosines: cosines(),
        tables: [&luminance_zigzag, &chrominance_zigzag],
        dc: [codes(&DC_LUMINANCE_COUNTS, &DC_LUMINANCE_SYMBOLS), codes(&DC_CHROMINANCE_COUNTS, &DC_CHROMINANCE_SYMBOLS)],
        ac: [codes(&AC_LUMINANCE_COUNTS, &AC_LUMINANCE_SYMBOLS), codes(&AC_CHROMINANCE_COUNTS, &AC_CHROMINANCE_SYMBOLS)],
        previous: [0; 3],
    };
    let ycbcr = |x: usize, y: usize| -> [f32; 3] {
        let (x, y) = (x.min(image.width - 1), y.min(image.height - 1));
        let at = (y * image.width + x) * 4;
        let alpha = f32::from(image.rgba[at + 3]) / 255.0;
        let over_white = |channel: u8| f32::from(channel) * alpha + 255.0 * (1.0 - alpha);
        let (red, green, blue) = (over_white(image.rgba[at]), over_white(image.rgba[at + 1]), over_white(image.rgba[at + 2]));
        [
            0.299 * red + 0.587 * green + 0.114 * blue - 128.0,
            -0.168_736 * red - 0.331_264 * green + 0.5 * blue,
            0.5 * red - 0.418_688 * green - 0.081_312 * blue,
        ]
    };
    for mcu_y in (0..image.height).step_by(16) {
        for mcu_x in (0..image.width).step_by(16) {
            for (block_x, block_y) in [(0, 0), (8, 0), (0, 8), (8, 8)] {
                let mut samples = [0f32; 64];
                for y in 0..8 {
                    for x in 0..8 {
                        samples[y * 8 + x] = ycbcr(mcu_x + block_x + x, mcu_y + block_y + y)[0];
                    }
                }
                encoder.block(&samples, 0);
            }
            for component in 1..3 {
                let mut samples = [0f32; 64];
                for y in 0..8 {
                    for x in 0..8 {
                        let (left, top) = (mcu_x + x * 2, mcu_y + y * 2);
                        samples[y * 8 + x] = (ycbcr(left, top)[component] + ycbcr(left + 1, top)[component] + ycbcr(left, top + 1)[component] + ycbcr(left + 1, top + 1)[component]) / 4.0;
                    }
                }
                encoder.block(&samples, component);
            }
        }
    }
    encoder.writer.finish();
    out.extend_from_slice(&encoder.writer.out);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}
