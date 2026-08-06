//! Minimal `multipart/form-data` field access.
//!
//! The audio endpoints (`/audio/transcriptions`, `/audio/translations`) are
//! multipart, not JSON. Routing needs exactly one thing out of such a body —
//! the `model` field — and every other byte, the audio payload above all, has
//! to reach the upstream untouched.
//!
//! So this is a field *locator*, not a parser. It returns the byte range of one
//! field's value and never materialises the rest; the common path forwards the
//! original `Bytes` unchanged, and only an alias pays for splicing a new model
//! name into that range.

use bytes::{Bytes, BytesMut};
use std::ops::Range;

/// Boundary token from a `Content-Type`, or `None` if this is not multipart.
pub fn boundary(content_type: &str) -> Option<String> {
    let (kind, params) = content_type.split_once(';')?;
    if !kind.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    for param in params.split(';') {
        let (key, value) = param.split_once('=')?;
        if key.trim().eq_ignore_ascii_case("boundary") {
            let value = value.trim().trim_matches('"');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Byte range of the value of the form field named `name`.
///
/// Scans parts in order and stops at the first match, so a `model` field placed
/// before the file part — where every OpenAI client puts it — costs a scan of a
/// few hundred bytes rather than of the upload.
pub fn find_field(body: &[u8], boundary: &str, name: &str) -> Option<Range<usize>> {
    let opening = format!("--{boundary}");
    let separator = format!("\r\n--{boundary}");

    let mut pos = find(body, opening.as_bytes())? + opening.len();
    loop {
        let rest = body.get(pos..)?;
        // A trailing `--` is the closing boundary: no more parts.
        if rest.starts_with(b"--") || !rest.starts_with(b"\r\n") {
            return None;
        }
        let headers_start = pos + 2;
        let headers_end = find(body.get(headers_start..)?, b"\r\n\r\n")? + headers_start;
        let value_start = headers_end + 4;
        let value_end = find(body.get(value_start..)?, separator.as_bytes())? + value_start;

        if part_is_named(&body[headers_start..headers_end], name) {
            return Some(value_start..value_end);
        }
        pos = value_end + separator.len();
    }
}

/// The field value as a string, trimmed. `None` if it is not valid UTF-8.
pub fn field_value(body: &[u8], range: Range<usize>) -> Option<&str> {
    std::str::from_utf8(body.get(range)?).ok().map(str::trim)
}

/// Rebuild the body with `value` spliced over `range`.
pub fn replace_range(body: &Bytes, range: Range<usize>, value: &str) -> Bytes {
    let mut out = BytesMut::with_capacity(body.len() + value.len());
    out.extend_from_slice(&body[..range.start]);
    out.extend_from_slice(value.as_bytes());
    out.extend_from_slice(&body[range.end..]);
    out.freeze()
}

/// Does this part's `Content-Disposition` carry `name="<name>"`?
fn part_is_named(headers: &[u8], name: &str) -> bool {
    let Ok(text) = std::str::from_utf8(headers) else {
        return false;
    };
    for line in text.split("\r\n") {
        let Some((key, params)) = line.split_once(':') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("content-disposition") {
            continue;
        }
        for param in params.split(';') {
            let Some((key, value)) = param.split_once('=') else {
                continue;
            };
            if key.trim().eq_ignore_ascii_case("name") {
                return value.trim().trim_matches('"') == name;
            }
        }
    }
    false
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: &str = "----abc123";

    fn form() -> Bytes {
        Bytes::from(
            "------abc123\r\n\
             Content-Disposition: form-data; name=\"model\"\r\n\r\n\
             whisper-1\r\n\
             ------abc123\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n\
             RIFF\x00\x00binary\r\n\
             ------abc123--\r\n",
        )
    }

    #[test]
    fn extracts_the_boundary() {
        assert_eq!(
            boundary("multipart/form-data; boundary=----abc123").as_deref(),
            Some("----abc123")
        );
        // Quoted, oddly cased, and with a charset param in the way.
        assert_eq!(
            boundary("Multipart/Form-Data; charset=utf-8; BOUNDARY=\"xy\"").as_deref(),
            Some("xy")
        );
        assert_eq!(boundary("application/json"), None);
        assert_eq!(boundary("multipart/form-data"), None);
    }

    #[test]
    fn finds_a_field_before_the_file_part() {
        let body = form();
        let range = find_field(&body, B, "model").unwrap();
        assert_eq!(field_value(&body, range), Some("whisper-1"));
    }

    #[test]
    fn finds_a_field_after_the_file_part() {
        let body = Bytes::from(
            "------abc123\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\r\n\
             RIFFdata\r\n\
             ------abc123\r\n\
             Content-Disposition: form-data; name=\"model\"\r\n\r\n\
             whisper-1\r\n\
             ------abc123--\r\n",
        );
        let range = find_field(&body, B, "model").unwrap();
        assert_eq!(field_value(&body, range), Some("whisper-1"));
    }

    #[test]
    fn missing_field_is_none() {
        assert_eq!(find_field(&form(), B, "temperature"), None);
    }

    #[test]
    fn filename_does_not_masquerade_as_the_name() {
        // `filename="model"` must not be mistaken for `name="model"`.
        let body = Bytes::from(
            "------abc123\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"model\"\r\n\r\n\
             data\r\n\
             ------abc123--\r\n",
        );
        assert_eq!(find_field(&body, B, "model"), None);
    }

    #[test]
    fn replacing_the_model_leaves_the_upload_intact() {
        let body = form();
        let range = find_field(&body, B, "model").unwrap();
        let out = replace_range(&body, range, "Systran/faster-whisper-large-v3");

        let range = find_field(&out, B, "model").unwrap();
        assert_eq!(
            field_value(&out, range),
            Some("Systran/faster-whisper-large-v3")
        );
        // The binary part survives byte for byte, NUL and all.
        assert_eq!(
            find_field(&out, B, "file").map(|r| out.slice(r)),
            Some(Bytes::from_static(b"RIFF\x00\x00binary"))
        );
    }

    #[test]
    fn a_truncated_body_does_not_panic() {
        assert_eq!(find_field(b"------abc123\r\nContent-Dis", B, "model"), None);
        assert_eq!(find_field(b"", B, "model"), None);
        assert_eq!(find_field(b"not multipart at all", B, "model"), None);
    }
}
