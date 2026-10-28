//! Query-string encoding that matches the reference's `pythonUrlEncode`.
//!
//! The signature is computed over the query bytes, so the encoder has to be
//! byte-identical to JavaScript's `encodeURIComponent` plus the reference's
//! follow-up escaping of `!'()`. Python's `urllib.parse.quote` behaves the same
//! way for these characters, which is why the reference calls it that.

const JS_UNRESERVED: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";

/// `encodeURIComponent` equivalent: everything outside the JS unreserved set is
/// percent-encoded from its UTF-8 bytes, uppercase hex.
fn js_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if JS_UNRESERVED.contains(byte) {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// The reference's post-pass: `/[!'()]/g` becomes `%XX` uppercase, while `~` and
/// `*` are deliberately left alone.
fn escape_reserved(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '!' | '\'' | '(' | ')' => format!("%{:02X}", ch as u32),
            other => other.to_string(),
        })
        .collect()
}

fn encode_pair(key: &str, value: &str) -> String {
    escape_reserved(&js_encode(key)) + "=" + &escape_reserved(&js_encode(value))
}

/// Encode an ordered parameter list. Order is significant: the reference builds
/// `{...device, ts, _rticket}` with object spread, which preserves insertion
/// order for string keys.
pub fn python_url_encode(values: &[(&str, String)]) -> String {
    values
        .iter()
        .map(|(key, value)| encode_pair(key, value))
        .collect::<Vec<_>>()
        .join("&")
}

/// Convenience for the literal `null`/`undefined` → empty string rule in the
/// reference (`String(item == null ? '' : item)`).
pub fn owned(values: &[(&'static str, Option<&str>)]) -> Vec<(&'static str, String)> {
    values
        .iter()
        .map(|(key, value)| (*key, value.unwrap_or("").to_string()))
        .collect()
}
