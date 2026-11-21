//! Path handling for the DAV tree: decode once, normalise to NFC, reject traversal.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use unicode_normalization::UnicodeNormalization;

/// Like SabreDAV's `encodePath`: everything except letters, digits and `_ - . ~ ( ) : @` is
/// percent-encoded (per segment), so hrefs look exactly like stock Nextcloud's.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'(')
    .remove(b')')
    .remove(b':')
    .remove(b'@');

/// One path segment, encoded for use in an href.
pub fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, SEGMENT).to_string()
}

#[derive(Debug, PartialEq, Eq)]
pub struct InvalidPath;

/// `raw` is the still percent-encoded path below the user root. Returns the canonical path
/// (`""` = root, `a/b`, no leading/trailing slash). Empty segments are collapsed; `.`/`..`,
/// NUL and invalid UTF-8 are rejected. Names are NFC-normalised so macOS (NFD) names match.
pub fn normalize(raw: &str) -> Result<String, InvalidPath> {
    let decoded = percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| InvalidPath)?;
    let mut segs = Vec::new();
    for s in decoded.split('/') {
        if s.is_empty() {
            continue;
        }
        if s == "." || s == ".." || s.contains('\0') {
            return Err(InvalidPath);
        }
        segs.push(s.nfc().collect::<String>());
    }
    Ok(segs.join("/"))
}

/// `(parent_path, name)`; the root has no parent.
pub fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// All ancestor paths of `path`, nearest first, ending with the root (`""`).
pub fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = path;
    while !cur.is_empty() {
        cur = split(cur).0;
        out.push(cur.to_string());
    }
    out
}

/// `prefix` is e.g. `/remote.php/dav/files/alice`; collections get a trailing slash.
pub fn href(prefix: &str, path: &str, is_dir: bool) -> String {
    let mut out = String::from(prefix);
    out.push('/');
    for (i, seg) in path.split('/').filter(|s| !s.is_empty()).enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.extend(utf8_percent_encode(seg, SEGMENT));
    }
    if is_dir && !path.is_empty() {
        out.push('/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_collapses() {
        assert_eq!(normalize("").unwrap(), "");
        assert_eq!(normalize("/").unwrap(), "");
        assert_eq!(normalize("a//b/").unwrap(), "a/b");
        assert_eq!(normalize("Test%20%C3%84%C3%B6/x").unwrap(), "Test Äö/x");
    }

    #[test]
    fn nfd_becomes_nfc() {
        // "ä" as a + U+0308 (NFD) must equal the precomposed form
        assert_eq!(normalize("a%CC%88").unwrap(), "ä");
    }

    #[test]
    fn rejects_traversal_and_garbage() {
        assert!(normalize("a/../b").is_err());
        assert!(normalize("./a").is_err());
        assert!(normalize("%2e%2e/etc").is_err());
        assert!(normalize("a%00b").is_err());
        assert!(normalize("%FF").is_err());
    }

    #[test]
    fn double_encoding_is_decoded_once() {
        assert_eq!(normalize("a%252fb").unwrap(), "a%2fb");
    }

    #[test]
    fn splits_and_ancestors() {
        assert_eq!(split("a/b/c"), ("a/b", "c"));
        assert_eq!(split("a"), ("", "a"));
        assert_eq!(ancestors("a/b/c"), vec!["a/b", "a", ""]);
        assert!(ancestors("").is_empty());
    }

    #[test]
    fn href_encodes_per_segment() {
        assert_eq!(
            href("/remote.php/dav/files/alice", "", true),
            "/remote.php/dav/files/alice/"
        );
        assert_eq!(
            href("/remote.php/dav/files/alice", "Test Äö/a b%.txt", false),
            "/remote.php/dav/files/alice/Test%20%C3%84%C3%B6/a%20b%25.txt"
        );
        assert_eq!(href("/p", "d", true), "/p/d/");
    }
}
