#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    if let Ok(p) = rustbucket::paths::normalize(&s) {
        // whatever came in, the result is a safe relative path: no traversal, no NUL, no empty parts
        assert!(!p.starts_with('/') && !p.contains('\0') && !p.contains("//"), "{p:?}");
        for c in p.split('/').filter(|_| !p.is_empty()) {
            assert!(c != ".." && c != "." && !c.is_empty(), "{p:?}");
            // an href segment decodes back to exactly the component
            let enc = rustbucket::paths::encode_segment(c);
            assert!(enc.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.~():@%".contains(&b)), "{enc:?}");
            assert_eq!(percent_decode(&enc), c);
        }
        let _ = rustbucket::paths::href("/remote.php/dav/files/u", &p, false);
    }
});

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() + 1 && i + 2 <= b.len() - 1 {
            let h = u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap();
            out.push(h);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}
