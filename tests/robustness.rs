//! Phase 9: garbage in must never panic a parser and never produce a 5xx. Deterministic pseudo-random
//! inputs (so a failure is reproducible); `cargo fuzz` (fuzz/) explores the same entry points coverage-guided.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request};
use base64::Engine;
use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::routes::{AppState, router};
use tower::ServiceExt;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = self.below(max + 1);
        (0..n).map(|_| self.next() as u8).collect()
    }
    /// Flip, drop, duplicate and splice bytes of a valid sample.
    fn mutate(&mut self, sample: &[u8]) -> Vec<u8> {
        let mut v = sample.to_vec();
        for _ in 0..1 + self.below(4) {
            if v.is_empty() {
                break;
            }
            let i = self.below(v.len());
            match self.below(5) {
                0 => v[i] = self.next() as u8,
                1 => {
                    v.remove(i);
                }
                2 => {
                    let b = v[i];
                    v.insert(i, b);
                }
                3 => v.truncate(i),
                _ => {
                    let j = self.below(v.len());
                    let chunk: Vec<u8> = v[i.min(j)..i.max(j)].to_vec();
                    v.splice(i..i, chunk);
                }
            }
        }
        v
    }
}

const PROPFIND: &[u8] = br#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns"><d:prop><d:getetag/><oc:fileid/><nc:has-preview/></d:prop></d:propfind>"#;
const SEARCH: &[u8] = br#"<?xml version="1.0"?><d:searchrequest xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:basicsearch><d:select><d:prop><oc:fileid/></d:prop></d:select><d:from><d:scope><d:href>/files/alice</d:href><d:depth>infinity</d:depth></d:scope></d:from><d:where><d:and><d:like><d:prop><d:getcontenttype/></d:prop><d:literal>image/%</d:literal></d:like><d:gt><d:prop><d:getlastmodified/></d:prop><d:literal>100</d:literal></d:gt></d:and></d:where><d:orderby><d:order><d:prop><d:getlastmodified/></d:prop><d:descending/></d:order></d:orderby><d:limit><d:nresults>5</d:nresults></d:limit></d:basicsearch></d:searchrequest>"#;
const TOML: &[u8] = b"listen = \"0.0.0.0:8080\"\nbase_url = \"https://x.example\"\ntrusted_proxies = [\"10.0.0.0/8\"]\nmax_file_size = 1000\n";

#[test]
fn parsers_survive_random_and_mutated_input() {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    for i in 0..30_000 {
        let data = if i % 3 == 0 {
            r.bytes(300)
        } else {
            r.mutate(PROPFIND)
        };
        let _ = rustbucket::propfind::parse_request(&data);
        let data = if i % 3 == 0 {
            r.bytes(300)
        } else {
            r.mutate(SEARCH)
        };
        let _ = rustbucket::search::fuzz_search_request(&data);
        let _ = rustbucket::search::fuzz_report_body(&data);
        let cfg = if i % 3 == 0 {
            r.bytes(120)
        } else {
            r.mutate(TOML)
        };
        let _ = Config::from_toml(&String::from_utf8_lossy(&cfg));
        let raw = r.bytes(60);
        let s = String::from_utf8_lossy(&raw);
        let _ = rustbucket::paths::normalize(&s);
        let _ = rustbucket::paths::normalize(&format!("/a/{s}/../{s}"));
        let _ = rustbucket::sharing_api::fuzz_params(&s, i % 2 == 0, &r.bytes(80));
        let _ = rustbucket::web::fmt_time(r.next() as i64);
        let _ = rustbucket::web::human_size(r.next() as i64);
        let _ = rustbucket::paths::href("/p", &s, i % 2 == 0);
    }
}

/// Requests with hostile paths, methods, headers and bodies against every route family: the answer may be
/// 4xx or a normal one, but never a 5xx (a 5xx would be a bug: a panic, an unhandled error, a bad query).
#[tokio::test(flavor = "multi_thread")]
async fn the_http_layer_never_answers_5xx_to_garbage() {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect(&format!("sqlite://{}", dir.path().join("t.db").display()))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    db::create_user(&pool, "alice", "Alice", "pw", true)
        .await
        .unwrap();
    db::create_user(&pool, "eve", "Eve", "pw2", false)
        .await
        .unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join(".rustbucket-data"), "").unwrap();
    let app = router(AppState::new(
        pool,
        Arc::new(Config {
            data_dir: data,
            ..Config::default()
        }),
    ));
    let auth = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("alice:pw")
    );
    let send = |app: axum::Router,
                method: String,
                uri: String,
                hdrs: Vec<(String, String)>,
                body: Vec<u8>| async move {
        let mut b = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET))
            .uri(uri);
        for (k, v) in hdrs {
            b = b.header(k, v);
        }
        match b.body(Body::from(body)) {
            Ok(req) => Some(app.oneshot(req).await.unwrap().status().as_u16()),
            Err(_) => None, // not even a valid HTTP request: rejected before it reaches us
        }
    };
    // some real content so deeper code paths run
    for (m, p, b) in [
        ("MKCOL", "/remote.php/dav/files/alice/d", ""),
        ("PUT", "/remote.php/dav/files/alice/d/a.txt", "x"),
        ("PUT", "/remote.php/dav/files/alice/img.png", "not a png"),
    ] {
        send(
            app.clone(),
            m.into(),
            p.into(),
            vec![("authorization".into(), auth.clone())],
            b.as_bytes().to_vec(),
        )
        .await;
    }
    let methods = [
        "GET",
        "HEAD",
        "PUT",
        "POST",
        "DELETE",
        "PROPFIND",
        "PROPPATCH",
        "MKCOL",
        "COPY",
        "MOVE",
        "LOCK",
        "UNLOCK",
        "REPORT",
        "SEARCH",
        "OPTIONS",
        "PATCH",
        "TRACE",
        "FOO",
    ];
    let paths = [
        "/remote.php/dav/files/alice/",
        "/remote.php/dav/files/alice/d/a.txt",
        "/remote.php/dav/files/alice/img.png",
        "/remote.php/webdav/d",
        "/remote.php/dav/uploads/alice/u1",
        "/remote.php/dav/uploads/alice/u1/0",
        "/remote.php/dav/uploads/alice/u1/.file",
        "/remote.php/dav/",
        "/remote.php/dav/trashbin/alice/",
        "/remote.php/dav/trashbin/alice/trash/",
        "/remote.php/dav/trashbin/alice/trash/a.txt.d1",
        "/remote.php/dav/trashbin/alice/restore/x",
        "/remote.php/dav/versions/alice/",
        "/remote.php/dav/versions/alice/versions/1",
        "/remote.php/dav/versions/alice/versions/1/2",
        "/ocs/v2.php/cloud/user",
        "/ocs/v2.php/cloud/capabilities",
        "/ocs/v2.php/apps/files_sharing/api/v1/shares",
        "/ocs/v2.php/apps/files_sharing/api/v1/shares/1",
        "/ocs/v2.php/apps/files_sharing/api/v1/sharees",
        "/ocs/v2.php/core/getapppassword",
        "/index.php/core/preview.png",
        "/index.php/core/preview",
        "/index.php/apps/files/api/v1/thumbnail/64/64/img.png",
        "/index.php/apps/files/api/v1/stats",
        "/index.php/login/v2",
        "/index.php/login/v2/poll",
        "/status.php",
        "/metrics",
        "/",
        "/login",
        "/apps/files/",
        "/apps/files/trash",
        "/ui/share",
        "/ui/versions",
        "/settings/user",
        "/settings/users",
        "/static/app.js",
        "/index.php/avatar/alice/64",
        "/ocs/v2.php/apps/notifications/api/v2/notifications",
        "/index.php/core/wipe/check",
    ];
    let queries = [
        "",
        "?format=json",
        "?x=0&y=0",
        "?file=/d/a.txt&x=64&y=64",
        "?fileId=abc",
        "?path=%00",
        "?dir=%FF%FE",
        "?search=%27%22%3C",
        "?path=/../../etc/passwd",
        "?dir=/d&hx=1",
    ];
    let mut r = Rng(0xDEAD_BEEF_CAFE_F00D);
    let mut bad = Vec::new();
    for i in 0..6_000 {
        let method = methods[r.below(methods.len())].to_string();
        let mut path = paths[r.below(paths.len())].to_string();
        if r.below(4) == 0 {
            let junk: String = r.bytes(12).iter().map(|b| format!("%{b:02X}")).collect();
            path.push_str(&format!("/{junk}"));
        }
        let uri = format!("{path}{}", queries[r.below(queries.len())]);
        let mut hdrs = vec![];
        match r.below(4) {
            0 => {}
            1 => hdrs.push((
                "authorization".to_string(),
                "Basic !!!not-base64".to_string(),
            )),
            _ => hdrs.push(("authorization".to_string(), auth.clone())),
        }
        for _ in 0..r.below(4) {
            let name = [
                "destination",
                "depth",
                "overwrite",
                "if-match",
                "if-none-match",
                "range",
                "oc-total-length",
                "oc-checksum",
                "x-oc-mtime",
                "hx-request",
                "content-type",
                "cookie",
                "x-csrf-token",
                "origin",
            ][r.below(14)];
            let val: String = r.bytes(20).iter().map(|b| (b % 94 + 33) as char).collect();
            hdrs.push((name.to_string(), val));
        }
        let body = match r.below(4) {
            0 => vec![],
            1 => r.bytes(400),
            2 => r.mutate(PROPFIND),
            _ => r.mutate(SEARCH),
        };
        if let Some(status) = send(app.clone(), method.clone(), uri.clone(), hdrs, body).await
            && status >= 500
        {
            bad.push(format!("#{i} {method} {uri} -> {status}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} responses were 5xx, first ones:\n{}",
        bad.len(),
        bad.iter().take(15).cloned().collect::<Vec<_>>().join("\n")
    );
}
