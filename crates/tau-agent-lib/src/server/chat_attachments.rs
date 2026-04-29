//! Validation and `UserMessage` assembly for `Request::Chat` attachments.
//!
//! The protocol carries attachments as `ChatAttachment` variants with
//! base64-encoded payloads. Before the server hands the message off to the
//! engine, every attachment is validated:
//!
//! - The MIME type must be on the [`ALLOWED_IMAGE_MIMES`] allow-list.
//! - The base64 data must decode cleanly.
//! - The decoded byte length must be at most [`MAX_IMAGE_BYTES`].
//!
//! Validation failures produce a human-readable error string that the
//! caller surfaces as `Response::Error { message }` rather than panicking.
//!
//! Once validated, [`build_user_message`] composes a `[Text, Image*]`
//! `UserMessage` (or `[Image*]` if the text body is empty), so all four
//! `Request::Chat` consumers (main dispatch, tool dispatch tunnel, worker
//! self-chat, and the tasks-scheduler bg-chat pump) share one path.
//!
//! See task 904 for the full design.
//!
//! ## Helpers for clients
//!
//! The CLI / TUI use [`encode_file_attachment`] to read an image file from
//! disk, sniff its MIME from the extension, and produce a `ChatAttachment`
//! ready to drop into a `Request::Chat`. Encoding is done client-side so
//! the same validator runs both pre- and post-flight.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use std::path::Path;

use crate::protocol::ChatAttachment;
use crate::types::{Message, TextContent, UserContent, UserMessage, timestamp_ms};

/// Anthropic per-image cap (5 MiB decoded). Applies to OpenAI too.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// MIME types we accept for `ChatAttachment::Image`. Images outside this
/// list are rejected at the entry point with a structured error.
pub const ALLOWED_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Validate a single attachment.
///
/// Returns `Ok(decoded_len)` so callers that already need the byte count
/// (e.g. the TUI's "📎 foo.png (12 KiB)" chip) can avoid decoding twice.
pub fn validate_attachment(att: &ChatAttachment) -> Result<usize, String> {
    match att {
        ChatAttachment::Image { data, mime_type } => {
            if !ALLOWED_IMAGE_MIMES.iter().any(|m| *m == mime_type) {
                return Err(format!(
                    "unsupported image mime type '{}': expected one of {}",
                    mime_type,
                    ALLOWED_IMAGE_MIMES.join(", ")
                ));
            }
            let decoded = STANDARD
                .decode(data.as_bytes())
                .map_err(|e| format!("attachment is not valid base64: {}", e))?;
            if decoded.len() > MAX_IMAGE_BYTES {
                return Err(format!(
                    "image is {} bytes, exceeds {}-byte cap",
                    decoded.len(),
                    MAX_IMAGE_BYTES
                ));
            }
            Ok(decoded.len())
        }
    }
}

/// Build the `UserMessage` that gets persisted + sent to the model from a
/// chat request's text + attachments.
///
/// Validation runs first; a single bad attachment fails the whole call so
/// the user sees a clear error rather than a partial message reaching the
/// model. When `attachments` is empty this collapses to today's behaviour
/// (`UserMessage::text(text)`); when the text body is empty the leading
/// text block is omitted (vision-only message).
pub fn build_user_message(
    text: &str,
    attachments: &[ChatAttachment],
) -> Result<UserMessage, String> {
    for (i, att) in attachments.iter().enumerate() {
        validate_attachment(att).map_err(|e| format!("attachment #{}: {}", i, e))?;
    }

    if attachments.is_empty() {
        return Ok(UserMessage::text(text));
    }

    let mut content: Vec<UserContent> = Vec::with_capacity(attachments.len() + 1);
    if !text.is_empty() {
        content.push(UserContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        }));
    }
    for att in attachments {
        content.push(att.to_user_content());
    }

    Ok(UserMessage {
        content,
        timestamp: timestamp_ms(),
    })
}

/// Wrapper used by the central Chat handler: build a `UserMessage` and
/// wrap it as `Message::User`. Returned `Err(String)` is meant to flow
/// straight into a `Response::Error { message }`.
pub fn build_user_message_for_request(
    text: &str,
    attachments: &[ChatAttachment],
) -> Result<Message, String> {
    build_user_message(text, attachments).map(Message::User)
}

/// Sniff the MIME type of an image file from its extension. Returns
/// `None` for unsupported / missing extensions.
pub fn sniff_image_mime(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())?;
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Read an image file from disk and produce a validated [`ChatAttachment`].
///
/// Used by the TUI's `/attach <path>` slash command and the CLI's
/// `--attach <path>` flag so both paths share the same MIME-sniff +
/// size-cap rules.
pub fn encode_file_attachment(path: &Path) -> Result<ChatAttachment, String> {
    let mime = sniff_image_mime(path).ok_or_else(|| {
        format!(
            "unsupported image extension: expected .png, .jpg, .jpeg, .gif, or .webp ({})",
            path.display()
        )
    })?;
    // Stat first so we can reject oversized files without reading them.
    let meta =
        std::fs::metadata(path).map_err(|e| format!("failed to stat {}: {}", path.display(), e))?;
    if meta.len() as usize > MAX_IMAGE_BYTES {
        return Err(format!(
            "{} is {} bytes, exceeds {}-byte cap",
            path.display(),
            meta.len(),
            MAX_IMAGE_BYTES
        ));
    }
    let bytes =
        std::fs::read(path).map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "{} is {} bytes, exceeds {}-byte cap",
            path.display(),
            bytes.len(),
            MAX_IMAGE_BYTES
        ));
    }
    let data = STANDARD.encode(&bytes);
    let att = ChatAttachment::Image {
        data,
        mime_type: mime.to_string(),
    };
    // Belt-and-braces: re-validate so the post-encode payload obeys the
    // exact same rules as a wire-arrived attachment.
    validate_attachment(&att)
        .map_err(|e| format!("encoded attachment failed validation: {}", e))?;
    Ok(att)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png_bytes() -> Vec<u8> {
        // Smallest possible PNG: 1x1 transparent.
        vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H',
            b'D', b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, b'I', b'D', b'A', b'T', 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    fn tiny_png_attachment() -> ChatAttachment {
        ChatAttachment::Image {
            data: STANDARD.encode(tiny_png_bytes()),
            mime_type: "image/png".into(),
        }
    }

    #[test]
    fn validates_supported_image() {
        let len = validate_attachment(&tiny_png_attachment()).expect("valid");
        assert_eq!(len, tiny_png_bytes().len());
    }

    #[test]
    fn rejects_unsupported_mime() {
        let att = ChatAttachment::Image {
            data: STANDARD.encode(tiny_png_bytes()),
            mime_type: "image/bmp".into(),
        };
        let err = validate_attachment(&att).expect_err("should reject");
        assert!(err.contains("unsupported image mime type"), "got: {err}");
    }

    #[test]
    fn rejects_bad_base64() {
        let att = ChatAttachment::Image {
            data: "not valid base64!!!".into(),
            mime_type: "image/png".into(),
        };
        let err = validate_attachment(&att).expect_err("should reject");
        assert!(err.contains("base64"), "got: {err}");
    }

    #[test]
    fn rejects_oversized_image() {
        // Encode 6 MiB of zeroes — well over the 5 MiB cap.
        let big = vec![0u8; 6 * 1024 * 1024];
        let att = ChatAttachment::Image {
            data: STANDARD.encode(&big),
            mime_type: "image/png".into(),
        };
        let err = validate_attachment(&att).expect_err("should reject");
        assert!(err.contains("exceeds"), "got: {err}");
    }

    #[test]
    fn build_user_message_text_only_collapses_to_text_helper() {
        let msg = build_user_message("hi", &[]).expect("ok");
        assert_eq!(msg.content.len(), 1);
        match &msg.content[0] {
            UserContent::Text(t) => assert_eq!(t.text, "hi"),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn build_user_message_with_image() {
        let msg = build_user_message("describe", &[tiny_png_attachment()]).expect("ok");
        assert_eq!(msg.content.len(), 2);
        assert!(matches!(&msg.content[0], UserContent::Text(t) if t.text == "describe"));
        assert!(matches!(&msg.content[1], UserContent::Image(_)));
    }

    #[test]
    fn build_user_message_image_only_omits_text_block() {
        let msg = build_user_message("", &[tiny_png_attachment()]).expect("ok");
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(&msg.content[0], UserContent::Image(_)));
    }

    #[test]
    fn build_user_message_propagates_validation_error() {
        let bad = ChatAttachment::Image {
            data: "###".into(),
            mime_type: "image/png".into(),
        };
        let err = build_user_message("hi", &[bad]).expect_err("should fail");
        assert!(err.starts_with("attachment #0:"), "got: {err}");
    }

    #[test]
    fn sniff_image_mime_known_extensions() {
        assert_eq!(sniff_image_mime(Path::new("foo.png")), Some("image/png"));
        assert_eq!(sniff_image_mime(Path::new("foo.PNG")), Some("image/png"));
        assert_eq!(sniff_image_mime(Path::new("foo.jpg")), Some("image/jpeg"));
        assert_eq!(sniff_image_mime(Path::new("foo.jpeg")), Some("image/jpeg"));
        assert_eq!(sniff_image_mime(Path::new("foo.gif")), Some("image/gif"));
        assert_eq!(sniff_image_mime(Path::new("foo.webp")), Some("image/webp"));
        assert_eq!(sniff_image_mime(Path::new("foo.bmp")), None);
        assert_eq!(sniff_image_mime(Path::new("foo")), None);
    }

    #[test]
    fn encode_file_attachment_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tiny.png");
        std::fs::write(&path, tiny_png_bytes()).expect("write");
        let att = encode_file_attachment(&path).expect("encode");
        let len = validate_attachment(&att).expect("valid");
        assert_eq!(len, tiny_png_bytes().len());
    }

    #[test]
    fn encode_file_attachment_rejects_unknown_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("foo.bmp");
        std::fs::write(&path, b"x").expect("write");
        let err = encode_file_attachment(&path).expect_err("should reject");
        assert!(err.contains("unsupported image extension"), "got: {err}");
    }
}
