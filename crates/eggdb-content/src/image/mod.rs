//! Image renditions (D1078): PNG and baseline JPEG read with codecs of EggDB's own, the Exif
//! orientation applied, the picture scaled down to fit a box by area averaging, and the result
//! written as PNG or baseline JPEG. Both formats are royalty-free.

mod jpeg_decode;
mod jpeg_encode;
mod png;

use crate::ContentError;

const MOST_PIXELS: usize = 64 << 20;

/// A decoded picture: 8-bit RGBA, rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Its width in pixels.
    pub width: usize,
    /// Its height in pixels.
    pub height: usize,
    /// Four bytes per pixel, red, green, blue and alpha.
    pub rgba: Vec<u8>,
}

/// The format a rendition is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenditionFormat {
    /// PNG, lossless, keeping transparency.
    Png,
    /// Baseline JPEG at a quality from 1 to 100.
    Jpeg(u8),
}

fn failure(message: impl Into<String>) -> ContentError {
    ContentError(message.into())
}

/// Decodes a PNG or a baseline JPEG, turned upright by its Exif orientation.
pub fn decode_image(bytes: &[u8]) -> Result<Image, ContentError> {
    if png::is_png(bytes) {
        return png::decode(bytes);
    }
    if jpeg_decode::is_jpeg(bytes) {
        let (image, orientation) = jpeg_decode::decode(bytes)?;
        return Ok(oriented(image, orientation));
    }
    Err(failure("renditions read PNG and JPEG images"))
}

/// The format the image in `bytes` is in, as its rendition keeps by default.
pub fn source_format(bytes: &[u8]) -> Option<RenditionFormat> {
    if png::is_png(bytes) {
        Some(RenditionFormat::Png)
    } else if jpeg_decode::is_jpeg(bytes) {
        Some(RenditionFormat::Jpeg(85))
    } else {
        None
    }
}

fn oriented(image: Image, orientation: u8) -> Image {
    if orientation <= 1 || orientation > 8 {
        return image;
    }
    let swapped = orientation >= 5;
    let (width, height) = if swapped { (image.height, image.width) } else { (image.width, image.height) };
    let mut out = Image { width, height, rgba: vec![0u8; image.rgba.len()] };
    for y in 0..height {
        for x in 0..width {
            let (source_x, source_y) = match orientation {
                2 => (image.width - 1 - x, y),
                3 => (image.width - 1 - x, image.height - 1 - y),
                4 => (x, image.height - 1 - y),
                5 => (y, x),
                6 => (y, image.height - 1 - x),
                7 => (image.width - 1 - y, image.height - 1 - x),
                _ => (image.width - 1 - y, x),
            };
            let (from, to) = ((source_y * image.width + source_x) * 4, (y * width + x) * 4);
            out.rgba[to..to + 4].copy_from_slice(&image.rgba[from..from + 4]);
        }
    }
    out
}

fn scaled_axis(source: usize, target: usize) -> Vec<Vec<(usize, f32)>> {
    let ratio = source as f64 / target as f64;
    (0..target)
        .map(|index| {
            let (start, end) = (index as f64 * ratio, (index + 1) as f64 * ratio);
            let mut weights = Vec::new();
            let mut position = start.floor() as usize;
            while (position as f64) < end && position < source {
                let covered = (end.min(position as f64 + 1.0) - start.max(position as f64)) as f32;
                if covered > 0.0 {
                    weights.push((position, covered / ratio as f32));
                }
                position += 1;
            }
            weights
        })
        .collect()
}

/// `image` scaled down to fit `max_width` by `max_height`, keeping its shape, by averaging
/// the area each new pixel covers (with alpha premultiplied); never enlarged.
pub fn fit(image: &Image, max_width: usize, max_height: usize) -> Image {
    let scale = (max_width as f64 / image.width as f64).min(max_height as f64 / image.height as f64).min(1.0);
    let width = ((image.width as f64 * scale).round() as usize).max(1);
    let height = ((image.height as f64 * scale).round() as usize).max(1);
    if width == image.width && height == image.height {
        return image.clone();
    }
    let columns = scaled_axis(image.width, width);
    let rows = scaled_axis(image.height, height);
    let mut across = vec![0f32; width * image.height * 4];
    for y in 0..image.height {
        for (x, weights) in columns.iter().enumerate() {
            let mut sum = [0f32; 4];
            for &(source_x, weight) in weights {
                let at = (y * image.width + source_x) * 4;
                let alpha = f32::from(image.rgba[at + 3]) / 255.0;
                for channel in 0..3 {
                    sum[channel] += f32::from(image.rgba[at + channel]) * alpha * weight;
                }
                sum[3] += alpha * weight;
            }
            across[(y * width + x) * 4..(y * width + x) * 4 + 4].copy_from_slice(&sum);
        }
    }
    let mut out = Image { width, height, rgba: vec![0u8; width * height * 4] };
    for (y, weights) in rows.iter().enumerate() {
        for x in 0..width {
            let mut sum = [0f32; 4];
            for &(source_y, weight) in weights {
                let at = (source_y * width + x) * 4;
                for channel in 0..4 {
                    sum[channel] += across[at + channel] * weight;
                }
            }
            let at = (y * width + x) * 4;
            let alpha = sum[3];
            for channel in 0..3 {
                out.rgba[at + channel] = if alpha > 0.0 { (sum[channel] / alpha).round().clamp(0.0, 255.0) as u8 } else { 0 };
            }
            out.rgba[at + 3] = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// Encodes `image` as `format`.
pub fn encode_image(image: &Image, format: RenditionFormat) -> Vec<u8> {
    match format {
        RenditionFormat::Png => png::encode(image),
        RenditionFormat::Jpeg(quality) => jpeg_encode::encode(image, quality),
    }
}

/// A rendition of the PNG or JPEG `bytes`: upright, fitted into `max_width` by `max_height`,
/// written as `format` (the source's own when `None`).
pub fn rendition(bytes: &[u8], max_width: usize, max_height: usize, format: Option<RenditionFormat>) -> Result<Vec<u8>, ContentError> {
    if max_width == 0 || max_height == 0 || max_width > 65_535 || max_height > 65_535 {
        return Err(failure("a rendition is 1 to 65,535 pixels wide and high"));
    }
    let format = format.or_else(|| source_format(bytes)).ok_or_else(|| failure("renditions read PNG and JPEG images"))?;
    let image = decode_image(bytes)?;
    Ok(encode_image(&fit(&image, max_width, max_height), format))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: usize, height: usize) -> Image {
        let mut rgba = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[(x * 255 / width) as u8, (y * 255 / height) as u8, 128, 255]);
            }
        }
        Image { width, height, rgba }
    }

    #[test]
    fn png_round_trips_and_fits_a_box() {
        let image = gradient(40, 20);
        let encoded = encode_image(&image, RenditionFormat::Png);
        assert_eq!(decode_image(&encoded).unwrap(), image);
        let small = decode_image(&rendition(&encoded, 10, 10, None).unwrap()).unwrap();
        assert_eq!((small.width, small.height), (10, 5));
        assert!(rendition(&encoded, 0, 10, None).is_err());
        assert!(decode_image(b"\x89PNG\r\n\x1a\nbroken").is_err());
    }

    #[test]
    fn jpeg_round_trips_within_its_loss() {
        let image = gradient(37, 23);
        let encoded = encode_image(&image, RenditionFormat::Jpeg(90));
        assert!(encoded.starts_with(&[0xFF, 0xD8]) && encoded.ends_with(&[0xFF, 0xD9]));
        let decoded = decode_image(&encoded).unwrap();
        assert_eq!((decoded.width, decoded.height), (37, 23));
        let worst = image.rgba.iter().zip(&decoded.rgba).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
        assert!(worst < 40, "the largest channel error was {worst}");
        let thumbnail = rendition(&encoded, 8, 8, Some(RenditionFormat::Png)).unwrap();
        assert!(thumbnail.starts_with(b"\x89PNG"));
        assert!(decode_image(&encoded[..encoded.len() / 2]).is_err());
    }

    #[test]
    fn exif_orientations_turn_the_picture_upright() {
        let image = Image { width: 2, height: 1, rgba: vec![1, 1, 1, 255, 2, 2, 2, 255] };
        let turned = oriented(image, 6);
        assert_eq!((turned.width, turned.height), (1, 2));
        assert_eq!(turned.rgba[0], 1);
    }
}
