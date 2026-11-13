use super::{Image, MOST_PIXELS, failure};
use crate::ContentError;

pub(crate) const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29,
    22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

pub(crate) fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8])
}

#[derive(Clone, Default)]
struct Huffman {
    lookup: Vec<(u16, u8, u8)>,
}

impl Huffman {
    fn new(counts: &[u8; 16], symbols: &[u8]) -> Result<Self, ContentError> {
        let mut lookup = Vec::with_capacity(symbols.len());
        let mut code: u32 = 0;
        let mut at = 0;
        for (length, &count) in counts.iter().enumerate() {
            for _ in 0..count {
                let symbol = *symbols.get(at).ok_or_else(|| failure("a JPEG Huffman table is cut short"))?;
                if code >= 1 << (length + 1) {
                    return Err(failure("a JPEG Huffman table is not valid"));
                }
                lookup.push((code as u16, length as u8 + 1, symbol));
                code += 1;
                at += 1;
            }
            code <<= 1;
        }
        Ok(Self { lookup })
    }
}

struct Bits<'b> {
    data: &'b [u8],
    at: usize,
    held: u32,
    count: u8,
}

impl Bits<'_> {
    fn bit(&mut self) -> Result<u32, ContentError> {
        if self.count == 0 {
            let byte = *self.data.get(self.at).ok_or_else(|| failure("the JPEG's scan is cut short"))?;
            self.at += 1;
            if byte == 0xFF {
                match self.data.get(self.at) {
                    Some(0x00) => self.at += 1,
                    _ => return Err(failure("the JPEG's scan ends early")),
                }
            }
            self.held = u32::from(byte);
            self.count = 8;
        }
        self.count -= 1;
        Ok((self.held >> self.count) & 1)
    }

    fn bits(&mut self, count: u8) -> Result<u32, ContentError> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Ok(value)
    }

    fn decode(&mut self, table: &Huffman) -> Result<u8, ContentError> {
        let mut code: u32 = 0;
        for length in 1..=16u8 {
            code = (code << 1) | self.bit()?;
            if let Some((_, _, symbol)) = table.lookup.iter().find(|(found, size, _)| *size == length && u32::from(*found) == code) {
                return Ok(*symbol);
            }
        }
        Err(failure("the JPEG's scan holds a code no table has"))
    }

    fn restart(&mut self) -> Result<(), ContentError> {
        self.count = 0;
        while self.at + 1 < self.data.len() && !(self.data[self.at] == 0xFF && (0xD0..=0xD7).contains(&self.data[self.at + 1])) {
            self.at += 1;
        }
        if self.at + 1 >= self.data.len() {
            return Err(failure("the JPEG's restart marker is missing"));
        }
        self.at += 2;
        Ok(())
    }
}

fn extend(value: u32, size: u8) -> i32 {
    if size == 0 {
        return 0;
    }
    let value = value as i32;
    if value < 1 << (size - 1) { value - (1 << size) + 1 } else { value }
}

#[derive(Clone)]
struct Component {
    id: u8,
    horizontal: usize,
    vertical: usize,
    table: usize,
    dc: usize,
    ac: usize,
    previous: i32,
    blocks_wide: usize,
    blocks_high: usize,
    samples: Vec<u8>,
}

pub(crate) type Cosines = [[f32; 8]; 8];

/// `C(u) cos((2x + 1) u π / 16)` for every `x` and `u`, the DCT's basis (T.81 A.3.3).
pub(crate) fn cosines() -> Cosines {
    let mut table = [[0f32; 8]; 8];
    for (x, row) in table.iter_mut().enumerate() {
        for (u, value) in row.iter_mut().enumerate() {
            let scale = if u == 0 { std::f32::consts::FRAC_1_SQRT_2 } else { 1.0 };
            *value = scale * (((2 * x + 1) as f32 * u as f32 * std::f32::consts::PI) / 16.0).cos();
        }
    }
    table
}

fn idct(coefficients: &[i32; 64], out: &mut [u8; 64], cosines: &Cosines) {
    let mut rows = [0f32; 64];
    for y in 0..8 {
        for x in 0..8 {
            rows[y * 8 + x] = (0..8).map(|u| cosines[x][u] * coefficients[y * 8 + u] as f32).sum::<f32>() / 2.0;
        }
    }
    for x in 0..8 {
        for y in 0..8 {
            let value = (0..8).map(|v| cosines[y][v] * rows[v * 8 + x]).sum::<f32>() / 2.0;
            out[y * 8 + x] = (value + 128.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// The Exif orientation (1 to 8) an APP1 segment gives, if any.
fn orientation(app1: &[u8]) -> Option<u8> {
    let tiff = app1.strip_prefix(b"Exif\0\0")?;
    let little = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| tiff.get(at..at + 2).map(|bytes| if little { u16::from_le_bytes([bytes[0], bytes[1]]) } else { u16::from_be_bytes([bytes[0], bytes[1]]) });
    let u32_at = |at: usize| {
        tiff.get(at..at + 4).map(|bytes| if little { u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) } else { u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) })
    };
    let directory = u32_at(4)? as usize;
    let entries = usize::from(u16_at(directory)?);
    (0..entries.min(256)).find_map(|entry| {
        let at = directory + 2 + entry * 12;
        (u16_at(at)? == 0x0112).then(|| u16_at(at + 8)).flatten().and_then(|value| u8::try_from(value).ok()).filter(|value| (1..=8).contains(value))
    })
}

/// A baseline or extended-sequential JPEG (ITU T.81 with Huffman coding), grayscale or YCbCr,
/// any sampling factors and restart intervals, with the Exif orientation it declares.
pub(crate) fn decode(bytes: &[u8]) -> Result<(Image, u8), ContentError> {
    let mut at = 2;
    let mut quantization = [[0u16; 64]; 4];
    let mut dc_tables: Vec<Huffman> = vec![Huffman::default(); 4];
    let mut ac_tables: Vec<Huffman> = vec![Huffman::default(); 4];
    let mut components: Vec<Component> = Vec::new();
    let (mut width, mut height) = (0usize, 0usize);
    let mut restart_interval = 0usize;
    let mut turned = 1;
    loop {
        while bytes.get(at) == Some(&0xFF) && bytes.get(at + 1) == Some(&0xFF) {
            at += 1;
        }
        let marker = match bytes.get(at..at + 2) {
            Some([0xFF, marker]) => *marker,
            _ => return Err(failure("the JPEG has no image")),
        };
        at += 2;
        if marker == 0xD9 {
            return Err(failure("the JPEG ends before its image"));
        }
        if (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            continue;
        }
        let length = usize::from(u16::from_be_bytes([*bytes.get(at).ok_or_else(|| failure("a JPEG segment is cut short"))?, *bytes.get(at + 1).ok_or_else(|| failure("a JPEG segment is cut short"))?]));
        let segment = bytes.get(at + 2..at + length).filter(|_| length >= 2).ok_or_else(|| failure("a JPEG segment is cut short"))?;
        at += length;
        match marker {
            0xE1 => turned = orientation(segment).unwrap_or(turned),
            0xDB => {
                let mut rest = segment;
                while let Some((&head, tail)) = rest.split_first() {
                    let (precision, index) = (head >> 4, usize::from(head & 0x0F));
                    let size = if precision == 0 { 64 } else { 128 };
                    let table = tail.get(..size).ok_or_else(|| failure("a JPEG quantization table is cut short"))?;
                    let slot = quantization.get_mut(index).ok_or_else(|| failure("a JPEG quantization table has a wrong index"))?;
                    for (position, value) in slot.iter_mut().enumerate() {
                        *value = if precision == 0 { u16::from(table[position]) } else { u16::from_be_bytes([table[position * 2], table[position * 2 + 1]]) };
                    }
                    rest = &tail[size..];
                }
            }
            0xC4 => {
                let mut rest = segment;
                while rest.len() >= 17 {
                    let (class, index) = (rest[0] >> 4, usize::from(rest[0] & 0x0F));
                    let mut counts = [0u8; 16];
                    counts.copy_from_slice(&rest[1..17]);
                    let total: usize = counts.iter().map(|count| usize::from(*count)).sum();
                    let symbols = rest.get(17..17 + total).ok_or_else(|| failure("a JPEG Huffman table is cut short"))?;
                    let table = Huffman::new(&counts, symbols)?;
                    let slot = if class == 0 { dc_tables.get_mut(index) } else { ac_tables.get_mut(index) };
                    *slot.ok_or_else(|| failure("a JPEG Huffman table has a wrong index"))? = table;
                    rest = &rest[17 + total..];
                }
            }
            0xDD if segment.len() >= 2 => restart_interval = usize::from(u16::from_be_bytes([segment[0], segment[1]])),
            0xC0 | 0xC1 => {
                if segment.len() < 6 || segment[0] != 8 {
                    return Err(failure("only 8-bit JPEGs are read"));
                }
                height = usize::from(u16::from_be_bytes([segment[1], segment[2]]));
                width = usize::from(u16::from_be_bytes([segment[3], segment[4]]));
                let count = usize::from(segment[5]);
                if width == 0 || height == 0 || width * height > MOST_PIXELS || !matches!(count, 1 | 3) {
                    return Err(failure("the JPEG is empty, too large, or neither grayscale nor YCbCr"));
                }
                for index in 0..count {
                    let part = segment.get(6 + index * 3..9 + index * 3).ok_or_else(|| failure("the JPEG's frame header is cut short"))?;
                    let (horizontal, vertical) = (usize::from(part[1] >> 4), usize::from(part[1] & 0x0F));
                    if !(1..=4).contains(&horizontal) || !(1..=4).contains(&vertical) || part[2] > 3 {
                        return Err(failure("the JPEG's sampling factors are not valid"));
                    }
                    components.push(Component { id: part[0], horizontal, vertical, table: usize::from(part[2]), dc: 0, ac: 0, previous: 0, blocks_wide: 0, blocks_high: 0, samples: Vec::new() });
                }
            }
            0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return Err(failure("progressive, lossless and arithmetic-coded JPEGs are not read yet")),
            0xDA => {
                if components.is_empty() {
                    return Err(failure("the JPEG's scan comes before its frame"));
                }
                let count = usize::from(*segment.first().ok_or_else(|| failure("the JPEG's scan header is cut short"))?);
                if count != components.len() {
                    return Err(failure("JPEGs with several scans are not read yet"));
                }
                for index in 0..count {
                    let part = segment.get(1 + index * 2..3 + index * 2).ok_or_else(|| failure("the JPEG's scan header is cut short"))?;
                    let component = components.iter_mut().find(|component| component.id == part[0]).ok_or_else(|| failure("the JPEG's scan names an unknown component"))?;
                    component.dc = usize::from(part[1] >> 4).min(3);
                    component.ac = usize::from(part[1] & 0x0F).min(3);
                }
                let image = decode_scan(&bytes[at..], width, height, &mut components, &quantization, &dc_tables, &ac_tables, restart_interval)?;
                return Ok((image, turned));
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_scan(
    data: &[u8],
    width: usize,
    height: usize,
    components: &mut [Component],
    quantization: &[[u16; 64]; 4],
    dc_tables: &[Huffman],
    ac_tables: &[Huffman],
    restart_interval: usize,
) -> Result<Image, ContentError> {
    let most_horizontal = components.iter().map(|component| component.horizontal).max().unwrap_or(1);
    let most_vertical = components.iter().map(|component| component.vertical).max().unwrap_or(1);
    let mcus_wide = width.div_ceil(8 * most_horizontal);
    let mcus_high = height.div_ceil(8 * most_vertical);
    for component in components.iter_mut() {
        component.blocks_wide = mcus_wide * component.horizontal;
        component.blocks_high = mcus_high * component.vertical;
        component.samples = vec![0u8; component.blocks_wide * component.blocks_high * 64];
    }
    let mut bits = Bits { data, at: 0, held: 0, count: 0 };
    let mut coefficients = [0i32; 64];
    let mut block = [0u8; 64];
    let basis = cosines();
    for mcu in 0..mcus_wide * mcus_high {
        if restart_interval > 0 && mcu > 0 && mcu % restart_interval == 0 {
            bits.restart()?;
            for component in components.iter_mut() {
                component.previous = 0;
            }
        }
        let (mcu_x, mcu_y) = (mcu % mcus_wide, mcu / mcus_wide);
        for component in components.iter_mut() {
            for v in 0..component.vertical {
                for h in 0..component.horizontal {
                    coefficients.fill(0);
                    let size = bits.decode(&dc_tables[component.dc])?;
                    if size > 11 {
                        return Err(failure("a JPEG DC difference is too large"));
                    }
                    let difference = extend(bits.bits(size)?, size);
                    component.previous = component.previous.saturating_add(difference);
                    let table = &quantization[component.table];
                    coefficients[0] = component.previous.saturating_mul(i32::from(table[0]));
                    let mut position = 1;
                    while position < 64 {
                        let symbol = bits.decode(&ac_tables[component.ac])?;
                        let (run, size) = (usize::from(symbol >> 4), symbol & 0x0F);
                        if size == 0 {
                            if run == 15 {
                                position += 16;
                                continue;
                            }
                            break;
                        }
                        position += run;
                        if position > 63 || size > 10 {
                            return Err(failure("a JPEG block runs past its 64 coefficients"));
                        }
                        coefficients[ZIGZAG[position]] = extend(bits.bits(size)?, size).saturating_mul(i32::from(table[position]));
                        position += 1;
                    }
                    idct(&coefficients, &mut block, &basis);
                    let (block_x, block_y) = (mcu_x * component.horizontal + h, mcu_y * component.vertical + v);
                    let stride = component.blocks_wide * 8;
                    for y in 0..8 {
                        let start = (block_y * 8 + y) * stride + block_x * 8;
                        component.samples[start..start + 8].copy_from_slice(&block[y * 8..y * 8 + 8]);
                    }
                }
            }
        }
    }
    let mut image = Image { width, height, rgba: vec![255u8; width * height * 4] };
    for y in 0..height {
        for x in 0..width {
            let value = |component: &Component| {
                let (sx, sy) = (x * component.horizontal / most_horizontal, y * component.vertical / most_vertical);
                f32::from(component.samples[sy * component.blocks_wide * 8 + sx])
            };
            let at = (y * width + x) * 4;
            if components.len() == 1 {
                let gray = value(&components[0]) as u8;
                image.rgba[at..at + 3].copy_from_slice(&[gray, gray, gray]);
            } else {
                let (luma, blue, red) = (value(&components[0]), value(&components[1]) - 128.0, value(&components[2]) - 128.0);
                let channel = |level: f32| level.round().clamp(0.0, 255.0) as u8;
                image.rgba[at] = channel(luma + 1.402 * red);
                image.rgba[at + 1] = channel(luma - 0.344_136 * blue - 0.714_136 * red);
                image.rgba[at + 2] = channel(luma + 1.772 * blue);
            }
        }
    }
    Ok(image)
}
