//! `file://` URI ↔ filesystem path conversion.
//!
//! LSP identifies documents by URI; Aether tracks buffers by canonical `PathBuf`. We percent-encode
//! on the way out and decode on the way in. Paths are assumed absolute (buffer canonical paths
//! always are), so `file://` + `/abs/path` yields the conventional `file:///abs/path`.

use std::path::PathBuf;

/// Bytes left unescaped: RFC 3986 unreserved set plus `/` (path separators stay literal).
fn is_safe(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/')
}

/// Encode an absolute filesystem path as a `file://` URI.
pub fn path_to_uri(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::from("file://");
    for &b in s.as_bytes() {
        if is_safe(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0xf));
        }
    }
    out
}

/// Decode a `file://` URI back to a path. Returns `None` if it isn't a `file:` URI or contains a
/// malformed percent-escape.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // An optional authority (e.g. `file://host/path`) ends at the first `/`; for local files it's
    // empty (`file:///path`), so the path is everything from the first `/`.
    let path_part = match rest.find('/') {
        Some(i) => &rest[i..],
        None => rest,
    };
    let bytes = percent_decode(path_part)?;
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = from_hex(*bytes.get(i + 1)?)?;
            let lo = from_hex(*bytes.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

fn hex_digit(n: u8) -> char {
    char::from_digit(n as u32, 16).unwrap().to_ascii_uppercase()
}

fn from_hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn absolute_path_gets_three_slashes() {
        assert_eq!(
            path_to_uri(Path::new("/home/joe/x.rs")),
            "file:///home/joe/x.rs"
        );
    }

    #[test]
    fn spaces_and_specials_are_encoded() {
        assert_eq!(path_to_uri(Path::new("/a b/c#d")), "file:///a%20b/c%23d");
    }

    #[test]
    fn round_trips() {
        for p in ["/home/joe/main.rs", "/tmp/a b/π/x#1.go", "/x/c++/y.cpp"] {
            let uri = path_to_uri(Path::new(p));
            assert_eq!(
                uri_to_path(&uri).unwrap(),
                PathBuf::from(p),
                "uri was {uri}"
            );
        }
    }

    #[test]
    fn decodes_conventional_uri() {
        assert_eq!(
            uri_to_path("file:///home/joe/main.rs").unwrap(),
            PathBuf::from("/home/joe/main.rs")
        );
    }

    #[test]
    fn rejects_non_file_uri() {
        assert!(uri_to_path("http://example.com").is_none());
    }

    #[test]
    fn rejects_bad_escape() {
        assert!(uri_to_path("file:///a%2").is_none());
        assert!(uri_to_path("file:///a%zz").is_none());
    }
}
