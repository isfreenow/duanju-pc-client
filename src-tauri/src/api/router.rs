//! Extraction helpers for the website's server-rendered pages.
//!
//! `hongguoduanju.com` ships its catalogue inside a `window._ROUTER_DATA` JSON
//! assignment and, for the ranking pages, inside an HTML-encoded `data-fn-args`
//! attribute of a streamed `mergeLoaderData` script. Both are read as data only —
//! no script from the site is ever executed.

use serde_json::Value;

/// `readRouterData` — brace-match the JSON object assigned to `_ROUTER_DATA`.
///
/// The scanner tracks quotes and escapes exactly like the reference so a `}`
/// inside a string cannot end the object early.
pub fn read_router_data(html: &str) -> Result<Value, String> {
    let bytes = html.as_bytes();
    let needle = b"_ROUTER_DATA";
    let mut from = 0usize;
    let start: usize = loop {
        let at = find(bytes, needle, from).ok_or("来源页面未返回目录数据，请重试或打开来源页面")?;
        let mut i = at + needle.len();
        while i < bytes.len()
            && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n' || bytes[i] == b'\r')
        {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len()
                && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n' || bytes[i] == b'\r')
            {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'{' {
                break i;
            }
        }
        from = at + needle.len();
    };

    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().skip(start) {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
        } else if *byte == b'"' {
            quoted = true;
        } else if *byte == b'{' {
            depth += 1;
        } else if *byte == b'}' {
            depth -= 1;
            if depth == 0 {
                return serde_json::from_slice(&bytes[start..=index])
                    .map_err(|error| format!("来源目录数据无法解析：{error}"));
            }
        }
    }
    Err("来源目录数据不完整，请重试".to_string())
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() + from {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// 从 `marker`（某个 `<script>` 属性里的字符串）之后取出该脚本的正文。
///
/// 站点把页面状态放进 `<script id="__NEXT_DATA__">{…}</script>`，正文本身是合法
/// JSON，所以这里只做「找标签结尾 → 找 `</script`」两步，并保证切出来的区间是
/// 完整的 UTF-8 边界（脚本正文永远是 ASCII 定界的 JSON，`get` 只是防御性检查）。
pub fn read_json_between(html: &str, marker: usize) -> Option<&str> {
    let bytes = html.as_bytes();
    let open = find(bytes, b">", marker)?;
    let body = open + 1;
    let close = find(bytes, b"</script", body)?;
    html.get(body..close)
}

/// `decodeHtml` — the five named entities plus numeric ones, used by the
/// ranking page where the payload arrives HTML-escaped.
pub fn decode_html(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'&' {
            // Copy whole characters, not bytes, so multibyte text survives.
            let length = char_len(bytes[index]);
            out.push_str(&value[index..(index + length).min(value.len())]);
            index += length;
            continue;
        }
        let Some(end) = find(bytes, b";", index) else {
            out.push('&');
            index += 1;
            continue;
        };
        let key = &value[index + 1..end];
        let decoded = match key {
            "quot" => Some("\"".to_string()),
            "apos" => Some("'".to_string()),
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            _ if key.starts_with('#') => {
                let code = if key[1..].starts_with('x') || key[1..].starts_with('X') {
                    u32::from_str_radix(&key[2..], 16).ok()
                } else {
                    key[1..].parse::<u32>().ok()
                };
                code.filter(|value| *value > 0 && *value <= 0x10_ff_ff)
                    .and_then(char::from_u32)
                    .map(|ch| ch.to_string())
            }
            _ => None,
        };
        match decoded {
            Some(text) => {
                out.push_str(&text);
                index = end + 1;
            }
            None => {
                out.push('&');
                index += 1;
            }
        }
    }
    out
}

/// Byte length of the UTF-8 character starting at `first`, so a scanner can copy
/// whole characters without re-parsing.
pub fn char_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// `text(value)` from the ranking parser: every tag becomes a single space, then
/// entities decode, then the result is trimmed — exactly the reference's steps,
/// including that inner whitespace runs are kept.
pub fn visible_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut inside = false;
    for ch in html.chars() {
        match ch {
            '<' => inside = true,
            '>' => {
                inside = false;
                out.push(' ');
            }
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    decode_html(&out).trim().to_string()
}

/// `str(value)` in the reference: `String(value ?? fallback).trim()`.
pub fn trimmed(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.trim().to_string(),
        Some(other) => other.to_string().trim().to_string(),
    }
}

/// `Number(value)` style parsing that accepts both JSON numbers and digit strings.
pub fn as_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

pub fn as_i64(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_data_scans_past_braces_inside_strings() {
        let html = r#"window._ROUTER_DATA = {"a":"} not the end {","b":{"c":[1,2]}}"#;
        let value = read_router_data(html).unwrap();
        assert_eq!(value["b"]["c"][1].as_u64(), Some(2));
    }

    #[test]
    fn entities_decode() {
        // Only the reference's five named entities plus numeric ones decode, and every
        // match needs its trailing semicolon: `&nbsp;` stays literal, `&#x4e2d` too.
        assert_eq!(
            decode_html("a&nbsp;&amp;b&#39;c&#x4e2d;d&#101;"),
            "a&nbsp;&b'c中de"
        );
        assert_eq!(decode_html("&#x4e2d"), "&#x4e2d");
        assert_eq!(decode_html("&#9999999999;"), "&#9999999999;");
        assert_eq!(
            visible_text("<h2 id=\"rank-title-3\">  热&#x64ad;  榜  </h2>"),
            "热播  榜"
        );
    }

    #[test]
    fn missing_router_data_reports_the_reference_message() {
        assert!(
            read_router_data("<html>nothing</html>")
                .unwrap_err()
                .contains("未返回目录数据")
        );
    }
}
