//! Image renditions (D1078) and transforms (D1179): PNG and baseline JPEG read with codecs of
//! EggDB's own, the Exif orientation applied, the picture cropped, scaled by area averaging to
//! fit, cover or fill a box, and the result written as PNG or baseline JPEG. Both formats are
//! royalty-free.

mod jpeg_decode;
mod jpeg_encode;
mod png;

use crate::ContentError;

const MOST_PIXELS: usize = 64 << 20;
const MOST_SIDE: usize = 65_535;

/// How a transform fits the picture into the width and height it asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// Inside the box, keeping the picture's shape.
    #[default]
    Contain,
    /// Over the whole box, keeping the shape, and centred with the overflow cut off.
    Cover,
    /// Exactly the box, stretched.
    Fill,
}

/// A rectangle of the picture, in pixels from its top left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crop {
    /// The left edge.
    pub x: usize,
    /// The top edge.
    pub y: usize,
    /// The width.
    pub width: usize,
    /// The height.
    pub height: usize,
}

/// What a transform does to a picture: crop it first, then scale it into the box, then write
/// it in a format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Transform {
    /// The box's width; from the height and the picture's shape when only that is given.
    pub width: Option<usize>,
    /// The box's height; from the width and the picture's shape when only that is given.
    pub height: Option<usize>,
    /// How the picture fits the box.
    pub fit: Fit,
    /// The rectangle kept before scaling.
    pub crop: Option<Crop>,
    /// The format written; the source's own when `None`.
    pub format: Option<RenditionFormat>,
    /// Whether the picture may grow beyond its own size.
    pub enlarge: bool,
}

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
    resampled(image, width, height)
}

/// The rectangle `crop` of `image`; an error when it reaches outside the picture or is empty.
pub fn crop(image: &Image, crop: Crop) -> Result<Image, ContentError> {
    let inside = crop.width > 0
        && crop.height > 0
        && crop.x.checked_add(crop.width).is_some_and(|right| right <= image.width)
        && crop.y.checked_add(crop.height).is_some_and(|bottom| bottom <= image.height);
    if !inside {
        return Err(failure(format!(
            "the crop {}x{} at {},{} is not inside the {}x{} picture",
            crop.width, crop.height, crop.x, crop.y, image.width, image.height
        )));
    }
    let mut rgba = Vec::with_capacity(crop.width * crop.height * 4);
    for row in crop.y..crop.y + crop.height {
        let start = (row * image.width + crop.x) * 4;
        rgba.extend_from_slice(&image.rgba[start..start + crop.width * 4]);
    }
    Ok(Image {
        width: crop.width,
        height: crop.height,
        rgba,
    })
}

fn scaled_side(side: usize, scale: f64) -> usize {
    ((side as f64 * scale).round() as usize).clamp(1, MOST_SIDE)
}

fn boxed(image: &Image, transform: &Transform) -> (usize, usize) {
    let (source_width, source_height) = (image.width as f64, image.height as f64);
    match (transform.width, transform.height) {
        (Some(width), Some(height)) => (width, height),
        (Some(width), None) => (width, scaled_side(image.height, width as f64 / source_width)),
        (None, Some(height)) => (scaled_side(image.width, height as f64 / source_height), height),
        (None, None) => (image.width, image.height),
    }
}

/// `image` transformed as `transform` says, before it is written: cropped, then contained in,
/// covering or filling the box.
pub fn transformed(image: &Image, transform: &Transform) -> Result<Image, ContentError> {
    let cropped = match transform.crop {
        Some(rectangle) => crop(image, rectangle)?,
        None => image.clone(),
    };
    let (width, height) = boxed(&cropped, transform);
    if width == 0 || height == 0 || width > MOST_SIDE || height > MOST_SIDE || width.saturating_mul(height) > MOST_PIXELS {
        return Err(failure("a transform is 1 to 65,535 pixels wide and high, and at most 64 megapixels"));
    }
    let limit = |scale: f64| if transform.enlarge { scale } else { scale.min(1.0) };
    let (source_width, source_height) = (cropped.width as f64, cropped.height as f64);
    Ok(match transform.fit {
        Fit::Contain => {
            let scale = limit((width as f64 / source_width).min(height as f64 / source_height));
            resampled(&cropped, scaled_side(cropped.width, scale), scaled_side(cropped.height, scale))
        }
        Fit::Cover => {
            let scale = limit((width as f64 / source_width).max(height as f64 / source_height));
            let scaled = resampled(&cropped, scaled_side(cropped.width, scale), scaled_side(cropped.height, scale));
            let (kept_width, kept_height) = (width.min(scaled.width), height.min(scaled.height));
            crop(
                &scaled,
                Crop {
                    x: (scaled.width - kept_width) / 2,
                    y: (scaled.height - kept_height) / 2,
                    width: kept_width,
                    height: kept_height,
                },
            )?
        }
        Fit::Fill => {
            let (width, height) = if transform.enlarge {
                (width, height)
            } else {
                (width.min(cropped.width), height.min(cropped.height))
            };
            resampled(&cropped, width, height)
        }
    })
}

/// The PNG or JPEG `bytes` transformed as `transform` says, and the format it is written in.
pub fn transform(bytes: &[u8], transform: &Transform) -> Result<(Vec<u8>, RenditionFormat), ContentError> {
    let format = transform
        .format
        .or_else(|| source_format(bytes))
        .ok_or_else(|| failure("transforms read PNG and JPEG images"))?;
    let image = decode_image(bytes)?;
    Ok((encode_image(&transformed(&image, transform)?, format), format))
}

fn resampled(image: &Image, width: usize, height: usize) -> Image {
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

    #[test]
    fn transforms_crop_then_contain_cover_or_fill_the_box() {
        let image = gradient(40, 20);
        let cropped = crop(&image, Crop { x: 10, y: 5, width: 20, height: 10 }).unwrap();
        assert_eq!((cropped.width, cropped.height), (20, 10));
        assert_eq!(&cropped.rgba[..4], &image.rgba[(5 * 40 + 10) * 4..(5 * 40 + 10) * 4 + 4]);
        assert!(crop(&image, Crop { x: 30, y: 0, width: 20, height: 5 }).is_err());
        let boxed = |fit: Fit, enlarge: bool| {
            let out = transformed(&image, &Transform { width: Some(10), height: Some(10), fit, enlarge, ..Transform::default() }).unwrap();
            (out.width, out.height)
        };
        assert_eq!(boxed(Fit::Contain, false), (10, 5));
        assert_eq!(boxed(Fit::Cover, false), (10, 10));
        assert_eq!(boxed(Fit::Fill, false), (10, 10));
        let wide = transformed(&image, &Transform { width: Some(80), enlarge: true, ..Transform::default() }).unwrap();
        assert_eq!((wide.width, wide.height), (80, 40));
        let kept = transformed(&image, &Transform { width: Some(80), ..Transform::default() }).unwrap();
        assert_eq!((kept.width, kept.height), (40, 20));
    }
}
