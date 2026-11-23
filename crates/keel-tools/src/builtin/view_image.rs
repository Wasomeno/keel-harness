//! `view_image`: hand a local raster image to the model.
//!
//! The tool reads the file through [`Workspace::open_existing`], so it refuses
//! escapes and symlinks exactly like `read`. It never fetches a URL and never
//! decodes or resizes the image; the 2 MiB cap is the whole size policy. The
//! run loop sends the returned attachment in a separate user message because
//! OpenAI Chat tool messages cannot carry an image.

use std::io::Read;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{
    ImageAttachment, Tool, ToolContext, ToolError, ToolFuture, ToolOutcome, ToolSpec,
    parse_arguments, tool_spec,
};

/// The only size policy: a file larger than this is refused before decoding.
pub const MAX_IMAGE_BYTES: u64 = 2 * 1024 * 1024;

pub struct ViewImageTool;

#[derive(Deserialize)]
struct ViewImageArguments {
    /// Path relative to the workspace root.
    path: String,
    /// Provider hint. Protocols without a detail field ignore it.
    #[serde(default)]
    detail: Detail,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Detail {
    Low,
    #[default]
    High,
}

impl Detail {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
        }
    }
}

/// A raster format the tool accepts. Detection is by magic bytes only; the
/// extension never decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Raster {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl Raster {
    const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

/// What the first bytes of the file look like. Non-raster variants exist so a
/// rejection can name the detected type instead of saying "not an image".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Detected {
    Raster(Raster),
    Svg,
    Pdf,
    Zip,
    Gzip,
    Bmp,
    Tiff,
    Ico,
    Text,
    Binary,
}

impl Detected {
    const fn label(self) -> &'static str {
        match self {
            Self::Raster(Raster::Png) => "PNG",
            Self::Raster(Raster::Jpeg) => "JPEG",
            Self::Raster(Raster::Gif) => "GIF",
            Self::Raster(Raster::Webp) => "WEBP",
            Self::Svg => "SVG",
            Self::Pdf => "PDF",
            Self::Zip => "ZIP",
            Self::Gzip => "gzip",
            Self::Bmp => "BMP",
            Self::Tiff => "TIFF",
            Self::Ico => "ICO",
            Self::Text => "a text file",
            Self::Binary => "unrecognised binary data",
        }
    }
}

impl Tool for ViewImageTool {
    fn name(&self) -> &str {
        "view_image"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "view_image",
            "Use when: you need to see a local raster image, such as a rendered screenshot, a diagram export, or an image the task refers to. The image reaches you as a separate user message right after this tool result.\n\
             Prefer: rendering the image to a file with `bash` first (for example a screenshot or a plot); `read` cannot show images and refuses binary files.\n\
             Returns: one line, `viewed <path> (image/png, 184 KiB, 1200x800)`, plus the image itself. Dimensions are omitted when the header does not carry them. Accepts PNG, JPEG, WEBP and GIF, detected by magic bytes; `detail` (`low` or `high`, default `high`) is a provider hint and is ignored where the protocol has no field for it.\n\
             On failure: says whether the path escaped the workspace (and names the root), was a symlink, was missing, is a directory, was not a supported raster image (naming the detected type; SVG carries a rasterise hint), or exceeded the 2 MiB cap. It never fetches URLs.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "detail": {
                        "type": "string",
                        "enum": ["low", "high"],
                        "description": "Provider detail hint; default high."
                    }
                },
                "required": ["path"]
            }),
        )
    }

    fn summarize(&self, arguments: &Value) -> String {
        format!(
            "view_image {}",
            arguments.get("path").and_then(Value::as_str).unwrap_or("?")
        )
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn read_only(&self) -> bool {
        true
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            match view(arguments, context).await {
                Ok(outcome) => outcome,
                Err(error) => error.into_outcome(),
            }
        })
    }
}

async fn view(arguments: Value, context: &ToolContext) -> Result<ToolOutcome, ToolError> {
    let arguments: ViewImageArguments = parse_arguments(arguments)?;
    let workspace = context.workspace.clone();
    let requested = arguments.path.clone();
    if requested.trim().is_empty() {
        return Err(ToolError::InvalidArguments(
            "`path` must not be empty; pass the workspace-relative path of an image".to_owned(),
        ));
    }
    let name = workspace.display_path(&requested);
    let read_name = name.clone();

    let bytes = tokio::task::spawn_blocking(move || {
        let mut file = workspace.open_existing(&requested)?;
        let metadata = file.metadata().map_err(|error| {
            ToolError::Io(format!(
                "could not inspect `{read_name}`: {error}; check that it is a readable image file"
            ))
        })?;
        if metadata.is_dir() {
            return Err(ToolError::Message(format!(
                "`{read_name}` is a directory; use `glob` to find the image file, then view it"
            )));
        }
        if metadata.len() > MAX_IMAGE_BYTES {
            return Err(too_large(&read_name, metadata.len()));
        }
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_IMAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                ToolError::Io(format!(
                    "could not read `{read_name}`: {error}; check that it is a readable image file"
                ))
            })?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(too_large(&read_name, bytes.len() as u64));
        }
        Ok::<_, ToolError>(bytes)
    })
    .await
    .map_err(|error| {
        ToolError::Io(format!(
            "could not read `{name}`: the blocking reader failed: {error}"
        ))
    })??;

    let raster = match detect(&bytes) {
        Detected::Raster(raster) => raster,
        detected => return Err(unsupported(&name, detected)),
    };

    let mime = raster.mime();
    let size = human_size(bytes.len() as u64);
    let dimensions = dimensions(raster, &bytes)
        .map(|(width, height)| format!(", {width}x{height}"))
        .unwrap_or_default();
    let caption = format!("image from view_image: {name}");
    let attachment = ImageAttachment {
        mime: mime.to_owned(),
        data: base64(&bytes),
        caption,
        detail: Some(arguments.detail.as_str().to_owned()),
    };
    Ok(
        ToolOutcome::ok(format!("viewed {name} ({mime}, {size}{dimensions})"))
            .with_image(attachment),
    )
}

fn too_large(name: &str, bytes: u64) -> ToolError {
    ToolError::Message(format!(
        "`{name}` is {}, over the 2 MiB view_image cap; render or resize it smaller, then try again",
        human_size(bytes)
    ))
}

fn unsupported(name: &str, detected: Detected) -> ToolError {
    match detected {
        Detected::Svg => ToolError::Message(format!(
            "`{name}` is SVG, which is not a raster image; rasterise it first, for example \
             `qlmanage -t -s 1200 -o /tmp {name}` on macOS or \
             `rsvg-convert {name} -o image.png`, then view the PNG"
        )),
        Detected::Text => ToolError::Message(format!(
            "`{name}` is a text file, not an image; view_image accepts PNG, JPEG, WEBP and GIF \
             detected by magic bytes. Use `read` for text"
        )),
        detected => ToolError::Message(format!(
            "`{name}` is {}, not a supported raster image; view_image accepts PNG, JPEG, WEBP \
             and GIF detected by magic bytes",
            detected.label()
        )),
    }
}

fn detect(bytes: &[u8]) -> Detected {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Detected::Raster(Raster::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Detected::Raster(Raster::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Detected::Raster(Raster::Gif)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Detected::Raster(Raster::Webp)
    } else if bytes.starts_with(b"%PDF-") {
        Detected::Pdf
    } else if bytes.starts_with(b"PK\x03\x04") {
        Detected::Zip
    } else if bytes.starts_with(&[0x1F, 0x8B]) {
        Detected::Gzip
    } else if bytes.starts_with(b"BM") {
        Detected::Bmp
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        Detected::Tiff
    } else if bytes.starts_with(&[0, 0, 1, 0]) {
        Detected::Ico
    } else if looks_like_svg(bytes) {
        Detected::Svg
    } else if looks_like_text(bytes) {
        Detected::Text
    } else {
        Detected::Binary
    }
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    let trimmed = head.trim_start_matches('\u{feff}').trim_start();
    trimmed.starts_with('<') && trimmed.to_ascii_lowercase().contains("<svg")
}

fn looks_like_text(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(8192)];
    !sample.contains(&0) && std::str::from_utf8(sample).is_ok()
}

/// Reads width and height from the file header. A malformed header simply
/// omits the dimensions; it does not fail the call.
fn dimensions(raster: Raster, bytes: &[u8]) -> Option<(u32, u32)> {
    let (width, height) = match raster {
        Raster::Png => {
            if bytes.get(12..16)? != &b"IHDR"[..] {
                return None;
            }
            (
                u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?),
                u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?),
            )
        }
        Raster::Gif => (
            u32::from(u16::from_le_bytes(bytes.get(6..8)?.try_into().ok()?)),
            u32::from(u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?)),
        ),
        Raster::Jpeg => jpeg_dimensions(bytes)?,
        Raster::Webp => webp_dimensions(bytes)?,
    };
    (width > 0 && height > 0).then_some((width, height))
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut cursor = 2;
    while cursor + 1 < bytes.len() {
        if bytes[cursor] != 0xFF {
            cursor += 1;
            continue;
        }
        let mut marker = bytes[cursor + 1];
        while marker == 0xFF {
            cursor += 1;
            marker = *bytes.get(cursor + 1)?;
        }
        cursor += 2;
        if matches!(marker, 0x01 | 0xD0..=0xD7) {
            continue;
        }
        if matches!(marker, 0xD9 | 0xDA) {
            return None;
        }
        let length = usize::from(u16::from_be_bytes(
            bytes.get(cursor..cursor + 2)?.try_into().ok()?,
        ));
        if length < 2 {
            return None;
        }
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let height = u16::from_be_bytes(bytes.get(cursor + 3..cursor + 5)?.try_into().ok()?);
            let width = u16::from_be_bytes(bytes.get(cursor + 5..cursor + 7)?.try_into().ok()?);
            return Some((u32::from(width), u32::from(height)));
        }
        cursor += length;
    }
    None
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let chunk = bytes.get(12..16)?;
    let payload = bytes.get(20..)?;
    match chunk {
        b"VP8X" => {
            let width =
                1 + u32::from_le_bytes([*payload.get(4)?, *payload.get(5)?, *payload.get(6)?, 0]);
            let height =
                1 + u32::from_le_bytes([*payload.get(7)?, *payload.get(8)?, *payload.get(9)?, 0]);
            Some((width, height))
        }
        b"VP8L" => {
            if *payload.first()? != 0x2F {
                return None;
            }
            let bits = u32::from_le_bytes([
                *payload.get(1)?,
                *payload.get(2)?,
                *payload.get(3)?,
                *payload.get(4)?,
            ]);
            Some((1 + (bits & 0x3FFF), 1 + ((bits >> 14) & 0x3FFF)))
        }
        b"VP8 " => {
            if payload.get(3..6)? != &[0x9D, 0x01, 0x2A][..] {
                return None;
            }
            let width =
                u32::from(u16::from_le_bytes([*payload.get(6)?, *payload.get(7)?]) & 0x3FFF);
            let height =
                u32::from(u16::from_le_bytes([*payload.get(8)?, *payload.get(9)?]) & 0x3FFF);
            Some((width, height))
        }
        _ => None,
    }
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{} KiB", bytes.div_ceil(1024))
    }
}

/// Standard base64 with padding. The crate has no base64 dependency, and the
/// payload is small enough that a direct encoder is the whole cost.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[usize::from(first >> 2)] as char);
        output.push(TABLE[usize::from((first & 0x03) << 4 | second >> 4)] as char);
        if chunk.len() > 1 {
            output.push(TABLE[usize::from((second & 0x0F) << 2 | third >> 6)] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[usize::from(third & 0x3F)] as char);
        } else {
            output.push('=');
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"\x89PNG\r\n\x1a\n"), "iVBORw0KGgo=");
    }

    #[test]
    fn magic_bytes_decide_the_format() {
        assert_eq!(
            detect(b"\x89PNG\r\n\x1a\nrest"),
            Detected::Raster(Raster::Png)
        );
        assert_eq!(
            detect(b"\xFF\xD8\xFF\xE0rest"),
            Detected::Raster(Raster::Jpeg)
        );
        assert_eq!(
            detect(b"GIF89a\x01\x00\x01\x00"),
            Detected::Raster(Raster::Gif)
        );
        assert_eq!(
            detect(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Detected::Raster(Raster::Webp)
        );
        assert_eq!(detect(b"<svg xmlns=\"x\"></svg>"), Detected::Svg);
        assert_eq!(
            detect(b"<?xml version=\"1.0\"?>\n<svg></svg>"),
            Detected::Svg
        );
        assert_eq!(detect(b"%PDF-1.7"), Detected::Pdf);
        assert_eq!(detect(b"hello\n"), Detected::Text);
    }

    #[test]
    fn human_size_uses_kib_above_one_kib() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1 KiB");
        assert_eq!(human_size(184 * 1024 + 1), "185 KiB");
    }

    #[test]
    fn a_png_header_yields_dimensions() {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&1200u32.to_be_bytes());
        bytes.extend_from_slice(&800u32.to_be_bytes());
        assert_eq!(dimensions(Raster::Png, &bytes), Some((1200, 800)));
    }
}
