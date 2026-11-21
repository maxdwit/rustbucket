//! Version and build number.
//!
//! * The **version** is the one in `Cargo.toml` (`make bump-version`).
//! * The **build number** is stamped into the image when it is built: `docker build --build-arg BUILD_DATE=...
//!   --build-arg GIT_COMMIT=...` (the Makefile passes them) become the environment variables read here. Started
//!   from `cargo run` or in a test they are unset and show as `dev`.
//! * The **build id** is a short hash of all three. It changes with every build, which is what browsers need to
//!   fetch new scripts and styles, and what shows that two servers run the same build.
//!
//! The values end up in HTML, in a Prometheus label and on a command line, so only a safe set of characters is kept.

use std::sync::OnceLock;

use sha2::{Digest, Sha256};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct BuildInfo {
    pub version: &'static str,
    /// ISO-8601 UTC (`2026-11-21T12:34:56Z`) as given at build time, or `dev`
    pub build_date: String,
    /// short commit id, `-dirty` when the tree had uncommitted changes, or `dev`
    pub commit: String,
    /// 12 hex characters of SHA-256 over "version|build_date|commit"
    pub hash: String,
}

/// The running build (read once from the environment).
pub fn get() -> &'static BuildInfo {
    static INFO: OnceLock<BuildInfo> = OnceLock::new();
    INFO.get_or_init(|| {
        BuildInfo::from_values(
            VERSION,
            std::env::var("RUSTBUCKET_BUILD_DATE").ok().as_deref(),
            std::env::var("RUSTBUCKET_BUILD_COMMIT").ok().as_deref(),
        )
    })
}

/// Keep letters, digits and `. - _ : +` (at most 64); nothing left, or "unknown" (the Dockerfile default) -> `dev`.
fn clean(v: Option<&str>) -> String {
    let s: String = v
        .unwrap_or("")
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '+'))
        .take(64)
        .collect();
    if s.is_empty() || s == "unknown" {
        "dev".to_string()
    } else {
        s
    }
}

impl BuildInfo {
    pub fn from_values(version: &'static str, date: Option<&str>, commit: Option<&str>) -> Self {
        let build_date = clean(date);
        let commit = clean(commit);
        let digest = Sha256::digest(format!("{version}|{build_date}|{commit}").as_bytes());
        let hash = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
        Self {
            version,
            build_date,
            commit,
            hash,
        }
    }

    /// `20261121-12:34:56` (UTC) for an ISO-8601 build date, anything else (`dev`) unchanged.
    pub fn build_date_display(&self) -> String {
        let b = self.build_date.as_bytes();
        let iso = b.len() == 20
            && b[4] == b'-'
            && b[7] == b'-'
            && b[10] == b'T'
            && b[13] == b':'
            && b[16] == b':'
            && b[19] == b'Z'
            && b.iter()
                .enumerate()
                .all(|(i, c)| matches!(i, 4 | 7 | 10 | 13 | 16 | 19) || c.is_ascii_digit());
        if !iso {
            return self.build_date.clone();
        }
        let d = &self.build_date;
        format!("{}{}{}-{}", &d[0..4], &d[5..7], &d[8..10], &d[11..19])
    }

    /// `v0.1.0 20261121-12:34:56 (3d33116)`; `v0.1.0 dev` when nothing was stamped.
    pub fn short(&self) -> String {
        let mut s = format!("v{} {}", self.version, self.build_date_display());
        if self.commit != "dev" {
            s.push_str(&format!(" ({})", self.commit));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_unknown_means_dev() {
        for (d, c) in [
            (None, None),
            (Some(""), Some("  ")),
            (Some("unknown"), Some("unknown")),
        ] {
            let b = BuildInfo::from_values("1.2.3", d, c);
            assert_eq!((b.build_date.as_str(), b.commit.as_str()), ("dev", "dev"));
            assert_eq!(b.short(), "v1.2.3 dev");
        }
    }

    #[test]
    fn date_is_shown_compact_and_other_text_unchanged() {
        let b = BuildInfo::from_values("1.2.3", Some("2026-11-21T12:34:56Z"), Some("3d33116"));
        assert_eq!(b.build_date_display(), "20261121-12:34:56");
        assert_eq!(b.short(), "v1.2.3 20261121-12:34:56 (3d33116)");
        let b = BuildInfo::from_values("1.2.3", Some("yesterday"), Some("3d33116-dirty"));
        assert_eq!(b.build_date_display(), "yesterday");
        assert_eq!(b.short(), "v1.2.3 yesterday (3d33116-dirty)");
        // wrong shape (letters where digits belong) is not reformatted
        let b = BuildInfo::from_values("1.2.3", Some("2026-11-21T12:34:5xZ"), None);
        assert_eq!(b.build_date_display(), "2026-11-21T12:34:5xZ");
    }

    #[test]
    fn hash_is_stable_short_and_changes_with_any_part() {
        let a = BuildInfo::from_values("1.2.3", Some("2026-11-21T12:34:56Z"), Some("abc1234"));
        let again = BuildInfo::from_values("1.2.3", Some("2026-11-21T12:34:56Z"), Some("abc1234"));
        assert_eq!(a.hash, again.hash);
        assert_eq!(a.hash.len(), 12);
        assert!(a.hash.chars().all(|c| c.is_ascii_hexdigit()));
        for other in [
            BuildInfo::from_values("1.2.4", Some("2026-11-21T12:34:56Z"), Some("abc1234")),
            BuildInfo::from_values("1.2.3", Some("2026-11-21T12:34:57Z"), Some("abc1234")),
            BuildInfo::from_values("1.2.3", Some("2026-11-21T12:34:56Z"), Some("abc1235")),
        ] {
            assert_ne!(a.hash, other.hash);
        }
    }

    #[test]
    fn hostile_values_cannot_break_html_labels_or_lines() {
        let b = BuildInfo::from_values(
            "1.2.3",
            Some("2026\"}\n# HELP x"),
            Some("<script>alert(1)</script>"),
        );
        for v in [&b.build_date, &b.commit] {
            assert!(
                !v.contains(['"', '<', '>', '\n', '{', '}', ' ', '/']),
                "{v}"
            );
        }
        assert!(b.commit.len() <= 64);
        let long = "a".repeat(500);
        assert_eq!(
            BuildInfo::from_values("1.2.3", None, Some(&long))
                .commit
                .len(),
            64
        );
    }
}
