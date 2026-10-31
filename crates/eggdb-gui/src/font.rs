use crate::canvas::Canvas;
use crate::color::Rgba;
use std::fmt;

const MAX_COMPOSITE_DEPTH: usize = 8;
const MAX_POINTS: usize = 1 << 16;
const CURVE_STEPS: usize = 8;

/// Why a font file could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FontError {
    /// The file is not a TrueType font or collection, or is damaged.
    Malformed(&'static str),
    /// The font keeps its outlines in a format this reader does not draw (CFF).
    Unsupported,
}

impl fmt::Display for FontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(table) => write!(f, "the font's {table} is damaged"),
            Self::Unsupported => f.write_str("the font has PostScript (CFF) outlines, not TrueType ones"),
        }
    }
}

impl std::error::Error for FontError {}

/// A size, line height and colour to draw text with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle {
    /// The font size in pixels.
    pub size: f32,
    /// The height of the line box.
    pub line_height: f32,
    /// The text colour.
    pub color: Rgba,
}

/// What draws text: a TrueType font, or the box typeface tests use.
pub trait Typeface {
    /// How far the pen moves after `character` at `size` pixels.
    fn advance(&self, character: char, size: f32) -> f32;

    /// How far above the baseline glyphs reach, and how far below, at `size` pixels.
    fn extent(&self, size: f32) -> (f32, f32);

    /// Draws `character` with its pen at (`x`, `baseline`).
    fn draw_glyph(&self, canvas: &mut Canvas, character: char, x: f32, baseline: f32, size: f32, color: Rgba);

    /// The width of `text` at `size`.
    fn width(&self, text: &str, size: f32) -> f32 {
        text.chars().map(|character| self.advance(character, size)).sum()
    }

    /// Draws `text` in a line box whose top is `top`, glyphs centred vertically in it,
    /// starting at `x`; returns where the pen ends.
    fn draw_text(&self, canvas: &mut Canvas, text: &str, x: f32, top: f32, style: TextStyle) -> f32 {
        let (ascent, descent) = self.extent(style.size);
        let baseline = (top + (style.line_height - ascent - descent) / 2.0 + ascent).round();
        let mut pen = x;
        for character in text.chars() {
            self.draw_glyph(canvas, character, pen, baseline, style.size, style.color);
            pen += self.advance(character, style.size);
        }
        pen
    }
}

/// A typeface with no font file: every character a box of its advance's width, so tests draw
/// the same pixels on every machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BoxFace;

impl Typeface for BoxFace {
    fn advance(&self, _character: char, size: f32) -> f32 {
        (size * 0.6).round()
    }

    fn extent(&self, size: f32) -> (f32, f32) {
        ((size * 0.8).round(), (size * 0.2).round())
    }

    fn draw_glyph(&self, canvas: &mut Canvas, character: char, x: f32, baseline: f32, size: f32, color: Rgba) {
        if character.is_whitespace() {
            return;
        }
        let width = self.advance(character, size) - 2.0;
        let height = (size * 0.7).round();
        canvas.fill_rect(
            crate::canvas::Rect::new(x + 1.0, baseline - height, width.max(1.0), height),
            color,
        );
    }
}

#[derive(Debug, Clone, Copy)]
struct TableRange {
    start: usize,
    end: usize,
}

/// A TrueType font (Apple's TrueType Reference Manual and OpenType's `glyf` and `cmap`
/// tables), read from the bytes of a `.ttf` file or the first font of a `.ttc` collection.
#[derive(Clone)]
pub struct TrueTypeFont {
    data: Vec<u8>,
    units_per_em: f32,
    long_offsets: bool,
    ascender: f32,
    descender: f32,
    glyph_count: u16,
    metric_count: u16,
    hmtx: TableRange,
    loca: TableRange,
    glyf: TableRange,
    cmap: TableRange,
}

impl fmt::Debug for TrueTypeFont {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrueTypeFont")
            .field("units_per_em", &self.units_per_em)
            .field("glyph_count", &self.glyph_count)
            .finish_non_exhaustive()
    }
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*data.get(at)?, *data.get(at + 1)?]))
}

fn i16_at(data: &[u8], at: usize) -> Option<i16> {
    u16_at(data, at).map(|value| value as i16)
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *data.get(at)?,
        *data.get(at + 1)?,
        *data.get(at + 2)?,
        *data.get(at + 3)?,
    ]))
}

#[derive(Debug, Clone, Copy)]
struct Point {
    x: f32,
    y: f32,
    on_curve: bool,
}

impl TrueTypeFont {
    /// Reads `data`, a TrueType font or collection.
    pub fn parse(data: Vec<u8>) -> Result<Self, FontError> {
        let mut base = 0usize;
        if data.get(..4) == Some(b"ttcf") {
            base = u32_at(&data, 12).ok_or(FontError::Malformed("collection header"))? as usize;
        }
        match u32_at(&data, base) {
            Some(0x0001_0000 | 0x7472_7565) => {}
            Some(0x4F54_544F) => return Err(FontError::Unsupported),
            _ => return Err(FontError::Malformed("header")),
        }
        let table_count = u16_at(&data, base + 4).ok_or(FontError::Malformed("header"))?;
        let find = |tag: &[u8; 4]| -> Option<TableRange> {
            (0..usize::from(table_count)).find_map(|index| {
                let record = base + 12 + 16 * index;
                (data.get(record..record + 4)? == tag).then_some(())?;
                let start = u32_at(&data, record + 8)? as usize;
                let end = start.checked_add(u32_at(&data, record + 12)? as usize)?;
                (end <= data.len()).then_some(TableRange { start, end })
            })
        };
        let table = |tag: &[u8; 4], name: &'static str| find(tag).ok_or(FontError::Malformed(name));
        let head = table(b"head", "head")?;
        let hhea = table(b"hhea", "hhea")?;
        let maxp = table(b"maxp", "maxp")?;
        let units_per_em = u16_at(&data, head.start + 18).filter(|units| *units >= 16)
            .ok_or(FontError::Malformed("head"))?;
        Ok(Self {
            units_per_em: f32::from(units_per_em),
            long_offsets: i16_at(&data, head.start + 50).ok_or(FontError::Malformed("head"))? != 0,
            ascender: f32::from(i16_at(&data, hhea.start + 4).ok_or(FontError::Malformed("hhea"))?),
            descender: f32::from(i16_at(&data, hhea.start + 6).ok_or(FontError::Malformed("hhea"))?),
            metric_count: u16_at(&data, hhea.start + 34).filter(|count| *count > 0)
                .ok_or(FontError::Malformed("hhea"))?,
            glyph_count: u16_at(&data, maxp.start + 4).ok_or(FontError::Malformed("maxp"))?,
            hmtx: table(b"hmtx", "hmtx")?,
            loca: table(b"loca", "loca")?,
            glyf: table(b"glyf", "glyf")?,
            cmap: table(b"cmap", "cmap")?,
            data,
        })
    }

    fn scale(&self, size: f32) -> f32 {
        size / self.units_per_em
    }

    /// The glyph `character` maps to, 0 (the missing glyph) when none.
    pub fn glyph_index(&self, character: char) -> u16 {
        let code = u32::from(character);
        let data = &self.data;
        let start = self.cmap.start;
        let count = u16_at(data, start + 2).unwrap_or(0);
        let mut best: Option<(u8, usize)> = None;
        for index in 0..usize::from(count) {
            let record = start + 4 + 8 * index;
            let (Some(platform), Some(encoding), Some(offset)) =
                (u16_at(data, record), u16_at(data, record + 2), u32_at(data, record + 4))
            else {
                break;
            };
            let rank = match (platform, encoding) {
                (3, 10) | (0, 4) | (0, 6) => 3,
                (3, 1) | (0, 3) => 2,
                (0, _) => 1,
                _ => continue,
            };
            let subtable = start + offset as usize;
            if best.is_none_or(|(held, _)| rank > held) {
                best = Some((rank, subtable));
            }
        }
        let Some((_, subtable)) = best else {
            return 0;
        };
        match u16_at(data, subtable) {
            Some(4) => self.format4(subtable, code),
            Some(12) => self.format12(subtable, code),
            _ => 0,
        }
        .filter(|glyph| *glyph < self.glyph_count)
        .unwrap_or(0)
    }

    fn format4(&self, subtable: usize, code: u32) -> Option<u16> {
        let code = u16::try_from(code).ok()?;
        let data = &self.data;
        let segments = usize::from(u16_at(data, subtable + 6)? / 2);
        let ends = subtable + 14;
        let starts = ends + 2 * segments + 2;
        let deltas = starts + 2 * segments;
        let ranges = deltas + 2 * segments;
        for segment in 0..segments {
            if code > u16_at(data, ends + 2 * segment)? {
                continue;
            }
            let first = u16_at(data, starts + 2 * segment)?;
            if code < first {
                return None;
            }
            let delta = u16_at(data, deltas + 2 * segment)?;
            let range_at = ranges + 2 * segment;
            let range = u16_at(data, range_at)?;
            if range == 0 {
                return Some(code.wrapping_add(delta));
            }
            let at = range_at + usize::from(range) + 2 * usize::from(code - first);
            let glyph = u16_at(data, at)?;
            return (glyph != 0).then(|| glyph.wrapping_add(delta));
        }
        None
    }

    fn format12(&self, subtable: usize, code: u32) -> Option<u16> {
        let data = &self.data;
        let groups = u32_at(data, subtable + 12)? as usize;
        for group in 0..groups.min(1 << 16) {
            let at = subtable + 16 + 12 * group;
            let (first, last, glyph) = (u32_at(data, at)?, u32_at(data, at + 4)?, u32_at(data, at + 8)?);
            if (first..=last).contains(&code) {
                return u16::try_from(glyph.checked_add(code - first)?).ok();
            }
        }
        None
    }

    fn glyph_advance(&self, glyph: u16) -> f32 {
        let index = usize::from(glyph.min(self.metric_count - 1));
        f32::from(u16_at(&self.data, self.hmtx.start + 4 * index).unwrap_or(0))
    }

    fn glyph_range(&self, glyph: u16) -> Option<(usize, usize)> {
        let index = usize::from(glyph);
        let (start, end) = if self.long_offsets {
            (
                u32_at(&self.data, self.loca.start + 4 * index)? as usize,
                u32_at(&self.data, self.loca.start + 4 * index + 4)? as usize,
            )
        } else {
            (
                2 * usize::from(u16_at(&self.data, self.loca.start + 2 * index)?),
                2 * usize::from(u16_at(&self.data, self.loca.start + 2 * index + 2)?),
            )
        };
        let (start, end) = (self.glyf.start + start, self.glyf.start + end);
        (start <= end && end <= self.glyf.end).then_some((start, end))
    }

    fn contours(&self, glyph: u16, depth: usize, out: &mut Vec<Vec<Point>>) -> Option<()> {
        if depth > MAX_COMPOSITE_DEPTH {
            return None;
        }
        let (start, end) = self.glyph_range(glyph)?;
        if start == end {
            return Some(());
        }
        let data = &self.data[..end];
        let contour_count = i16_at(data, start)?;
        if contour_count >= 0 {
            self.simple(data, start, contour_count as usize, out)
        } else {
            self.composite(data, start + 10, depth, out)
        }
    }

    fn simple(&self, data: &[u8], start: usize, count: usize, out: &mut Vec<Vec<Point>>) -> Option<()> {
        let mut ends = Vec::with_capacity(count);
        for index in 0..count {
            ends.push(usize::from(u16_at(data, start + 10 + 2 * index)?));
        }
        let point_count = ends.last().map_or(0, |last| last + 1);
        if point_count > MAX_POINTS || ends.windows(2).any(|pair| pair[1] < pair[0]) {
            return None;
        }
        let instructions = start + 10 + 2 * count;
        let mut at = instructions + 2 + usize::from(u16_at(data, instructions)?);
        let mut flags = Vec::with_capacity(point_count);
        while flags.len() < point_count {
            let flag = *data.get(at)?;
            at += 1;
            flags.push(flag);
            if flag & 8 != 0 {
                let repeat = *data.get(at)?;
                at += 1;
                for _ in 0..repeat {
                    flags.push(flag);
                }
            }
        }
        flags.truncate(point_count);
        let mut read_axis = |short: u8, same: u8| -> Option<Vec<i32>> {
            let mut value = 0i32;
            let mut values = Vec::with_capacity(point_count);
            for &flag in &flags {
                if flag & short != 0 {
                    let delta = i32::from(*data.get(at)?);
                    at += 1;
                    value += if flag & same != 0 { delta } else { -delta };
                } else if flag & same == 0 {
                    value += i32::from(i16_at(data, at)?);
                    at += 2;
                }
                values.push(value);
            }
            Some(values)
        };
        let xs = read_axis(2, 16)?;
        let ys = read_axis(4, 32)?;
        let mut first = 0;
        for end in ends {
            let contour = (first..=end)
                .map(|index| Point {
                    x: xs[index] as f32,
                    y: ys[index] as f32,
                    on_curve: flags[index] & 1 != 0,
                })
                .collect();
            out.push(contour);
            first = end + 1;
        }
        Some(())
    }

    fn composite(&self, data: &[u8], mut at: usize, depth: usize, out: &mut Vec<Vec<Point>>) -> Option<()> {
        loop {
            let flags = u16_at(data, at)?;
            let glyph = u16_at(data, at + 2)?;
            at += 4;
            let (dx, dy) = if flags & 1 != 0 {
                let pair = (f32::from(i16_at(data, at)?), f32::from(i16_at(data, at + 2)?));
                at += 4;
                pair
            } else {
                let pair = (f32::from(*data.get(at)? as i8), f32::from(*data.get(at + 1)? as i8));
                at += 2;
                pair
            };
            let f2dot14 = |at: usize| i16_at(data, at).map(|value| f32::from(value) / 16384.0);
            let (mut xx, mut xy, mut yx, mut yy) = (1.0, 0.0, 0.0, 1.0);
            if flags & 8 != 0 {
                xx = f2dot14(at)?;
                yy = xx;
                at += 2;
            } else if flags & 0x40 != 0 {
                xx = f2dot14(at)?;
                yy = f2dot14(at + 2)?;
                at += 4;
            } else if flags & 0x80 != 0 {
                xx = f2dot14(at)?;
                xy = f2dot14(at + 2)?;
                yx = f2dot14(at + 4)?;
                yy = f2dot14(at + 6)?;
                at += 8;
            }
            let mut parts = Vec::new();
            self.contours(glyph, depth + 1, &mut parts)?;
            for contour in parts {
                out.push(
                    contour
                        .into_iter()
                        .map(|point| Point {
                            x: point.x * xx + point.y * yx + dx,
                            y: point.x * xy + point.y * yy + dy,
                            on_curve: point.on_curve,
                        })
                        .collect(),
                );
            }
            if flags & 0x20 == 0 {
                return Some(());
            }
        }
    }
}

fn flattened(contour: &[Point]) -> Vec<(f32, f32)> {
    let count = contour.len();
    let mut out = Vec::new();
    if count < 2 {
        return out;
    }
    let start_index = contour.iter().position(|point| point.on_curve);
    let start = match start_index {
        Some(index) => (contour[index].x, contour[index].y),
        None => (
            (contour[0].x + contour[count - 1].x) / 2.0,
            (contour[0].y + contour[count - 1].y) / 2.0,
        ),
    };
    let first = start_index.unwrap_or(0);
    out.push(start);
    let mut control: Option<(f32, f32)> = None;
    let mut current = start;
    for step in 1..=count {
        let point = contour[(first + step) % count];
        let here = (point.x, point.y);
        match (point.on_curve, control) {
            (true, None) => {
                out.push(here);
                current = here;
            }
            (true, Some(through)) => {
                quadratic(&mut out, current, through, here);
                current = here;
                control = None;
            }
            (false, None) => control = Some(here),
            (false, Some(through)) => {
                let middle = ((through.0 + here.0) / 2.0, (through.1 + here.1) / 2.0);
                quadratic(&mut out, current, through, middle);
                current = middle;
                control = Some(here);
            }
        }
    }
    if let Some(through) = control {
        quadratic(&mut out, current, through, start);
    }
    out
}

fn quadratic(out: &mut Vec<(f32, f32)>, from: (f32, f32), through: (f32, f32), to: (f32, f32)) {
    for step in 1..=CURVE_STEPS {
        let t = step as f32 / CURVE_STEPS as f32;
        let u = 1.0 - t;
        out.push((
            u * u * from.0 + 2.0 * u * t * through.0 + t * t * to.0,
            u * u * from.1 + 2.0 * u * t * through.1 + t * t * to.1,
        ));
    }
}

struct Coverage {
    width: usize,
    height: usize,
    area: Vec<f32>,
}

impl Coverage {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            area: vec![0.0; (width + 2) * height],
        }
    }

    fn add(&mut self, row: usize, column: i64, amount: f32) {
        if column < 0 {
            return;
        }
        let column = (column as usize).min(self.width + 1);
        self.area[row * (self.width + 2) + column] += amount;
    }

    fn line(&mut self, from: (f32, f32), to: (f32, f32)) {
        if (from.1 - to.1).abs() < f32::EPSILON {
            return;
        }
        let (direction, top, bottom) = if from.1 < to.1 { (1.0, from, to) } else { (-1.0, to, from) };
        let slope = (bottom.0 - top.0) / (bottom.1 - top.1);
        let first_row = top.1.max(0.0).floor() as usize;
        let last_row = (bottom.1.ceil().max(0.0) as usize).min(self.height);
        let mut x = top.0 + ((first_row as f32).max(top.1) - top.1) * slope;
        for row in first_row..last_row {
            let row_top = (row as f32).max(top.1);
            let row_bottom = (row as f32 + 1.0).min(bottom.1);
            let dy = row_bottom - row_top;
            if dy <= 0.0 {
                continue;
            }
            let next = x + slope * dy;
            let amount = dy * direction;
            let (left, right) = if x < next { (x, next) } else { (next, x) };
            let left_floor = left.floor();
            let left_column = left_floor as i64;
            let right_column = right.ceil() as i64;
            if right_column <= left_column + 1 {
                let middle = 0.5 * (x + next) - left_floor;
                self.add(row, left_column, amount * (1.0 - middle));
                self.add(row, left_column + 1, amount * middle);
            } else {
                let inverse = 1.0 / (right - left);
                let left_fraction = left - left_floor;
                let first_area = 0.5 * inverse * (1.0 - left_fraction).powi(2);
                let right_fraction = right - right.ceil() + 1.0;
                let last_area = 0.5 * inverse * right_fraction.powi(2);
                self.add(row, left_column, amount * first_area);
                let mut running = first_area;
                for column in left_column + 1..right_column - 1 {
                    let step = if column == left_column + 1 {
                        inverse * (1.5 - left_fraction) - first_area
                    } else {
                        inverse
                    };
                    self.add(row, column, amount * step);
                    running += step;
                }
                self.add(row, right_column - 1, amount * (1.0 - running - last_area));
                self.add(row, right_column, amount * last_area);
            }
            x = next;
        }
    }
}

impl Typeface for TrueTypeFont {
    fn advance(&self, character: char, size: f32) -> f32 {
        self.glyph_advance(self.glyph_index(character)) * self.scale(size)
    }

    fn extent(&self, size: f32) -> (f32, f32) {
        (self.ascender * self.scale(size), -self.descender * self.scale(size))
    }

    fn draw_glyph(&self, canvas: &mut Canvas, character: char, x: f32, baseline: f32, size: f32, color: Rgba) {
        let mut contours = Vec::new();
        if self.contours(self.glyph_index(character), 0, &mut contours).is_none() {
            return;
        }
        let scale = self.scale(size);
        let (ascent, descent) = self.extent(size);
        let left = x.floor();
        let top = (baseline - ascent).floor() - 1.0;
        let width = (self.advance(character, size) + size * 0.5).ceil().max(1.0) as usize + 2;
        let height = (ascent + descent).ceil() as usize + 3;
        let mut coverage = Coverage::new(width, height);
        for contour in &contours {
            let points = flattened(contour);
            for pair in points.windows(2) {
                let map = |point: (f32, f32)| (x - left + point.0 * scale, baseline - top - point.1 * scale);
                coverage.line(map(pair[0]), map(pair[1]));
            }
        }
        for row in 0..height {
            let mut sum = 0.0f32;
            for column in 0..width {
                sum += coverage.area[row * (width + 2) + column];
                let alpha = sum.abs().min(1.0);
                canvas.blend(left as i64 + column as i64, top as i64 + row as i64, color, alpha);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_coverage_of_a_square_fills_it() {
        let mut coverage = Coverage::new(10, 10);
        let square = [(2.0, 2.0), (6.0, 2.0), (6.0, 6.0), (2.0, 6.0), (2.0, 2.0)];
        for pair in square.windows(2) {
            coverage.line(pair[0], pair[1]);
        }
        let mut filled = 0;
        for row in 0..10 {
            let mut sum = 0.0f32;
            for column in 0..10 {
                sum += coverage.area[row * 12 + column];
                if sum.abs() > 0.99 {
                    filled += 1;
                }
            }
        }
        assert_eq!(filled, 16);
    }

    #[test]
    fn hostile_font_files_are_errors_and_never_panic() {
        assert!(TrueTypeFont::parse(Vec::new()).is_err());
        assert_eq!(TrueTypeFont::parse(b"OTTO\0\0\0\0\0\0\0\0".to_vec()).unwrap_err(), FontError::Unsupported);
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..2000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let mut bytes = vec![0u8; 300];
            bytes[..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
            for byte in bytes.iter_mut().skip(4) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *byte = (seed >> 32) as u8;
            }
            if let Ok(font) = TrueTypeFont::parse(bytes) {
                let mut canvas = Canvas::new(32, 32, Rgba::hex(0xFFFFFF));
                let style = TextStyle { size: 13.0, line_height: 18.0, color: Rgba::hex(0) };
                font.draw_text(&mut canvas, "Ag", 2.0, 2.0, style);
            }
        }
    }

    #[test]
    fn the_box_face_draws_the_same_pixels_everywhere() {
        let mut canvas = Canvas::new(40, 20, Rgba::hex(0xFFFFFF));
        let style = TextStyle { size: 10.0, line_height: 20.0, color: Rgba::hex(0) };
        let end = BoxFace.draw_text(&mut canvas, "ab", 2.0, 0.0, style);
        assert_eq!(end, 14.0);
        assert_eq!(canvas.pixel(4, 12), Some(Rgba::hex(0)));
        assert_eq!(canvas.pixel(8, 12), Some(Rgba::hex(0xFFFFFF)));
    }
}
