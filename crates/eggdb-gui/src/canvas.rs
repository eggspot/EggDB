use crate::color::Rgba;

/// A rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// The rectangle at (`x`, `y`) of `w` × `h`.
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

/// An RGBA framebuffer that shapes are drawn into, with antialiased edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<Rgba>,
}

impl Canvas {
    /// A `width` × `height` canvas filled with `background`.
    pub fn new(width: usize, height: usize, background: Rgba) -> Self {
        Self {
            width,
            height,
            pixels: vec![background; width * height],
        }
    }

    /// The width in pixels.
    pub fn width(&self) -> usize {
        self.width
    }

    /// The height in pixels.
    pub fn height(&self) -> usize {
        self.height
    }

    /// The colour at (`x`, `y`), or `None` outside the canvas.
    pub fn pixel(&self, x: usize, y: usize) -> Option<Rgba> {
        (x < self.width && y < self.height).then(|| self.pixels[y * self.width + x])
    }

    /// The pixels, row by row.
    pub fn pixels(&self) -> &[Rgba] {
        &self.pixels
    }

    /// Mixes `color` into (`x`, `y`) with `coverage`; points outside are ignored.
    pub fn blend(&mut self, x: i64, y: i64, color: Rgba, coverage: f32) {
        if coverage <= 0.0 || x < 0 || y < 0 {
            return;
        }
        let (x, y) = (x as usize, y as usize);
        if x >= self.width || y >= self.height {
            return;
        }
        let at = y * self.width + x;
        self.pixels[at] = color.over(self.pixels[at], coverage);
    }

    fn coverage_span(start: f32, end: f32, pixel: i64) -> f32 {
        let left = (pixel as f32).max(start);
        let right = (pixel as f32 + 1.0).min(end);
        (right - left).max(0.0)
    }

    /// Fills `rect`, with partly covered edge pixels blended.
    pub fn fill_rect(&mut self, rect: Rect, color: Rgba) {
        let (x0, x1) = (rect.x.floor() as i64, (rect.x + rect.w).ceil() as i64);
        let (y0, y1) = (rect.y.floor() as i64, (rect.y + rect.h).ceil() as i64);
        for y in y0..y1 {
            let vertical = Self::coverage_span(rect.y, rect.y + rect.h, y);
            for x in x0..x1 {
                let horizontal = Self::coverage_span(rect.x, rect.x + rect.w, x);
                self.blend(x, y, color, vertical * horizontal);
            }
        }
    }

    /// Fills `rect` with corners rounded to `radius`.
    pub fn fill_rounded_rect(&mut self, rect: Rect, radius: f32, color: Rgba) {
        let radius = radius.min(rect.w / 2.0).min(rect.h / 2.0).max(0.0);
        let (x0, x1) = (rect.x.floor() as i64, (rect.x + rect.w).ceil() as i64);
        let (y0, y1) = (rect.y.floor() as i64, (rect.y + rect.h).ceil() as i64);
        for y in y0..y1 {
            for x in x0..x1 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let cx = px.clamp(rect.x + radius, rect.x + rect.w - radius);
                let cy = py.clamp(rect.y + radius, rect.y + rect.h - radius);
                let distance = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                let inside_corner = (radius - distance + 0.5).clamp(0.0, 1.0);
                let edge = Self::coverage_span(rect.x, rect.x + rect.w, x)
                    * Self::coverage_span(rect.y, rect.y + rect.h, y);
                let coverage = if radius > 0.0 { inside_corner.min(edge) } else { edge };
                self.blend(x, y, color, coverage);
            }
        }
    }

    /// Draws a 1-pixel outline just inside `rect`.
    pub fn stroke_rect(&mut self, rect: Rect, color: Rgba) {
        self.fill_rect(Rect::new(rect.x, rect.y, rect.w, 1.0), color);
        self.fill_rect(Rect::new(rect.x, rect.y + rect.h - 1.0, rect.w, 1.0), color);
        self.fill_rect(Rect::new(rect.x, rect.y + 1.0, 1.0, rect.h - 2.0), color);
        self.fill_rect(Rect::new(rect.x + rect.w - 1.0, rect.y + 1.0, 1.0, rect.h - 2.0), color);
    }

    /// Fills the circle of `diameter` centred at (`cx`, `cy`).
    pub fn fill_circle(&mut self, cx: f32, cy: f32, diameter: f32, color: Rgba) {
        self.ring(cx, cy, diameter / 2.0, None, color);
    }

    /// Draws the circle of `diameter` centred at (`cx`, `cy`) as a stroke `width` wide, inside
    /// the diameter.
    pub fn stroke_circle(&mut self, cx: f32, cy: f32, diameter: f32, width: f32, color: Rgba) {
        self.ring(cx, cy, diameter / 2.0, Some(diameter / 2.0 - width), color);
    }

    fn ring(&mut self, cx: f32, cy: f32, outer: f32, inner: Option<f32>, color: Rgba) {
        let (x0, x1) = ((cx - outer).floor() as i64, (cx + outer).ceil() as i64);
        let (y0, y1) = ((cy - outer).floor() as i64, (cy + outer).ceil() as i64);
        for y in y0..y1 {
            for x in x0..x1 {
                let distance =
                    ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let mut coverage = (outer - distance + 0.5).clamp(0.0, 1.0);
                if let Some(inner) = inner {
                    coverage = coverage.min((distance - inner + 0.5).clamp(0.0, 1.0));
                }
                self.blend(x, y, color, coverage);
            }
        }
    }

    /// Draws a line from (`x0`, `y0`) to (`x1`, `y1`) `width` wide, with round ends.
    pub fn line(&mut self, from: (f32, f32), to: (f32, f32), width: f32, color: Rgba) {
        let half = width / 2.0;
        let (min_x, max_x) = (from.0.min(to.0) - half, from.0.max(to.0) + half);
        let (min_y, max_y) = (from.1.min(to.1) - half, from.1.max(to.1) + half);
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let length_squared = dx * dx + dy * dy;
        for y in min_y.floor() as i64..max_y.ceil() as i64 {
            for x in min_x.floor() as i64..max_x.ceil() as i64 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let t = if length_squared > 0.0 {
                    (((px - from.0) * dx + (py - from.1) * dy) / length_squared).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let (nx, ny) = (from.0 + t * dx, from.1 + t * dy);
                let distance = ((px - nx).powi(2) + (py - ny).powi(2)).sqrt();
                self.blend(x, y, color, (half - distance + 0.5).clamp(0.0, 1.0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: Rgba = Rgba::hex(0xFFFFFF);
    const BLACK: Rgba = Rgba::hex(0x000000);

    #[test]
    fn whole_pixel_rects_are_exact_and_half_pixels_blend() {
        let mut canvas = Canvas::new(10, 10, WHITE);
        canvas.fill_rect(Rect::new(2.0, 2.0, 3.0, 3.0), BLACK);
        assert_eq!(canvas.pixel(2, 2), Some(BLACK));
        assert_eq!(canvas.pixel(4, 4), Some(BLACK));
        assert_eq!(canvas.pixel(5, 5), Some(WHITE));
        canvas.fill_rect(Rect::new(6.5, 0.0, 1.0, 1.0), BLACK);
        assert_eq!(canvas.pixel(6, 0), Some(Rgba::hex(0x808080)));
    }

    #[test]
    fn circles_lines_and_rounded_corners_stay_inside_their_bounds() {
        let mut canvas = Canvas::new(40, 40, WHITE);
        canvas.fill_circle(20.0, 20.0, 10.0, BLACK);
        assert_eq!(canvas.pixel(20, 20), Some(BLACK));
        assert_eq!(canvas.pixel(20, 10), Some(WHITE));
        canvas.line((0.0, 35.5), (39.0, 35.5), 1.0, BLACK);
        assert_eq!(canvas.pixel(10, 35), Some(BLACK));
        canvas.fill_rounded_rect(Rect::new(0.0, 0.0, 12.0, 12.0), 6.0, BLACK);
        assert_eq!(canvas.pixel(0, 0), Some(WHITE));
        assert_eq!(canvas.pixel(6, 6), Some(BLACK));
        canvas.fill_rect(Rect::new(-5.0, -5.0, 1000.0, 2.0), BLACK);
    }
}
