use eggdb_content::{Crop, Fit, RenditionFormat, Transform};

/// The query parameter that asks for an object transformed on its way out (D1179).
pub const TRANSFORM_PARAMETER: &str = "x-eggdb-transform";

/// The largest object a transform reads.
pub const MOST_TRANSFORMED_BYTES: u64 = 64 << 20;

/// Runs a named transform of an object's bytes, such as a WebAssembly extension's function,
/// for `x-eggdb-transform=wasm:<name>`; what it returns is sent in place of the object.
pub trait ObjectTransform: Sync {
    /// The object's `bytes`, of `content_type`, after transform `name`.
    fn run(&self, name: &str, bytes: &[u8], content_type: &str) -> Result<Vec<u8>, String>;
}

/// What `x-eggdb-transform` asks for: an image transform, a named one, or both (the named one
/// runs first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Requested {
    pub(crate) image: Option<Transform>,
    pub(crate) named: Option<String>,
}

fn number(key: &str, value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| format!("{TRANSFORM_PARAMETER}: '{key}' takes a whole number, not '{value}'"))
}

/// Reads `x-eggdb-transform`: comma-separated `key:value` items, `w`, `h`, `fit`
/// (`contain`, `cover`, `fill`), `crop` (`x_y_width_height`), `format` (`png`, `jpeg`),
/// `q` (JPEG quality 1 to 100), `enlarge` (`1`) and `wasm` (a transform's name).
pub(crate) fn parse(text: &str) -> Result<Requested, String> {
    let mut image = Transform::default();
    let mut touched = false;
    let mut quality: Option<u8> = None;
    let mut format: Option<&str> = None;
    let mut named = None;
    for item in text.split(',').map(str::trim).filter(|item| !item.is_empty()) {
        let (key, value) = item
            .split_once(':')
            .ok_or_else(|| format!("{TRANSFORM_PARAMETER}: '{item}' is not key:value"))?;
        match key {
            "w" => image.width = Some(number(key, value)?),
            "h" => image.height = Some(number(key, value)?),
            "fit" => {
                image.fit = match value {
                    "contain" => Fit::Contain,
                    "cover" => Fit::Cover,
                    "fill" => Fit::Fill,
                    other => return Err(format!("{TRANSFORM_PARAMETER}: fit is contain, cover or fill, not '{other}'")),
                }
            }
            "crop" => {
                let parts: Vec<&str> = value.split('_').collect();
                let [x, y, width, height] = parts.as_slice() else {
                    return Err(format!("{TRANSFORM_PARAMETER}: crop is x_y_width_height"));
                };
                image.crop = Some(Crop {
                    x: number(key, x)?,
                    y: number(key, y)?,
                    width: number(key, width)?,
                    height: number(key, height)?,
                });
            }
            "format" => format = Some(value),
            "q" => {
                let given = number(key, value)?;
                if !(1..=100).contains(&given) {
                    return Err(format!("{TRANSFORM_PARAMETER}: q is 1 to 100"));
                }
                quality = Some(given as u8);
            }
            "enlarge" => image.enlarge = value == "1" || value == "true",
            "wasm" => {
                if value.is_empty() || !value.chars().all(|character| character.is_ascii_alphanumeric() || character == '_') {
                    return Err(format!("{TRANSFORM_PARAMETER}: wasm names a transform by letters, digits and _"));
                }
                named = Some(value.to_owned());
                continue;
            }
            other => return Err(format!("{TRANSFORM_PARAMETER}: '{other}' is not a transform option")),
        }
        touched = true;
    }
    image.format = match (format, quality) {
        (Some("png"), _) => Some(RenditionFormat::Png),
        (Some("jpeg" | "jpg"), quality) => Some(RenditionFormat::Jpeg(quality.unwrap_or(85))),
        (Some(other), _) => return Err(format!("{TRANSFORM_PARAMETER}: format is png or jpeg, not '{other}'")),
        (None, Some(quality)) => Some(RenditionFormat::Jpeg(quality)),
        (None, None) => None,
    };
    if image.format.is_some() {
        touched = true;
    }
    if !touched && named.is_none() {
        return Err(format!("{TRANSFORM_PARAMETER} asks for nothing"));
    }
    Ok(Requested {
        image: touched.then_some(image),
        named,
    })
}

pub(crate) fn content_type_of(format: RenditionFormat) -> &'static str {
    match format {
        RenditionFormat::Png => "image/png",
        RenditionFormat::Jpeg(_) => "image/jpeg",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_parameters_read_as_their_options() {
        let requested = parse("w:200,h:100,fit:cover,crop:10_20_300_200,format:jpeg,q:70").unwrap();
        let image = requested.image.unwrap();
        assert_eq!((image.width, image.height, image.fit), (Some(200), Some(100), Fit::Cover));
        assert_eq!(image.crop, Some(Crop { x: 10, y: 20, width: 300, height: 200 }));
        assert_eq!(image.format, Some(RenditionFormat::Jpeg(70)));
        assert_eq!(parse("wasm:watermark").unwrap(), Requested { image: None, named: Some("watermark".to_owned()) });
        for bad in ["", "w:x", "fit:stretch", "crop:1_2", "q:0", "wasm:a.b", "size:3"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
