//! Phase 3: WebDAV file core. Expected statuses/headers mirror the recordings of stock Nextcloud 35
//! in tests/fixtures/nc35/ (file names are noted next to each case).

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use http_body_util::BodyExt;
use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::routes::{AppState, router};
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
    app: Router,
    data: std::path::PathBuf,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let pool = db::connect(&format!("sqlite://{}", dir.path().join("t.db").display()))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    db::create_user(&pool, "alice", "Alice A.", "pw", false)
        .await
        .unwrap();
    db::create_user(&pool, "eve", "Eve", "pw2", false)
        .await
        .unwrap();
    let cfg = Config {
        data_dir: data.clone(),
        ..Config::default()
    };
    Env {
        app: router(AppState::new(pool, Arc::new(cfg))),
        data,
        _dir: dir,
    }
}

struct R {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}
impl R {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn h(&self, n: &str) -> String {
        self.headers
            .get(n)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    }
}

async fn call(
    e: &Env,
    user: &str,
    pw: &str,
    method: &str,
    uri: &str,
    hdrs: &[(&str, &str)],
    body: Vec<u8>,
) -> R {
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
    let status = r.status();
    let headers = r.headers().clone();
    let body = r.into_body().collect().await.unwrap().to_bytes().to_vec();
    R {
        status,
        headers,
        body,
    }
}

async fn max(e: &Env, method: &str, path: &str, hdrs: &[(&str, &str)], body: &[u8]) -> R {
    call(
        e,
        "alice",
        "pw",
        method,
        &format!("/remote.php/dav/files/alice{path}"),
        hdrs,
        body.to_vec(),
    )
    .await
}

const PF: &str = r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns"><d:prop><d:getetag/><d:getlastmodified/><d:getcontentlength/><d:resourcetype/><oc:fileid/><oc:size/><oc:permissions/><oc:checksums/></d:prop></d:propfind>"#;

fn tag(xml: &str, t: &str) -> String {
    let open = format!("<{t}>");
    let i = xml
        .find(&open)
        .unwrap_or_else(|| panic!("no <{t}> in {xml}"))
        + open.len();
    xml[i..i + xml[i..].find("</").unwrap()].to_string()
}

async fn root_etag(e: &Env) -> String {
    let r = max(e, "PROPFIND", "/", &[("depth", "0")], PF.as_bytes()).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    tag(&r.text(), "d:getetag")
}

#[tokio::test]
async fn auth_and_user_isolation() {
    let e = env().await;
    // no auth -> 401 (noauth-propfind.txt)
    let r = e
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("PROPFIND")
                .uri("/remote.php/dav/files/alice/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // someone else's tree -> 404 (wrong-user.txt)
    let r = call(
        &e,
        "eve",
        "pw2",
        "PROPFIND",
        "/remote.php/dav/files/alice/",
        &[("depth", "0")],
        vec![],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // webdav alias maps to the authenticated user (webdav-alias.txt)
    let r = call(
        &e,
        "alice",
        "pw",
        "PROPFIND",
        "/remote.php/webdav/",
        &[("depth", "0")],
        vec![],
    )
    .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert!(r.text().contains("<d:href>/remote.php/webdav/</d:href>"));
}

#[tokio::test]
async fn options_and_root_propfind() {
    let e = env().await;
    let r = max(&e, "OPTIONS", "/", &[], b"").await; // options-root.txt
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.h("dav").starts_with("1, 3, extended-mkcol"));
    assert!(r.h("allow").contains("PROPFIND") && r.h("allow").contains("MKCOL"));
    let r = max(&e, "PROPFIND", "/", &[("depth", "0")], PF.as_bytes()).await; // propfind-root-d0.txt
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    let x = r.text();
    assert!(x.contains("<d:href>/remote.php/dav/files/alice/</d:href>"));
    assert!(x.contains("<d:resourcetype><d:collection/></d:resourcetype>"));
    assert!(x.contains("<oc:permissions>RGDNVCK</oc:permissions>"));
    assert!(x.contains("<oc:size>0</oc:size>"));
    assert!(x.contains(
        "<d:prop><d:getcontentlength/><oc:checksums/></d:prop><d:status>HTTP/1.1 404 Not Found"
    )); // propstat 404
    // no body -> allprop (propfind-root-allprop-noBody.txt)
    let r = max(&e, "PROPFIND", "/", &[("depth", "0")], b"").await;
    assert!(
        r.text()
            .contains("<d:quota-used-bytes>0</d:quota-used-bytes>")
    );
}

#[tokio::test]
async fn mkcol_put_get_cases() {
    let e = env().await;
    let r = max(&e, "MKCOL", "/Test%20%C3%84%C3%B6/", &[], b"").await; // mkcol-new.txt
    assert_eq!(r.status, StatusCode::CREATED);
    assert!(r.h("oc-fileid").starts_with("0000"));
    assert_eq!(
        max(&e, "MKCOL", "/Test%20%C3%84%C3%B6/", &[], b"")
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED
    ); // mkcol-exists.txt
    assert_eq!(
        max(&e, "MKCOL", "/nope/deeper/", &[], b"").await.status,
        StatusCode::CONFLICT
    ); // mkcol-noparent.txt

    let r = max(
        &e,
        "PUT",
        "/Test%20%C3%84%C3%B6/h.txt",
        &[
            ("x-oc-mtime", "1700000000"),
            (
                "oc-checksum",
                "SHA1:22596363b3de40b06f981fb85d82312e8c0ed511",
            ),
        ],
        b"hello world\n",
    )
    .await; // put-new.txt
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.h("x-oc-mtime"), "accepted");
    assert!(r.h("etag").starts_with('"') && r.h("etag") == r.h("oc-etag"));
    assert_eq!(r.h("x-nc-permissions"), "RGDNVW");
    assert_eq!(r.h("x-nc-ownerid"), "alice");
    let fileid = r.h("oc-fileid");
    let r2 = max(
        &e,
        "PUT",
        "/Test%20%C3%84%C3%B6/h.txt",
        &[("x-oc-mtime", "1700000100")],
        b"hello world\n",
    )
    .await; // put-overwrite.txt
    assert_eq!(r2.status, StatusCode::NO_CONTENT);
    assert_eq!(r2.h("oc-fileid"), fileid, "overwrite keeps the fileid");
    assert_ne!(r2.h("etag"), r.h("etag"), "overwrite changes the etag");
    assert_eq!(
        max(&e, "PUT", "/nope2/h.txt", &[], b"x").await.status,
        StatusCode::NOT_FOUND
    ); // put-noparent.txt (404, not 409)

    // bad checksum is refused and leaves nothing behind
    let bad = max(
        &e,
        "PUT",
        "/bad.txt",
        &[("oc-checksum", "SHA1:deadbeef")],
        b"hello",
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        max(&e, "GET", "/bad.txt", &[], b"").await.status,
        StatusCode::NOT_FOUND
    );

    let g = max(&e, "GET", "/Test%20%C3%84%C3%B6/h.txt", &[], b"").await; // get-file.txt
    assert_eq!(g.status, StatusCode::OK);
    assert_eq!(g.text(), "hello world\n");
    assert_eq!(g.h("content-type"), "text/plain");
    assert!(
        g.h("content-disposition")
            .starts_with("attachment; filename*=UTF-8''h%2Etxt")
            || g.h("content-disposition").contains("filename=\"h.txt\"")
    );
    let rg = max(
        &e,
        "GET",
        "/Test%20%C3%84%C3%B6/h.txt",
        &[("range", "bytes=0-4")],
        b"",
    )
    .await; // get-range.txt
    assert_eq!(rg.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(rg.text(), "hello");
    assert_eq!(rg.h("content-range"), "bytes 0-4/12");
    assert_eq!(
        max(
            &e,
            "GET",
            "/Test%20%C3%84%C3%B6/h.txt",
            &[("range", "bytes=50-")],
            b""
        )
        .await
        .status,
        StatusCode::RANGE_NOT_SATISFIABLE
    );
    let hd = max(&e, "HEAD", "/Test%20%C3%84%C3%B6/h.txt", &[], b"").await; // head-file.txt
    assert_eq!(hd.status, StatusCode::OK);
    assert!(hd.body.is_empty() && hd.h("content-length") == "12");
    // HEAD on a collection (the Windows client probes the DAV root): 200 with an ETag, not 405
    let hdir = max(&e, "HEAD", "/", &[], b"").await;
    assert_eq!(hdir.status, StatusCode::OK);
    assert!(!hdir.h("etag").is_empty() && hdir.body.is_empty());

    // propfind on the file: mtime from X-OC-Mtime, size, checksum
    let p = max(
        &e,
        "PROPFIND",
        "/Test%20%C3%84%C3%B6/h.txt",
        &[("depth", "0")],
        PF.as_bytes(),
    )
    .await; // propfind-file-d0.txt
    let x = p.text();
    assert!(
        x.contains("<d:getlastmodified>Tue, 14 Nov 2023 22:15:00 GMT</d:getlastmodified>"),
        "{x}"
    );
    assert!(x.contains("<d:getcontentlength>12</d:getcontentlength>"));
    assert!(x.contains("<d:resourcetype/>"));
    assert!(x.contains("<oc:permissions>RGDNVW</oc:permissions>"));
}

#[tokio::test]
async fn etag_and_size_propagate_to_root() {
    let e = env().await;
    let e0 = root_etag(&e).await;
    max(&e, "MKCOL", "/a/", &[], b"").await;
    max(&e, "MKCOL", "/a/b/", &[], b"").await;
    let e1 = root_etag(&e).await;
    assert_ne!(e0, e1, "mkcol changes root etag");
    let r = max(&e, "PUT", "/a/b/f.bin", &[], &[7u8; 1000]).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let e2 = root_etag(&e).await;
    assert_ne!(e1, e2, "deep put changes root etag");
    // every ancestor changed and carries the recursive size
    for (p, size) in [("/", 1000), ("/a/", 1000), ("/a/b/", 1000)] {
        let r = max(&e, "PROPFIND", p, &[("depth", "0")], PF.as_bytes()).await;
        assert_eq!(tag(&r.text(), "oc:size"), size.to_string(), "{p}");
    }
    max(&e, "PUT", "/a/b/f.bin", &[], &[7u8; 400]).await; // shrink
    let r = max(&e, "PROPFIND", "/a/", &[("depth", "0")], PF.as_bytes()).await;
    assert_eq!(tag(&r.text(), "oc:size"), "400");
    let e3 = root_etag(&e).await;
    assert_ne!(e2, e3);
    max(&e, "DELETE", "/a/b/f.bin", &[], b"").await;
    let r = max(&e, "PROPFIND", "/", &[("depth", "0")], PF.as_bytes()).await;
    assert_eq!(tag(&r.text(), "oc:size"), "0");
    assert_ne!(tag(&r.text(), "d:getetag"), e3);
}

#[tokio::test]
async fn depth_one_lists_children_and_depth_infinity_recurses() {
    let e = env().await;
    max(&e, "MKCOL", "/d/", &[], b"").await;
    max(&e, "MKCOL", "/d/sub/", &[], b"").await;
    max(&e, "PUT", "/d/one.txt", &[], b"1").await;
    max(&e, "PUT", "/d/sub/two.txt", &[], b"22").await;
    let r = max(&e, "PROPFIND", "/d/", &[("depth", "1")], PF.as_bytes()).await; // propfind-dir-d1.txt
    let x = r.text();
    assert_eq!(x.matches("<d:response>").count(), 3); // d, sub, one.txt
    assert!(x.contains("/remote.php/dav/files/alice/d/sub/</d:href>"));
    assert!(!x.contains("two.txt"));
    let r = max(
        &e,
        "PROPFIND",
        "/d/",
        &[("depth", "infinity")],
        PF.as_bytes(),
    )
    .await; // propfind-root-d-inf.txt
    assert_eq!(r.text().matches("<d:response>").count(), 4);
    assert_eq!(
        max(
            &e,
            "PROPFIND",
            "/missing/",
            &[("depth", "0")],
            PF.as_bytes()
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn move_keeps_fileid_and_etag_and_updates_tree() {
    let e = env().await;
    max(&e, "MKCOL", "/src/", &[], b"").await;
    max(&e, "MKCOL", "/src/in/", &[], b"").await;
    let put = max(&e, "PUT", "/src/in/f.txt", &[], b"data").await;
    let dir_id = max(&e, "PROPFIND", "/src/", &[("depth", "0")], PF.as_bytes()).await;
    let dir_id = tag(&dir_id.text(), "oc:fileid");
    // rename a file (move-file.txt)
    let r = max(
        &e,
        "MOVE",
        "/src/in/f.txt",
        &[(
            "destination",
            "http://localhost/remote.php/dav/files/alice/src/in/g.txt",
        )],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.h("oc-fileid"), put.h("oc-fileid"));
    assert_eq!(r.h("etag"), put.h("etag"));
    assert_eq!(
        max(&e, "GET", "/src/in/f.txt", &[], b"").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        max(&e, "GET", "/src/in/g.txt", &[], b"").await.text(),
        "data"
    );
    // move a whole directory with content, destination percent-encoded with umlauts
    let r = max(
        &e,
        "MOVE",
        "/src/",
        &[(
            "destination",
            "/remote.php/dav/files/alice/Zielordner%20%C3%9C/",
        )],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(
        r.h("oc-fileid")
            .trim_start_matches('0')
            .split("oc")
            .next()
            .unwrap(),
        dir_id
    );
    assert_eq!(
        max(&e, "GET", "/Zielordner%20%C3%9C/in/g.txt", &[], b"")
            .await
            .text(),
        "data"
    );
    assert_eq!(
        max(&e, "PROPFIND", "/src/", &[("depth", "0")], b"")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // overwrite semantics + guards
    max(&e, "PUT", "/x.txt", &[], b"x").await;
    max(&e, "PUT", "/y.txt", &[], b"yy").await;
    let no = max(
        &e,
        "MOVE",
        "/x.txt",
        &[
            ("destination", "/remote.php/dav/files/alice/y.txt"),
            ("overwrite", "F"),
        ],
        b"",
    )
    .await;
    assert_eq!(no.status, StatusCode::PRECONDITION_FAILED);
    let ow = max(
        &e,
        "MOVE",
        "/x.txt",
        &[("destination", "/remote.php/dav/files/alice/y.txt")],
        b"",
    )
    .await;
    assert_eq!(ow.status, StatusCode::NO_CONTENT);
    assert_eq!(max(&e, "GET", "/y.txt", &[], b"").await.text(), "x");
    let into_self = max(
        &e,
        "MOVE",
        "/Zielordner%20%C3%9C/",
        &[(
            "destination",
            "/remote.php/dav/files/alice/Zielordner%20%C3%9C/in/z/",
        )],
        b"",
    )
    .await;
    assert_eq!(into_self.status, StatusCode::FORBIDDEN);
    let other = max(
        &e,
        "MOVE",
        "/y.txt",
        &[("destination", "/remote.php/dav/files/eve/y.txt")],
        b"",
    )
    .await;
    assert_eq!(other.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        max(
            &e,
            "MOVE",
            "/y.txt",
            &[("destination", "/remote.php/dav/files/alice/nodir/y.txt")],
            b""
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    // sizes stayed consistent through all of it
    let r = max(&e, "PROPFIND", "/", &[("depth", "0")], PF.as_bytes()).await;
    assert_eq!(tag(&r.text(), "oc:size"), "5"); // "data" + "x"
}

#[tokio::test]
async fn copy_creates_new_fileids_and_independent_blobs() {
    let e = env().await;
    max(&e, "MKCOL", "/c/", &[], b"").await;
    let a = max(&e, "PUT", "/c/a.txt", &[], b"abc").await;
    let r = max(
        &e,
        "COPY",
        "/c/",
        &[("destination", "/remote.php/dav/files/alice/c2/")],
        b"",
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let copy = max(&e, "GET", "/c2/a.txt", &[], b"").await;
    assert_eq!(copy.text(), "abc");
    let g = max(
        &e,
        "PROPFIND",
        "/c2/a.txt",
        &[("depth", "0")],
        PF.as_bytes(),
    )
    .await;
    assert_ne!(
        tag(&g.text(), "oc:fileid"),
        a.h("oc-fileid")
            .trim_start_matches('0')
            .split("oc")
            .next()
            .unwrap()
    );
    // deleting the original must not break the copy (hard links / separate blobs)
    max(&e, "DELETE", "/c/", &[], b"").await;
    assert_eq!(max(&e, "GET", "/c2/a.txt", &[], b"").await.text(), "abc");
    let root = max(&e, "PROPFIND", "/", &[("depth", "0")], PF.as_bytes()).await;
    assert_eq!(tag(&root.text(), "oc:size"), "3");
}

#[tokio::test]
async fn delete_cases_and_real_files_on_disk() {
    let e = env().await;
    let f = e.data.join("alice/files/f.txt");
    max(&e, "PUT", "/f.txt", &[], b"payload").await;
    assert_eq!(
        std::fs::read(&f).unwrap(),
        b"payload",
        "stored under its real name"
    );
    assert_eq!(
        max(&e, "DELETE", "/f.txt", &[], b"").await.status,
        StatusCode::NO_CONTENT
    ); // delete-file.txt
    assert_eq!(
        max(&e, "DELETE", "/f.txt", &[], b"").await.status,
        StatusCode::NOT_FOUND
    ); // delete-missing.txt
    assert!(!f.exists(), "file removed from disk");
    assert_eq!(
        max(&e, "DELETE", "/", &[], b"").await.status,
        StatusCode::FORBIDDEN
    );
    // overwrite replaces in place, leaves no temp files
    max(&e, "PUT", "/o.txt", &[], b"one").await;
    max(&e, "PUT", "/o.txt", &[], b"two!").await;
    assert_eq!(
        std::fs::read(e.data.join("alice/files/o.txt")).unwrap(),
        b"two!"
    );
    assert!(
        walk(&e.data.join("alice/tmp")).is_empty(),
        "no temp files left"
    );
    assert_eq!(walk(&e.data.join("alice/files")).len(), 1);
}

#[tokio::test]
async fn disk_layout_mtime_move_and_copy_independence() {
    let e = env().await;
    max(&e, "MKCOL", "/Ordner%20%C3%84/", &[], b"").await;
    max(
        &e,
        "PUT",
        "/Ordner%20%C3%84/a.txt",
        &[("x-oc-mtime", "1700000000")],
        b"abc",
    )
    .await;
    let p = e.data.join("alice/files/Ordner Ä/a.txt");
    assert_eq!(std::fs::read(&p).unwrap(), b"abc");
    let mtime = std::fs::metadata(&p)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(mtime, 1_700_000_000, "disk mtime follows X-OC-Mtime");
    // move on disk
    max(
        &e,
        "MOVE",
        "/Ordner%20%C3%84/",
        &[("destination", "/remote.php/dav/files/alice/neu/")],
        b"",
    )
    .await;
    assert!(
        e.data.join("alice/files/neu/a.txt").exists()
            && !e.data.join("alice/files/Ordner Ä").exists()
    );
    // a copy is a real, independent file (edit-in-place of the original must not touch it)
    max(
        &e,
        "COPY",
        "/neu/a.txt",
        &[("destination", "/remote.php/dav/files/alice/b.txt")],
        b"",
    )
    .await;
    std::fs::write(e.data.join("alice/files/neu/a.txt"), b"CHANGED").unwrap();
    assert_eq!(max(&e, "GET", "/b.txt", &[], b"").await.text(), "abc");
    // other users have their own folder
    let r = call(
        &e,
        "eve",
        "pw2",
        "PUT",
        "/remote.php/dav/files/eve/b.txt",
        &[],
        b"eve".to_vec(),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(
        std::fs::read(e.data.join("eve/files/b.txt")).unwrap(),
        b"eve"
    );
    assert_eq!(max(&e, "GET", "/b.txt", &[], b"").await.text(), "abc");
}

#[tokio::test]
async fn missing_file_on_disk_is_404_and_long_names_are_400() {
    let e = env().await;
    max(&e, "PUT", "/gone.txt", &[], b"x").await;
    std::fs::remove_file(e.data.join("alice/files/gone.txt")).unwrap(); // out-of-band removal
    assert_eq!(
        max(&e, "GET", "/gone.txt", &[], b"").await.status,
        StatusCode::NOT_FOUND
    );
    let long = "x".repeat(300);
    assert_eq!(
        max(&e, "PUT", &format!("/{long}"), &[], b"x").await.status,
        StatusCode::BAD_REQUEST
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p))
            } else {
                out.push(p)
            }
        }
    }
    out
}

#[tokio::test]
async fn nfc_nfd_names_are_the_same_file_and_traversal_is_rejected() {
    let e = env().await;
    // "ä" precomposed, then the decomposed form addresses the same file
    max(&e, "PUT", "/%C3%A4.txt", &[], b"x").await;
    assert_eq!(max(&e, "GET", "/a%CC%88.txt", &[], b"").await.text(), "x");
    assert_eq!(
        max(&e, "PUT", "/a/../../etc/passwd", &[], b"x")
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        max(&e, "PUT", "/%2e%2e/x", &[], b"x").await.status,
        StatusCode::BAD_REQUEST
    );
    // emoji + spaces round trip through PROPFIND hrefs
    max(&e, "PUT", "/%F0%9F%93%B7%20pic%20%231.jpg", &[], b"jpg").await;
    let r = max(&e, "PROPFIND", "/", &[("depth", "1")], PF.as_bytes()).await;
    assert!(
        r.text()
            .contains("/remote.php/dav/files/alice/%F0%9F%93%B7%20pic%20%231.jpg"),
        "{}",
        r.text()
    );
}

#[tokio::test]
async fn conditional_put_and_large_body() {
    let e = env().await;
    let r = max(&e, "PUT", "/big.bin", &[], &vec![1u8; 5 * 1024 * 1024]).await; // > axum's 2 MB default extractor limit
    assert_eq!(r.status, StatusCode::CREATED);
    let g = max(&e, "GET", "/big.bin", &[], b"").await;
    assert_eq!(g.body.len(), 5 * 1024 * 1024);
    let etag = r.h("etag");
    assert_eq!(
        max(&e, "PUT", "/big.bin", &[("if-match", "\"stale\"")], b"x")
            .await
            .status,
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(
        max(&e, "PUT", "/big.bin", &[("if-match", &etag)], b"y")
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        max(&e, "PUT", "/big.bin", &[("if-none-match", "*")], b"z")
            .await
            .status,
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(max(&e, "GET", "/big.bin", &[], b"").await.text(), "y");
}

#[tokio::test]
async fn lock_unlock_proppatch_stubs() {
    let e = env().await;
    // OPTIONS now announces class 2 and the methods
    let o = max(&e, "OPTIONS", "/", &[], b"").await;
    assert!(o.h("dav").split(", ").any(|c| c == "2"));
    assert!(o.h("allow").contains("LOCK") && o.h("allow").contains("PROPPATCH"));
    // LOCK on an existing file: 200 + token; on an unmapped URL: creates an empty file (201)
    max(&e, "PUT", "/f.txt", &[], b"abc").await;
    let l = max(&e, "LOCK", "/f.txt", &[], b"").await;
    assert_eq!(l.status, StatusCode::OK);
    assert!(l.h("lock-token").starts_with("<opaquelocktoken:"));
    assert!(
        l.text().contains("<d:lockdiscovery>")
            && l.text().contains("/remote.php/dav/files/alice/f.txt")
    );
    let n = max(&e, "LOCK", "/new.txt", &[], b"").await;
    assert_eq!(n.status, StatusCode::CREATED);
    assert_eq!(
        std::fs::read(e.data.join("alice/files/new.txt")).unwrap(),
        b""
    );
    assert_eq!(
        max(&e, "LOCK", "/no/dir/x.txt", &[], b"").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        max(
            &e,
            "UNLOCK",
            "/f.txt",
            &[("lock-token", &l.h("lock-token"))],
            b""
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    // content is untouched by locking
    assert_eq!(max(&e, "GET", "/f.txt", &[], b"").await.body, b"abc");
    // PROPPATCH: nothing is writable -> 207 with 403 per property
    let pp = max(&e, "PROPPATCH", "/f.txt", &[], br#"<d:propertyupdate xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:set><d:prop><oc:favorite>1</oc:favorite></d:prop></d:set></d:propertyupdate>"#).await;
    assert_eq!(pp.status, StatusCode::MULTI_STATUS);
    assert!(pp.text().contains("<oc:favorite/>") && pp.text().contains("403 Forbidden"));
}
