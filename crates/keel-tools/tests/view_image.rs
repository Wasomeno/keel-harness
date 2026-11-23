//! `view_image` against real files. Detection is by magic bytes, so the
//! accepted fixtures deliberately do not use their real extensions.

use std::path::Path;

use keel_tools::{ToolContext, ToolLimits, ToolOutcome, Workspace, builtin};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const PNG: &[u8] = include_bytes!("fixtures/view_image/tiny.png");
const JPEG: &[u8] = include_bytes!("fixtures/view_image/tiny.jpg");
const GIF: &[u8] = include_bytes!("fixtures/view_image/tiny.gif");
const WEBP: &[u8] = include_bytes!("fixtures/view_image/tiny.webp");

fn context(root: &Path) -> ToolContext {
    ToolContext {
        workspace: Workspace::open(root).expect("workspace opens"),
        cancel: CancellationToken::new(),
        limits: ToolLimits::default(),
        session_state: None,
        sandbox: false,
    }
}

async fn view(root: &Path, arguments: Value) -> ToolOutcome {
    let registry = builtin::registry();
    registry
        .get("view_image")
        .expect("view_image is registered by default")
        .execute(arguments, &context(root))
        .await
}

#[tokio::test]
async fn png_jpeg_gif_and_webp_are_accepted_by_magic_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let cases = [
        ("sample.txt", PNG, "image/png", "iVBOR"),
        ("photo.bin", JPEG, "image/jpeg", "/9j/"),
        ("animation.dat", GIF, "image/gif", "R0lGOD"),
        ("picture.md", WEBP, "image/webp", "UklGR"),
    ];

    for (name, bytes, mime, prefix) in cases {
        std::fs::write(directory.path().join(name), bytes).unwrap();
        let outcome = view(directory.path(), json!({"path": name})).await;

        assert!(!outcome.is_error, "{name}: {}", outcome.text);
        assert_eq!(
            outcome.text,
            format!("viewed {name} ({mime}, {} B, 1x1)", bytes.len()),
            "{name}"
        );
        assert_eq!(outcome.images.len(), 1, "{name}");
        let image = &outcome.images[0];
        assert_eq!(image.mime, mime, "{name}");
        assert_eq!(image.caption, format!("image from view_image: {name}"));
        assert_eq!(image.detail.as_deref(), Some("high"));
        assert!(
            image.data.starts_with(prefix),
            "{name}: payload is base64 of the file: {}",
            image.data
        );
    }
}

#[tokio::test]
async fn detail_is_optional_and_low_is_forwarded() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tiny.png"), PNG).unwrap();

    let outcome = view(
        directory.path(),
        json!({"path": "tiny.png", "detail": "low"}),
    )
    .await;
    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(outcome.images[0].detail.as_deref(), Some("low"));

    let outcome = view(
        directory.path(),
        json!({"path": "tiny.png", "detail": "medium"}),
    )
    .await;
    assert!(outcome.is_error);
    assert!(
        outcome.text.contains("invalid arguments") && outcome.text.contains("medium"),
        "{}",
        outcome.text
    );
    assert!(outcome.images.is_empty());
}

#[tokio::test]
async fn an_svg_is_refused_with_a_rasterise_hint() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("diagram.svg"),
        r#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1"/></svg>"#,
    )
    .unwrap();

    let outcome = view(directory.path(), json!({"path": "diagram.svg"})).await;

    assert!(outcome.is_error);
    assert!(outcome.images.is_empty());
    assert!(outcome.text.contains("SVG"), "{}", outcome.text);
    assert!(
        outcome.text.contains("not a raster image"),
        "{}",
        outcome.text
    );
    assert!(outcome.text.contains("qlmanage"), "{}", outcome.text);
    assert!(outcome.text.contains("rsvg-convert"), "{}", outcome.text);
}

#[tokio::test]
async fn a_text_file_is_refused_and_named() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.png"), "plain text\n").unwrap();

    let outcome = view(directory.path(), json!({"path": "notes.png"})).await;

    assert!(outcome.is_error);
    assert!(outcome.images.is_empty());
    assert!(outcome.text.contains("a text file"), "{}", outcome.text);
    assert!(
        outcome.text.contains("PNG, JPEG, WEBP and GIF"),
        "{}",
        outcome.text
    );
}

#[tokio::test]
async fn a_pdf_is_refused_and_named() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("report.png"), b"%PDF-1.7\n").unwrap();

    let outcome = view(directory.path(), json!({"path": "report.png"})).await;

    assert!(outcome.is_error);
    assert!(outcome.text.contains("PDF"), "{}", outcome.text);
}

#[tokio::test]
async fn a_file_over_two_mib_is_refused_and_names_the_cap() {
    let directory = tempfile::tempdir().unwrap();
    let mut bytes = PNG.to_vec();
    bytes.resize(2 * 1024 * 1024 + 1, 0);
    std::fs::write(directory.path().join("huge.png"), bytes).unwrap();

    let outcome = view(directory.path(), json!({"path": "huge.png"})).await;

    assert!(outcome.is_error);
    assert!(outcome.images.is_empty());
    assert!(outcome.text.contains("2 MiB"), "{}", outcome.text);
    assert!(outcome.text.contains("view_image cap"), "{}", outcome.text);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tiny.png"), PNG).unwrap();
    std::os::unix::fs::symlink("tiny.png", directory.path().join("link.png")).unwrap();

    let outcome = view(directory.path(), json!({"path": "link.png"})).await;

    assert!(outcome.is_error);
    assert!(outcome.images.is_empty());
    assert!(outcome.text.contains("symbolic link"), "{}", outcome.text);
    assert!(
        outcome.text.contains("symlinks are not followed"),
        "{}",
        outcome.text
    );
}

#[tokio::test]
async fn a_path_escaping_the_workspace_is_refused_and_names_the_root() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("outside.png"), PNG).unwrap();

    let outcome = view(directory.path(), json!({"path": "../outside.png"})).await;

    assert!(outcome.is_error);
    assert!(outcome.images.is_empty());
    assert!(
        outcome.text.contains("escapes the workspace root"),
        "{}",
        outcome.text
    );
    assert!(
        outcome
            .text
            .contains(&directory.path().display().to_string()),
        "the root is named: {}",
        outcome.text
    );
}

#[tokio::test]
async fn a_missing_file_is_reported() {
    let directory = tempfile::tempdir().unwrap();

    let outcome = view(directory.path(), json!({"path": "absent.png"})).await;

    assert!(outcome.is_error);
    assert!(outcome.images.is_empty());
    assert!(
        outcome.text.contains("path not found: `absent.png`"),
        "{}",
        outcome.text
    );
    assert!(outcome.text.contains("`glob`"), "{}", outcome.text);
}

#[tokio::test]
async fn a_directory_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("images")).unwrap();

    let outcome = view(directory.path(), json!({"path": "images"})).await;

    assert!(outcome.is_error);
    assert!(outcome.text.contains("is a directory"), "{}", outcome.text);
    assert!(outcome.text.contains("`glob`"), "{}", outcome.text);
}
