//! Phase 5: previews, SEARCH/REPORT, mobile probes, auto-upload style requests.
//! Statuses/shapes follow probes of stock Nextcloud 35.0.3 (see tests/fixtures/nc35/).

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
    fn h(&self, n: &str) -> String {
        self.headers
            .get(n)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not json: {}", self.text()))
    }
    fn dims(&self) -> (u32, u32) {
        let i = image::load_from_memory(&self.body).unwrap_or_else(|e| panic!("not an image: {e}"));
        (i.width(), i.height())
    }
}

async fn call_as(
    e: &Env,
    auth: Option<(&str, &str)>,
    method: &str,
    uri: &str,
    hdrs: &[(&str, &str)],
    body: Vec<u8>,
) -> R {
    let mut b = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri);
    if let Some((u, p)) = auth {
        b = b.header(
            header::AUTHORIZATION,
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))
            ),
        );
    }
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
async fn call(e: &Env, method: &str, uri: &str, hdrs: &[(&str, &str)], body: Vec<u8>) -> R {
    call_as(e, Some(("alice", "pw")), method, uri, hdrs, body).await
}
async fn get(e: &Env, uri: &str) -> R {
    call(e, "GET", uri, &[("ocs-apirequest", "true")], vec![]).await
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 128, 255])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

async fn put(e: &Env, path: &str, body: Vec<u8>, mtime: &str) -> R {
    call(
        e,
        "PUT",
        &format!("/remote.php/dav/files/alice{path}"),
        &[("x-oc-mtime", mtime)],
        body,
    )
    .await
}

async fn fileid(e: &Env, path: &str) -> String {
    let r = call(
        e, "PROPFIND", &format!("/remote.php/dav/files/alice{path}"), &[("depth", "0")],
        br#"<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:fileid/></d:prop></d:propfind>"#.to_vec(),
    ).await;
    let t = r.text();
    let i = t.find("<oc:fileid>").unwrap() + 11;
    t[i..i + t[i..].find('<').unwrap()].to_string()
}

#[tokio::test]
async fn previews_sizes_cache_etag_and_errors() {
    let e = env().await;
    assert_eq!(
        put(&e, "/pic.png", png(800, 600), "1600000000")
            .await
            .status,
        StatusCode::CREATED
    );
    put(&e, "/note.txt", b"hello".to_vec(), "1600000000").await;
    call(&e, "MKCOL", "/remote.php/dav/files/alice/dir", &[], vec![]).await;

    // a=1&mode=cover: aspect kept, short side reaches 64 (stock: 85x64)
    let r = get(
        &e,
        "/index.php/core/preview.png?file=/pic.png&x=64&y=64&a=1&mode=cover&forceIcon=0",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.h("content-type"), "image/png");
    assert_eq!(r.h("cache-control"), "private, max-age=86400, immutable");
    assert_eq!(r.dims(), (85, 64));
    // a=1 without cover: fits inside
    assert_eq!(
        get(&e, "/index.php/core/preview?file=/pic.png&x=64&y=64&a=1")
            .await
            .dims(),
        (64, 48)
    );
    // no a: cropped to exactly x*y (stock: 64x64)
    assert_eq!(
        get(&e, "/index.php/core/preview.png?file=/pic.png&x=64&y=64")
            .await
            .dims(),
        (64, 64)
    );
    // never enlarged beyond the original
    assert_eq!(
        get(
            &e,
            "/index.php/core/preview?file=/pic.png&x=2000&y=2000&a=1"
        )
        .await
        .dims(),
        (800, 600)
    );
    // by file id
    let id = fileid(&e, "/pic.png").await;
    let by_id = get(
        &e,
        &format!("/index.php/core/preview?fileId={id}&x=128&y=128&a=true"),
    )
    .await;
    assert_eq!((by_id.status, by_id.dims()), (StatusCode::OK, (256, 192)));
    // cached on disk (outside every user folder), identical second answer, 304 on matching ETag
    let cached: Vec<_> = walk(&e.data.join(".appdata/preview"));
    assert!(cached.len() >= 4, "{cached:?}");
    let again = get(
        &e,
        &format!("/index.php/core/preview?fileId={id}&x=128&y=128&a=true"),
    )
    .await;
    assert_eq!(again.body, by_id.body);
    assert_eq!(again.h("etag"), by_id.h("etag"));
    let nm = call(
        &e,
        "GET",
        &format!("/index.php/core/preview?fileId={id}&x=128&y=128&a=true"),
        &[("if-none-match", &by_id.h("etag"))],
        vec![],
    )
    .await;
    assert_eq!(nm.status, StatusCode::NOT_MODIFIED);
    // editing the file changes the ETag: no stale preview
    put(&e, "/pic.png", png(200, 100), "1600000001").await;
    let edited = get(&e, "/index.php/core/preview?file=/pic.png&x=64&y=64&a=1").await;
    assert_eq!(edited.dims(), (64, 32));
    assert_ne!(edited.h("etag"), by_id.h("etag"));

    // non-image: generic icon (stock answers 200 with an icon), directory/missing: 404 []
    let icon = get(
        &e,
        "/index.php/core/preview.png?file=/note.txt&x=64&y=64&a=1",
    )
    .await;
    assert_eq!((icon.status, icon.dims()), (StatusCode::OK, (64, 64)));
    for p in [
        "/index.php/core/preview.png?file=/dir&x=64&y=64",
        "/index.php/core/preview.png?file=/nope.png&x=64&y=64",
        "/index.php/core/preview?fileId=999999&x=64&y=64",
    ] {
        let r = get(&e, p).await;
        assert_eq!(
            (r.status, r.text()),
            (StatusCode::NOT_FOUND, "[]".to_string()),
            "{p}"
        );
    }
    // bad parameters: 400 []
    for p in [
        "/index.php/core/preview.png?x=64&y=64",
        "/index.php/core/preview.png?file=/pic.png&x=0&y=0",
        "/index.php/core/preview.png?file=/pic.png&x=abc",
    ] {
        assert_eq!(get(&e, p).await.status, StatusCode::BAD_REQUEST, "{p}");
    }
    // auth required; another user cannot reach max's file ids
    assert_eq!(
        call_as(
            &e,
            None,
            "GET",
            "/index.php/core/preview.png?file=/pic.png&x=64&y=64",
            &[],
            vec![]
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    let eve = call_as(
        &e,
        Some(("eve", "pw2")),
        "GET",
        &format!("/index.php/core/preview?fileId={id}&x=64&y=64"),
        &[],
        vec![],
    )
    .await;
    assert_eq!(eve.status, StatusCode::NOT_FOUND);
    // thumbnail endpoint: crop, never cached by the client
    let t = get(&e, "/index.php/apps/files/api/v1/thumbnail/64/64/pic.png").await;
    assert_eq!((t.status, t.dims()), (StatusCode::OK, (64, 64)));
    assert_eq!(t.h("cache-control"), "no-cache, no-store, must-revalidate");
    // a corrupt "png" is a 404, not a crash
    put(&e, "/broken.png", b"not a png".to_vec(), "1600000000").await;
    assert_eq!(
        get(&e, "/index.php/core/preview.png?file=/broken.png&x=64&y=64")
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // PROPFIND: has-preview true only for decodable images
    let pf = call(&e, "PROPFIND", "/remote.php/dav/files/alice/", &[("depth", "1")],
        br#"<d:propfind xmlns:d="DAV:" xmlns:nc="http://nextcloud.org/ns"><d:prop><nc:has-preview/></d:prop></d:propfind>"#.to_vec()).await.text();
    let png_resp = &pf[pf.find("pic.png").unwrap()..];
    assert!(
        png_resp[..png_resp.find("</d:response>").unwrap()]
            .contains("<nc:has-preview>true</nc:has-preview>")
    );
    let txt_resp = &pf[pf.find("note.txt").unwrap()..];
    assert!(
        txt_resp[..txt_resp.find("</d:response>").unwrap()]
            .contains("<nc:has-preview>false</nc:has-preview>")
    );
}

fn walk(p: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v = vec![];
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                v.extend(walk(&e.path()))
            } else {
                v.push(e.path())
            }
        }
    }
    v
}

const MEDIA: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="no" ?>
<d:searchrequest xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns"><d:basicsearch><d:select><d:prop><oc:fileid/><d:getlastmodified/><d:getcontenttype/><d:getcontentlength/><nc:has-preview/><d:getetag/></d:prop></d:select><d:from><d:scope><d:href>/files/alice</d:href><d:depth>infinity</d:depth></d:scope></d:from><d:where><d:or><d:like><d:prop><d:getcontenttype/></d:prop><d:literal>image/%</d:literal></d:like><d:like><d:prop><d:getcontenttype/></d:prop><d:literal>video/%</d:literal></d:like></d:or></d:where><d:orderby><d:order><d:prop><d:getlastmodified/></d:prop><d:descending/></d:order></d:orderby><d:limit><d:nresults>LIMIT</d:nresults></d:limit></d:basicsearch></d:searchrequest>"#;

#[tokio::test]
async fn search_media_recent_and_report_favorites() {
    let e = env().await;
    call(
        &e,
        "MKCOL",
        "/remote.php/dav/files/alice/Photos",
        &[],
        vec![],
    )
    .await;
    put(&e, "/Photos/old.png", png(10, 10), "1000000000").await;
    put(&e, "/Photos/new.png", png(10, 10), "1700000000").await;
    put(&e, "/clip.mp4", vec![0; 10], "1500000000").await;
    put(&e, "/readme.txt", b"x".to_vec(), "1800000000").await;
    let s = |body: String| async {
        let r = call(
            &e,
            "SEARCH",
            "/remote.php/dav/",
            &[("content-type", "text/xml")],
            body.into_bytes(),
        )
        .await;
        (r.status, r.h("content-type"), r.text())
    };
    let (st, ct, xml) = s(MEDIA.replace("LIMIT", "100")).await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    assert_eq!(ct, "application/xml; charset=\"utf-8\"");
    let order: Vec<&str> = xml
        .match_indices("<d:href>")
        .map(|(i, _)| &xml[i + 8..i + xml[i..].find("</d:href>").unwrap()])
        .collect();
    assert_eq!(
        order,
        [
            "/remote.php/dav/files/alice/Photos/new.png",
            "/remote.php/dav/files/alice/clip.mp4",
            "/remote.php/dav/files/alice/Photos/old.png"
        ]
    );
    assert!(!xml.contains("readme.txt") && !xml.contains("/Photos/<")); // text and folders are filtered out
    assert!(
        xml.contains("<nc:has-preview>true</nc:has-preview>")
            && xml.contains("<d:getcontenttype>video/mp4")
    );
    // limit
    let (_, _, xml) = s(MEDIA.replace("LIMIT", "1")).await;
    assert_eq!(xml.matches("<d:response>").count(), 1);
    // "recent": everything newer than a timestamp, scoped to a folder, depth 1
    let recent = r#"<d:searchrequest xmlns:d="DAV:"><d:basicsearch><d:select><d:prop><d:displayname/></d:prop></d:select><d:from><d:scope><d:href>/files/alice/Photos</d:href><d:depth>1</d:depth></d:scope></d:from><d:where><d:gt><d:prop><d:getlastmodified/></d:prop><d:literal>1600000000</d:literal></d:gt></d:where></d:basicsearch></d:searchrequest>"#;
    let (_, _, xml) = s(recent.to_string()).await;
    assert!(xml.contains("new.png") && !xml.contains("old.png"));
    // someone else's tree / garbage / no auth
    let other = MEDIA.replace("/files/alice", "/files/eve");
    assert_eq!(s(other).await.0, StatusCode::FORBIDDEN);
    assert_eq!(s("<nope/>".into()).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        call_as(
            &e,
            None,
            "SEARCH",
            "/remote.php/dav/",
            &[],
            MEDIA.as_bytes().to_vec()
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );

    // REPORT filter-files (favourites): nothing is favourited -> empty multistatus; others -> 501
    let fav = br#"<?xml version="1.0"?><oc:filter-files xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:fileid/></d:prop><oc:filter-rules><oc:favorite>1</oc:favorite></oc:filter-rules></oc:filter-files>"#;
    let r = call(
        &e,
        "REPORT",
        "/remote.php/dav/files/alice/",
        &[],
        fav.to_vec(),
    )
    .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert!(!r.text().contains("<d:response>"));
    let r = call(
        &e,
        "REPORT",
        "/remote.php/dav/files/alice/",
        &[],
        br#"<x:other xmlns:x="urn:x"/>"#.to_vec(),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn mobile_probes_match_stock_shapes() {
    let e = env().await;
    put(&e, "/a.bin", vec![0; 1000], "1600000000").await;
    // notifications: empty list, single one 404, delete ok
    let n = get(
        &e,
        "/ocs/v2.php/apps/notifications/api/v2/notifications?format=json",
    )
    .await;
    assert_eq!(
        (n.status, n.json()["ocs"]["data"].clone()),
        (StatusCode::OK, serde_json::json!([]))
    );
    assert_eq!(
        get(
            &e,
            "/ocs/v2.php/apps/notifications/api/v2/notifications/1?format=json"
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let d = call(
        &e,
        "DELETE",
        "/ocs/v2.php/apps/notifications/api/v2/notifications/5?format=json",
        &[("ocs-apirequest", "true")],
        vec![],
    )
    .await;
    assert_eq!(d.status, StatusCode::OK);
    // user status: none set (404 + message), predefined list
    let us = get(
        &e,
        "/ocs/v2.php/apps/user_status/api/v1/user_status?format=json",
    )
    .await;
    assert_eq!(us.status, StatusCode::NOT_FOUND);
    assert_eq!(
        us.json()["ocs"]["meta"]["message"],
        "No status for the current user"
    );
    let pre = get(
        &e,
        "/ocs/v2.php/apps/user_status/api/v1/predefined_statuses?format=json",
    )
    .await;
    assert_eq!(pre.json()["ocs"]["data"][0]["id"], "meeting");
    // remote wipe: stock answers 404 [] for tokens that are not scheduled for a wipe
    for p in ["/index.php/core/wipe/check", "/index.php/core/wipe/success"] {
        let r = call_as(
            &e,
            None,
            "POST",
            p,
            &[("content-type", "application/x-www-form-urlencoded")],
            b"token=abc".to_vec(),
        )
        .await;
        assert_eq!(
            (r.status, r.text()),
            (StatusCode::NOT_FOUND, "[]".into()),
            "{p}"
        );
    }
    // shares: none yet (Phase 6)
    let sh = get(
        &e,
        "/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json&path=/a.bin&reshares=true",
    )
    .await;
    assert_eq!(
        (sh.status, sh.json()["ocs"]["data"].clone()),
        (StatusCode::OK, serde_json::json!([]))
    );
    // storage stats
    let st = get(&e, "/index.php/apps/files/api/v1/stats").await;
    assert_eq!(st.status, StatusCode::OK);
    let j = st.json();
    assert_eq!(
        (
            j["message"].clone(),
            j["data"]["used"].clone(),
            j["data"]["quota"].clone(),
            j["data"]["owner"].clone()
        ),
        ("ok".into(), 1000.into(), (-3).into(), "alice".into())
    );
    assert!(j["data"]["free"].as_u64().unwrap() > 0);
    assert_eq!(
        st.h("cache-control"),
        "private, max-age=300, must-revalidate"
    );
    // all of them need a login
    assert_eq!(
        call_as(
            &e,
            None,
            "GET",
            "/ocs/v2.php/apps/notifications/api/v2/notifications",
            &[],
            vec![]
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    // notifications are not advertised (no push proxy); bulk upload neither
    let c = get(&e, "/ocs/v1.php/cloud/capabilities?format=json")
        .await
        .json();
    let caps = &c["ocs"]["data"]["capabilities"];
    assert!(caps.get("notifications").is_none() && caps["dav"].get("bulkupload").is_none());
}

#[tokio::test]
async fn auto_upload_creates_nested_folders_and_keeps_the_camera_mtime() {
    let e = env().await;
    // what the mobile apps do: MKCOL each level, then PUT with X-OC-Mtime (and X-OC-CTime)
    for p in [
        "/InstantUpload",
        "/InstantUpload/Camera",
        "/InstantUpload/Camera/2026",
        "/InstantUpload/Camera/2026/11",
    ] {
        assert_eq!(
            call(
                &e,
                "MKCOL",
                &format!("/remote.php/dav/files/alice{p}"),
                &[],
                vec![]
            )
            .await
            .status,
            StatusCode::CREATED,
            "{p}"
        );
    }
    let r = call(
        &e,
        "PUT",
        "/remote.php/dav/files/alice/InstantUpload/Camera/2026/11/IMG_0001.jpg",
        &[
            ("x-oc-mtime", "1795000000"),
            ("x-oc-ctime", "1794999990"),
            ("if-none-match", "*"),
            ("content-type", "image/jpeg"),
        ],
        png(40, 30),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.h("x-oc-mtime"), "accepted");
    let g = call(
        &e,
        "GET",
        "/remote.php/dav/files/alice/InstantUpload/Camera/2026/11/IMG_0001.jpg",
        &[],
        vec![],
    )
    .await;
    assert_eq!(
        g.h("last-modified"),
        httpdate::fmt_http_date(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1795000000))
    );
    // a retried upload of the same file (If-None-Match: *) must not overwrite it
    let again = call(
        &e,
        "PUT",
        "/remote.php/dav/files/alice/InstantUpload/Camera/2026/11/IMG_0001.jpg",
        &[("if-none-match", "*")],
        b"other".to_vec(),
    )
    .await;
    assert_eq!(again.status, StatusCode::PRECONDITION_FAILED);
}
