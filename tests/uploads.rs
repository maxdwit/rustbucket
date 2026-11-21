//! Phase 4: chunked upload v2 (`/remote.php/dav/uploads/...`).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use http_body_util::BodyExt;
use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::routes::{AppState, router};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
    app: Router,
    data: std::path::PathBuf,
    state: AppState,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let pool = db::connect(&format!("sqlite://{}", dir.path().join("t.db").display()))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    db::create_user(&pool, "alice", "Alice", "pw", false)
        .await
        .unwrap();
    db::create_user(&pool, "eve", "Eve", "pw2", false)
        .await
        .unwrap();
    let cfg = Config {
        data_dir: data.clone(),
        ..Config::default()
    };
    let state = AppState::new(pool, Arc::new(cfg));
    Env {
        app: router(state.clone()),
        data,
        state,
        _dir: dir,
    }
}

struct R {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}
impl R {
    fn h(&self, n: &str) -> String {
        self.headers
            .get(n)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

async fn call(
    e: &Env,
    user: &str,
    method: &str,
    uri: &str,
    hdrs: &[(&str, &str)],
    body: Vec<u8>,
) -> R {
    let pw = if user == "alice" { "pw" } else { "pw2" };
    let mut b = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri)
        .header(
            header::AUTHORIZATION,
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pw}"))
            ),
        );
    for (k, v) in hdrs {
        b = b.header(*k, *v);
    }
    let r = e
        .app
        .clone()
        .oneshot(b.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let (status, headers) = (r.status(), r.headers().clone());
    let body = r.into_body().collect().await.unwrap().to_bytes().to_vec();
    R {
        status,
        headers,
        body,
    }
}

fn up(path: &str) -> String {
    format!("/remote.php/dav/uploads/alice{path}")
}
fn dest(path: &str) -> String {
    format!("https://cloud.example.com/remote.php/dav/files/alice{path}")
}
fn sha256(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}
fn pseudo(n: usize) -> Vec<u8> {
    let mut x = 0x1234_5678_9abc_def1u64;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

#[tokio::test]
async fn out_of_order_unpadded_chunks_assemble_numerically() {
    let e = env().await;
    let data = pseudo(12 * 1000 + 7);
    assert_eq!(
        call(
            &e,
            "alice",
            "MKCOL",
            &up("/u1"),
            &[("destination", &dest("/big.bin"))],
            vec![]
        )
        .await
        .status,
        StatusCode::CREATED
    );
    // names 1..12 unpadded, sent in scrambled order: a text sort would put 10 before 2
    let order = [7, 1, 12, 3, 10, 2, 9, 4, 11, 5, 8, 6];
    for n in order {
        let a = (n - 1) * 1000;
        let b = if n == 12 { data.len() } else { n * 1000 };
        let r = call(
            &e,
            "alice",
            "PUT",
            &up(&format!("/u1/{n}")),
            &[],
            data[a..b].to_vec(),
        )
        .await;
        assert_eq!(r.status, StatusCode::CREATED, "chunk {n}");
    }
    let total = data.len().to_string();
    let sha1: String = Sha1::digest(&data)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    let mv = call(
        &e,
        "alice",
        "MOVE",
        &up("/u1/.file"),
        &[
            ("destination", &dest("/big.bin")),
            ("oc-total-length", &total),
            ("x-oc-mtime", "1600000000"),
            ("oc-checksum", &format!("SHA1:{sha1}")),
        ],
        vec![],
    )
    .await;
    assert_eq!(mv.status, StatusCode::CREATED, "{}", mv.text());
    assert_eq!(mv.h("x-oc-mtime"), "accepted");
    assert!(
        !mv.h("oc-fileid").is_empty() && !mv.h("etag").is_empty() && !mv.h("oc-etag").is_empty()
    );
    // real file on disk, correct content, upload folder gone
    let on_disk = std::fs::read(e.data.join("alice/files/big.bin")).unwrap();
    assert_eq!(sha256(&on_disk), sha256(&data));
    assert!(!e.data.join("alice/uploads/u1").exists());
    // served back with the mtime from the request
    let g = call(
        &e,
        "alice",
        "GET",
        "/remote.php/dav/files/alice/big.bin",
        &[],
        vec![],
    )
    .await;
    assert_eq!(sha256(&g.body), sha256(&data));
    assert_eq!(g.h("last-modified"), "Sun, 13 Sep 2020 12:26:40 GMT");
    // second upload onto the existing file -> 204
    call(&e, "alice", "MKCOL", &up("/u2"), &[], vec![]).await;
    call(
        &e,
        "alice",
        "PUT",
        &up("/u2/0000000000000000"),
        &[],
        b"short".to_vec(),
    )
    .await;
    let mv = call(
        &e,
        "alice",
        "MOVE",
        &up("/u2/.file"),
        &[("destination", &dest("/big.bin")), ("oc-total-length", "5")],
        vec![],
    )
    .await;
    assert_eq!(mv.status, StatusCode::NO_CONTENT);
    assert_eq!(
        std::fs::read(e.data.join("alice/files/big.bin")).unwrap(),
        b"short"
    );
}

#[tokio::test]
async fn missing_chunk_length_and_checksum_mismatch_are_400_and_nothing_is_written() {
    let e = env().await;
    call(&e, "alice", "MKCOL", &up("/u1"), &[], vec![]).await;
    call(&e, "alice", "PUT", &up("/u1/0"), &[], vec![1; 100]).await;
    call(&e, "alice", "PUT", &up("/u1/200"), &[], vec![3; 100]).await; // chunk 100 missing
    let mv = call(
        &e,
        "alice",
        "MOVE",
        &up("/u1/.file"),
        &[("destination", &dest("/x.bin")), ("oc-total-length", "300")],
        vec![],
    )
    .await;
    assert_eq!(mv.status, StatusCode::BAD_REQUEST);
    assert!(mv.text().contains("do not sum up to 300 but 200"));
    assert!(!e.data.join("alice/files/x.bin").exists());
    // the upload is still there and can be completed (resume)
    let pf = call(
        &e,
        "alice",
        "PROPFIND",
        &up("/u1"),
        &[("depth", "1")],
        vec![],
    )
    .await;
    assert_eq!(pf.status, StatusCode::MULTI_STATUS);
    let t = pf.text();
    assert!(
        t.contains("/u1/0<") && t.contains("/u1/200<") && !t.contains("/u1/100<"),
        "{t}"
    );
    assert!(t.contains("<d:getcontentlength>100</d:getcontentlength>"));
    call(&e, "alice", "PUT", &up("/u1/100"), &[], vec![2; 100]).await;
    let bad = call(
        &e,
        "alice",
        "MOVE",
        &up("/u1/.file"),
        &[
            ("destination", &dest("/x.bin")),
            ("oc-total-length", "300"),
            (
                "oc-checksum",
                "SHA1:0000000000000000000000000000000000000000",
            ),
        ],
        vec![],
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(!e.data.join("alice/files/x.bin").exists());
    // wrong checksum discards the upload; a fresh one succeeds
    assert_eq!(
        call(
            &e,
            "alice",
            "PROPFIND",
            &up("/u1"),
            &[("depth", "1")],
            vec![]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn abort_wrong_user_bad_names_and_conflicts() {
    let e = env().await;
    assert_eq!(
        call(&e, "alice", "MKCOL", &up("/u1"), &[], vec![])
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        call(&e, "alice", "MKCOL", &up("/u1"), &[], vec![])
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    // another user cannot touch max's uploads (principal mismatch -> 404)
    for (m, p) in [
        ("MKCOL", "/u9"),
        ("PUT", "/u1/0"),
        ("DELETE", "/u1"),
        ("PROPFIND", "/u1"),
    ] {
        let r = call(&e, "eve", m, &up(p), &[], vec![]).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{m} {p}");
    }
    // unsafe ids / chunk names, and .file is MOVE-only
    for p in ["/..", "/.hidden", "/u1/.hidden", "/u1/a%2Fb", "/u1/0/extra"] {
        let r = call(&e, "alice", "PUT", &up(p), &[], b"x".to_vec()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{p}");
    }
    assert_eq!(
        call(&e, "alice", "PUT", &up("/u1/.file"), &[], b"x".to_vec())
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    // chunk into an upload that does not exist
    assert_eq!(
        call(&e, "alice", "PUT", &up("/nope/0"), &[], b"x".to_vec())
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // abort
    call(&e, "alice", "PUT", &up("/u1/0"), &[], b"x".to_vec()).await;
    assert_eq!(
        call(&e, "alice", "DELETE", &up("/u1"), &[], vec![])
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert!(!e.data.join("alice/uploads/u1").exists());
    assert_eq!(
        call(&e, "alice", "DELETE", &up("/u1"), &[], vec![])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // destination in another user's tree / outside -> 502, parent missing -> 409
    call(&e, "alice", "MKCOL", &up("/u2"), &[], vec![]).await;
    call(&e, "alice", "PUT", &up("/u2/0"), &[], b"abc".to_vec()).await;
    let r = call(
        &e,
        "alice",
        "MOVE",
        &up("/u2/.file"),
        &[("destination", "https://x/remote.php/dav/files/eve/a.txt")],
        vec![],
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY);
    let r = call(
        &e,
        "alice",
        "MOVE",
        &up("/u2/.file"),
        &[("destination", &dest("/no/such/dir/a.txt"))],
        vec![],
    )
    .await;
    assert!(
        r.status == StatusCode::CONFLICT || r.status == StatusCode::NOT_FOUND,
        "{}",
        r.status
    );
}

#[tokio::test]
async fn stale_if_match_on_the_final_move_is_412() {
    let e = env().await;
    call(
        &e,
        "alice",
        "PUT",
        "/remote.php/dav/files/alice/doc.txt",
        &[],
        b"v1".to_vec(),
    )
    .await;
    call(&e, "alice", "MKCOL", &up("/u1"), &[], vec![]).await;
    call(&e, "alice", "PUT", &up("/u1/0"), &[], b"v2".to_vec()).await;
    let r = call(
        &e,
        "alice",
        "MOVE",
        &up("/u1/.file"),
        &[
            ("destination", &dest("/doc.txt")),
            ("if-match", "\"stale\""),
        ],
        vec![],
    )
    .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        std::fs::read(e.data.join("alice/files/doc.txt")).unwrap(),
        b"v1"
    );
}

#[tokio::test]
async fn capabilities_advertise_chunking_only() {
    let e = env().await;
    let r = call(
        &e,
        "alice",
        "GET",
        "/ocs/v1.php/cloud/capabilities?format=json",
        &[],
        vec![],
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    let c = &v["ocs"]["data"]["capabilities"];
    assert_eq!(c["dav"]["chunking"], "1.0");
    assert!(c["dav"].get("bulkupload").is_none());
    assert_eq!(c["files"]["bigfilechunking"], true);
    // legacy v1 chunked PUTs are refused instead of being stored as odd files
    let r = call(
        &e,
        "alice",
        "PUT",
        "/remote.php/dav/files/alice/f.bin-chunking-1-2-0",
        &[("oc-chunked", "1")],
        b"x".to_vec(),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_IMPLEMENTED);
    assert!(!e.data.join("alice/files/f.bin-chunking-1-2-0").exists());
    // ... but a user file that merely has such a name is fine
    let r = call(
        &e,
        "alice",
        "PUT",
        "/remote.php/dav/files/alice/f.bin-chunking-1-2-0",
        &[],
        b"x".to_vec(),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
}

#[tokio::test]
async fn cleanup_removes_old_uploads_keeps_fresh_ones() {
    let e = env().await;
    call(&e, "alice", "MKCOL", &up("/old"), &[], vec![]).await;
    call(&e, "alice", "PUT", &up("/old/0"), &[], b"x".to_vec()).await;
    std::fs::create_dir_all(e.data.join("alice/tmp")).unwrap();
    std::fs::write(e.data.join("alice/tmp/orphan"), b"x").unwrap();
    // a stray top-level folder that is not a user is left alone
    std::fs::create_dir_all(e.data.join("tmp")).unwrap();
    let day = Duration::from_secs(24 * 3600);
    let n = rustbucket::uploads::cleanup_stale(&e.state.pool, &e.state.store, day)
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert!(e.data.join("alice/uploads/old/0").exists());
    tokio::time::sleep(Duration::from_millis(30)).await;
    let n = rustbucket::uploads::cleanup_stale(
        &e.state.pool,
        &e.state.store,
        Duration::from_millis(10),
    )
    .await
    .unwrap();
    assert_eq!(n, 2);
    assert!(
        !e.data.join("alice/uploads/old").exists() && !e.data.join("alice/tmp/orphan").exists()
    );
    assert!(e.data.join("tmp").exists());
}

#[tokio::test]
async fn size_limit_applies_to_put_chunks_and_totals() {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect(&format!("sqlite://{}", dir.path().join("t.db").display()))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    db::create_user(&pool, "alice", "Alice", "pw", false)
        .await
        .unwrap();
    let cfg = Config {
        data_dir: dir.path().join("data"),
        max_file_size: 100,
        ..Config::default()
    };
    let state = AppState::new(pool, Arc::new(cfg));
    let e = Env {
        app: router(state.clone()),
        data: dir.path().join("data"),
        state,
        _dir: dir,
    };
    let f = "/remote.php/dav/files/alice/f.bin";
    assert_eq!(
        call(&e, "alice", "PUT", f, &[], vec![0; 100]).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        call(&e, "alice", "PUT", f, &[], vec![0; 101]).await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        std::fs::read(e.data.join("alice/files/f.bin"))
            .unwrap()
            .len(),
        100
    ); // old content kept
    // announced total too big: refused at MKCOL
    assert_eq!(
        call(
            &e,
            "alice",
            "MKCOL",
            &up("/u1"),
            &[("oc-total-length", "101")],
            vec![]
        )
        .await
        .status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    // chunks that add up past the limit: the upload is dropped
    assert_eq!(
        call(&e, "alice", "MKCOL", &up("/u2"), &[], vec![])
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        call(&e, "alice", "PUT", &up("/u2/0"), &[], vec![1; 60])
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        call(&e, "alice", "PUT", &up("/u2/60"), &[], vec![1; 60])
            .await
            .status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert!(!e.data.join("alice/uploads/u2").exists());
    assert!(
        std::fs::read_dir(e.data.join("alice/tmp"))
            .unwrap()
            .next()
            .is_none()
    );
}
