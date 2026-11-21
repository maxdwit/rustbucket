//! Phase 8: trash bin and file versions (WebDAV `trashbin` / `versions`, storage rules, expiry).

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::routes::{AppState, router};
use rustbucket::storage::{Owner, Store};
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
    app: Router,
    data: std::path::PathBuf,
    pool: sqlx::SqlitePool,
}

async fn env_with(versions_max: u32) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let pool = db::connect(&format!("sqlite://{}", dir.path().join("t.db").display()))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    for u in ["alice", "eve"] {
        db::create_user(&pool, u, &format!("{u} X"), &format!("pw-{u}"), false)
            .await
            .unwrap();
    }
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join(".rustbucket-data"), "").unwrap();
    let cfg = Config {
        data_dir: data.clone(),
        versions_max,
        ..Config::default()
    };
    Env {
        app: router(AppState::new(pool.clone(), Arc::new(cfg))),
        data,
        pool,
        _dir: dir,
    }
}
async fn env() -> Env {
    env_with(50).await
}

struct R {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}
impl R {
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
    method: &str,
    uri: &str,
    hdrs: &[(&str, &str)],
    body: &str,
) -> R {
    let mut b = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri)
        .header(
            "authorization",
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:pw-{user}"))
            ),
        );
    for (k, v) in hdrs {
        b = b.header(*k, *v);
    }
    let r = e
        .app
        .clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let (status, headers) = (r.status(), r.headers().clone());
    R {
        status,
        headers,
        body: String::from_utf8_lossy(&r.into_body().collect().await.unwrap().to_bytes())
            .into_owned(),
    }
}
/// Percent-encode like a client does (keeps `/`, letters, digits and `._-`).
fn enc(p: &str) -> String {
    p.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"/._-".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
async fn f(e: &Env, user: &str, method: &str, path: &str, body: &str) -> R {
    call(
        e,
        user,
        method,
        &format!("/remote.php/dav/files/{user}{}", enc(path)),
        &[],
        body,
    )
    .await
}
async fn put_mt(e: &Env, user: &str, path: &str, body: &str, mtime: &str) -> R {
    call(
        e,
        user,
        "PUT",
        &format!("/remote.php/dav/files/{user}{path}"),
        &[("x-oc-mtime", mtime)],
        body,
    )
    .await
}
async fn mv(e: &Env, user: &str, from: &str, to: &str) -> R {
    call(
        e,
        user,
        "MOVE",
        from,
        &[("destination", &format!("http://x{to}"))],
        "",
    )
    .await
}

const PF: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns"><d:prop><nc:trashbin-filename/><nc:trashbin-original-location/><nc:trashbin-deletion-time/><nc:trashbin-deleted-by-id/><d:getcontentlength/><d:getcontenttype/><d:resourcetype/><oc:fileid/><d:getlastmodified/><d:getetag/><oc:permissions/><oc:size/><d:displayname/><nc:version-author/></d:prop></d:propfind>"#;

fn blocks(xml: &str) -> Vec<(String, String)> {
    xml.split("<d:response>")
        .skip(1)
        .map(|b| {
            (
                b[b.find("<d:href>").unwrap() + 8..b.find("</d:href>").unwrap()].to_string(),
                b.to_string(),
            )
        })
        .collect()
}
fn prop(b: &str, name: &str) -> String {
    match b.find(&format!("<{name}>")) {
        Some(i) => b[i + name.len() + 2..i + b[i..].find("</").unwrap()].to_string(),
        None => String::new(),
    }
}
async fn pf(e: &Env, user: &str, uri: &str, depth: &str) -> (StatusCode, Vec<(String, String)>) {
    let r = call(e, user, "PROPFIND", uri, &[("depth", depth)], PF).await;
    (r.status, blocks(&r.body))
}
fn trash(user: &str) -> String {
    format!("/remote.php/dav/trashbin/{user}/trash")
}
fn restore(user: &str) -> String {
    format!("/remote.php/dav/trashbin/{user}/restore/x")
}
/// hrefs of the entries (without the collection itself) in the trash
async fn entries(e: &Env, user: &str) -> Vec<(String, String)> {
    let (st, v) = pf(e, user, &format!("{}/", trash(user)), "1").await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    v.into_iter().skip(1).collect()
}
fn entry_href<'a>(v: &'a [(String, String)], name: &str) -> &'a str {
    &v.iter()
        .find(|(h, _)| {
            percent_encoding::percent_decode_str(h)
                .decode_utf8_lossy()
                .contains(&format!("/trash/{name}.d"))
        })
        .unwrap_or_else(|| {
            panic!(
                "no {name} in {:?}",
                v.iter().map(|x| &x.0).collect::<Vec<_>>()
            )
        })
        .0
}
async fn scan_clean(e: &Env, user: &str) {
    let id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE uid = $1")
        .bind(user)
        .fetch_one(&e.pool)
        .await
        .unwrap();
    let store = Store::new(e.pool.clone(), e.data.clone());
    let rep = store
        .scan(
            &Owner {
                id,
                uid: user.into(),
            },
            true,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        (rep.added, rep.updated, rep.removed),
        (0, 0, 0),
        "index != disk for {user}"
    );
}
async fn fileid(e: &Env, user: &str, path: &str) -> String {
    let r = call(
        e,
        user,
        "PROPFIND",
        &format!("/remote.php/dav/files/{user}{path}"),
        &[("depth", "0")],
        PF,
    )
    .await;
    prop(&r.body, "oc:fileid")
}
async fn size_of(e: &Env, user: &str, path: &str) -> String {
    let r = call(
        e,
        user,
        "PROPFIND",
        &format!("/remote.php/dav/files/{user}{path}"),
        &[("depth", "0")],
        PF,
    )
    .await;
    prop(&r.body, "oc:size")
}

#[tokio::test]
async fn deleting_a_file_moves_it_to_the_trash_and_restore_brings_it_back() {
    let e = env().await;
    f(&e, "alice", "MKCOL", "/Docs", "").await;
    put_mt(&e, "alice", "/Docs/a.txt", "precious", "1700000000").await;
    let size_before = size_of(&e, "alice", "/").await;
    let old_etag = prop(
        &call(
            &e,
            "alice",
            "PROPFIND",
            "/remote.php/dav/files/alice/Docs",
            &[("depth", "0")],
            PF,
        )
        .await
        .body,
        "d:getetag",
    );
    assert_eq!(
        f(&e, "alice", "DELETE", "/Docs/a.txt", "").await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        f(&e, "alice", "GET", "/Docs/a.txt", "").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(size_of(&e, "alice", "/").await, "0"); // trash does not count in the live tree
    let v = entries(&e, "alice").await;
    assert_eq!(v.len(), 1);
    let (href, b) = &v[0];
    assert!(
        href.starts_with("/remote.php/dav/trashbin/alice/trash/a.txt.d"),
        "{href}"
    );
    assert_eq!(
        (
            prop(b, "nc:trashbin-filename").as_str(),
            prop(b, "nc:trashbin-original-location").as_str(),
            prop(b, "nc:trashbin-deleted-by-id").as_str()
        ),
        ("a.txt", "Docs/a.txt", "alice")
    );
    assert!(prop(b, "nc:trashbin-deletion-time").parse::<i64>().unwrap() > 1_700_000_000);
    assert_eq!(
        (
            prop(b, "d:getcontentlength").as_str(),
            prop(b, "d:getcontenttype").as_str(),
            prop(b, "oc:permissions").as_str(),
            prop(b, "oc:size").as_str(),
            prop(b, "d:displayname").as_str()
        ),
        ("8", "text/plain", "GD", "8", "a.txt")
    );
    // stock shows the deletion time, not the file's own mtime
    let at: u64 = prop(b, "nc:trashbin-deletion-time").parse().unwrap();
    assert_eq!(
        prop(b, "d:getlastmodified"),
        httpdate::fmt_http_date(std::time::UNIX_EPOCH + std::time::Duration::from_secs(at))
    );
    assert!(
        b.contains("<d:getetag/>") || b.contains("<d:getetag></d:getetag>"),
        "stock shows an empty etag for files"
    );
    // content can still be read
    let g = call(&e, "alice", "GET", href, &[], "").await;
    assert_eq!((g.status, g.body.as_str()), (StatusCode::OK, "precious"));
    // restore (MOVE into restore/): original place, same content and mtime, entry gone
    assert_eq!(
        mv(&e, "alice", href, &restore("alice")).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        f(&e, "alice", "GET", "/Docs/a.txt", "").await.body,
        "precious"
    );
    assert!(entries(&e, "alice").await.is_empty());
    assert_eq!(size_of(&e, "alice", "/Docs").await, "8");
    assert_ne!(
        prop(
            &call(
                &e,
                "alice",
                "PROPFIND",
                "/remote.php/dav/files/alice/Docs",
                &[("depth", "0")],
                PF
            )
            .await
            .body,
            "d:getetag"
        ),
        old_etag,
        "parent etag changes so sync clients notice"
    );
    assert_eq!(size_of(&e, "alice", "/").await, size_before);
    assert!(e.data.join("alice/files/Docs/a.txt").exists());
    assert!(
        std::fs::read_dir(e.data.join("alice/trash")).map_or(true, |mut d| d.next().is_none()),
        "no trash leftovers"
    );
    scan_clean(&e, "alice").await;
}

#[tokio::test]
async fn trashed_folders_keep_their_tree_and_restore_completely() {
    let e = env().await;
    f(&e, "alice", "MKCOL", "/folder", "").await;
    f(&e, "alice", "PUT", "/folder/b.txt", "inside").await;
    f(&e, "alice", "MKCOL", "/folder/sub", "").await;
    f(&e, "alice", "PUT", "/folder/sub/c.txt", "deep").await;
    f(&e, "alice", "PUT", "/keep.txt", "x").await;
    let fid = fileid(&e, "alice", "/folder").await;
    assert_eq!(
        f(&e, "alice", "DELETE", "/folder", "").await.status,
        StatusCode::NO_CONTENT
    );
    let v = entries(&e, "alice").await;
    assert_eq!(v.len(), 1, "one entry for the folder, not one per file");
    let href = v[0].0.clone();
    assert!(href.ends_with('/') && v[0].1.contains("<d:collection/>"));
    assert_eq!(
        (prop(&v[0].1, "oc:fileid"), prop(&v[0].1, "oc:size")),
        (fid, "10".to_string())
    );
    // browse inside the trashed folder
    let (_, inside) = pf(&e, "alice", &href, "1").await;
    let names: Vec<&str> = inside
        .iter()
        .map(|(h, _)| h.trim_end_matches('/').rsplit('/').next().unwrap())
        .collect();
    assert_eq!(&names[1..], ["b.txt", "sub"]);
    let (_, deeper) = pf(&e, "alice", &format!("{href}sub/"), "1").await;
    assert!(deeper.iter().any(|(h, _)| h.ends_with("/sub/c.txt")));
    assert_eq!(
        call(&e, "alice", "GET", &format!("{href}sub/c.txt"), &[], "")
            .await
            .body,
        "deep"
    );
    // path tricks cannot leave the trashed folder
    for bad in ["../x", "sub/../../x"] {
        let r = call(&e, "alice", "GET", &format!("{href}{bad}"), &[], "").await;
        assert!(
            r.status == StatusCode::NOT_FOUND
                || r.status == StatusCode::BAD_REQUEST
                || r.status == StatusCode::METHOD_NOT_ALLOWED,
            "{bad}: {}",
            r.status
        );
    }
    // individual items of a trashed folder cannot be deleted on their own
    assert_eq!(
        call(&e, "alice", "DELETE", &format!("{href}b.txt"), &[], "")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // restore the whole thing
    assert_eq!(
        mv(&e, "alice", &href, &restore("alice")).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        f(&e, "alice", "GET", "/folder/b.txt", "").await.body,
        "inside"
    );
    assert_eq!(
        f(&e, "alice", "GET", "/folder/sub/c.txt", "").await.body,
        "deep"
    );
    assert_eq!(size_of(&e, "alice", "/folder").await, "10");
    assert_eq!(size_of(&e, "alice", "/").await, "11");
    assert!(entries(&e, "alice").await.is_empty());
    scan_clean(&e, "alice").await;
}

#[tokio::test]
async fn restore_into_a_taken_name_or_a_missing_folder() {
    let e = env().await;
    // name taken: "dup (restored).txt", then "(restored 2)"; the existing file is never touched
    f(&e, "alice", "PUT", "/dup.txt", "first").await;
    f(&e, "alice", "DELETE", "/dup.txt", "").await;
    f(&e, "alice", "PUT", "/dup.txt", "second").await;
    let v = entries(&e, "alice").await;
    assert_eq!(
        mv(&e, "alice", entry_href(&v, "dup.txt"), &restore("alice"))
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        (
            f(&e, "alice", "GET", "/dup.txt", "").await.body.as_str(),
            f(&e, "alice", "GET", "/dup (restored).txt", "")
                .await
                .body
                .as_str()
        ),
        ("second", "first")
    );
    f(&e, "alice", "DELETE", "/dup (restored).txt", "").await;
    f(&e, "alice", "PUT", "/dup (restored).txt", "third").await;
    let v = entries(&e, "alice").await;
    assert_eq!(
        mv(
            &e,
            "alice",
            entry_href(&v, "dup (restored).txt"),
            &restore("alice")
        )
        .await
        .status,
        StatusCode::CREATED
    );
    assert_eq!(
        f(&e, "alice", "GET", "/dup (restored) (restored).txt", "")
            .await
            .status,
        StatusCode::OK
    );
    // parent gone: restored at the top level
    f(&e, "alice", "MKCOL", "/tmpdir", "").await;
    f(&e, "alice", "PUT", "/tmpdir/o.txt", "orphan").await;
    f(&e, "alice", "DELETE", "/tmpdir/o.txt", "").await;
    f(&e, "alice", "DELETE", "/tmpdir", "").await;
    let v = entries(&e, "alice").await;
    let o = v.iter().find(|(h, _)| h.contains("/o.txt.d")).unwrap();
    assert_eq!(prop(&o.1, "nc:trashbin-original-location"), "tmpdir/o.txt");
    assert_eq!(
        mv(&e, "alice", &o.0, &restore("alice")).await.status,
        StatusCode::CREATED
    );
    assert_eq!(f(&e, "alice", "GET", "/o.txt", "").await.body, "orphan");
    // parent exists again later: it goes back there
    f(&e, "alice", "MKCOL", "/back", "").await;
    f(&e, "alice", "PUT", "/back/p.txt", "p").await;
    f(&e, "alice", "DELETE", "/back/p.txt", "").await;
    let v = entries(&e, "alice").await;
    let p = entry_href(&v, "p.txt").to_string();
    mv(&e, "alice", &p, &restore("alice")).await;
    assert_eq!(f(&e, "alice", "GET", "/back/p.txt", "").await.body, "p");
    scan_clean(&e, "alice").await;
}

#[tokio::test]
async fn delete_forever_empty_and_the_error_cases() {
    let e = env().await;
    for n in ["a", "b", "c"] {
        f(&e, "alice", "PUT", &format!("/{n}.txt"), n).await;
        f(&e, "alice", "DELETE", &format!("/{n}.txt"), "").await;
    }
    let v = entries(&e, "alice").await;
    assert_eq!(v.len(), 3);
    let a = entry_href(&v, "a.txt").to_string();
    assert_eq!(
        call(&e, "alice", "DELETE", &a, &[], "").await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&e, "alice", "DELETE", &a, &[], "").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(entries(&e, "alice").await.len(), 2);
    // restore of something that is not there; a MOVE anywhere but restore/ is refused (stock: 404)
    assert_eq!(
        mv(
            &e,
            "alice",
            &format!("{}/nope.d123", trash("alice")),
            &restore("alice")
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let b = entry_href(&v, "b.txt").to_string();
    // a MOVE to a path in the files tree restores the item THERE (stock): parent must exist, name must be free
    assert_eq!(
        mv(
            &e,
            "alice",
            &b,
            "/remote.php/dav/files/alice/no/such/dir/b.txt"
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    f(&e, "alice", "PUT", "/taken.txt", "existing").await;
    assert_eq!(
        mv(&e, "alice", &b, "/remote.php/dav/files/alice/taken.txt")
            .await
            .status,
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(
        f(&e, "alice", "GET", "/taken.txt", "").await.body,
        "existing"
    );
    assert_eq!(
        entries(&e, "alice").await.len(),
        2,
        "failed restores keep the entry"
    );
    assert_eq!(
        mv(&e, "alice", &b, "/remote.php/dav/files/alice/elsewhere.txt")
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(f(&e, "alice", "GET", "/elsewhere.txt", "").await.body, "b");
    // a destination outside the user's files is refused
    let c = entry_href(&entries(&e, "alice").await, "c.txt").to_string();
    assert_eq!(
        mv(&e, "alice", &c, "/remote.php/dav/files/eve/c.txt")
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        entries(&e, "alice").await.len(),
        1,
        "only c.txt is left in the bin"
    );
    // nobody else sees or touches this trash
    assert_eq!(
        call(
            &e,
            "eve",
            "PROPFIND",
            &format!("{}/", trash("alice")),
            &[("depth", "1")],
            PF
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&e, "eve", "GET", &b, &[], "").await.status,
        StatusCode::NOT_FOUND
    );
    assert!(entries(&e, "eve").await.is_empty());
    // the trash root lists restore/ and trash/
    let (_, root) = pf(&e, "alice", "/remote.php/dav/trashbin/alice/", "1").await;
    let hrefs: Vec<&str> = root.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(
        hrefs,
        [
            "/remote.php/dav/trashbin/alice/",
            "/remote.php/dav/trashbin/alice/restore/",
            "/remote.php/dav/trashbin/alice/trash/"
        ]
    );
    // empty the bin: one DELETE on the collection, the disk is cleaned too
    assert_eq!(
        call(
            &e,
            "alice",
            "DELETE",
            &format!("{}/", trash("alice")),
            &[],
            ""
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert!(entries(&e, "alice").await.is_empty());
    assert!(std::fs::read_dir(e.data.join("alice/trash")).map_or(true, |mut d| d.next().is_none()));
    // capabilities announce it
    let c = call(
        &e,
        "alice",
        "GET",
        "/ocs/v1.php/cloud/capabilities?format=json",
        &[],
        "",
    )
    .await
    .body;
    assert!(c.contains("\"undelete\":true") && c.contains("\"versioning\":true"));
}

#[tokio::test]
async fn same_name_deleted_in_the_same_second_gets_distinct_entries() {
    let e = env().await;
    for i in 0..3 {
        f(&e, "alice", "PUT", "/same.txt", &format!("v{i}")).await;
        f(&e, "alice", "DELETE", "/same.txt", "").await;
    }
    let v = entries(&e, "alice").await;
    let mut names: Vec<String> = v.iter().map(|(h, _)| h.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 3, "{names:?}");
    // each restores to its own content, one after the other
    let mut got = Vec::new();
    for (h, _) in &v {
        mv(&e, "alice", h, &restore("alice")).await;
    }
    for p in [
        "/same.txt",
        "/same (restored).txt",
        "/same (restored 2).txt",
    ] {
        got.push(f(&e, "alice", "GET", p, "").await.body);
    }
    got.sort();
    assert_eq!(got, ["v0", "v1", "v2"]);
}

#[tokio::test]
async fn overwriting_by_move_keeps_the_old_file_in_the_trash_but_cross_share_moves_do_not() {
    let e = env().await;
    f(&e, "alice", "PUT", "/new.txt", "new").await;
    f(&e, "alice", "PUT", "/target.txt", "old").await;
    let r = call(
        &e,
        "alice",
        "MOVE",
        "/remote.php/dav/files/alice/new.txt",
        &[
            (
                "destination",
                "http://x/remote.php/dav/files/alice/target.txt",
            ),
            ("overwrite", "T"),
        ],
        "",
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(f(&e, "alice", "GET", "/target.txt", "").await.body, "new");
    let v = entries(&e, "alice").await;
    assert_eq!(
        call(&e, "alice", "GET", entry_href(&v, "target.txt"), &[], "")
            .await
            .body,
        "old"
    );
    // moving a file out of a share is a move, not a delete: nothing lands in the owner's trash
    f(&e, "alice", "MKCOL", "/shared", "").await;
    f(&e, "alice", "PUT", "/shared/m.txt", "moved").await;
    let s = call(
        &e,
        "alice",
        "POST",
        "/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json",
        &[("content-type", "application/x-www-form-urlencoded")],
        "shareType=0&shareWith=eve&path=/shared&permissions=15",
    )
    .await;
    assert_eq!(s.status, StatusCode::OK, "{}", s.body);
    assert_eq!(
        mv(
            &e,
            "eve",
            "/remote.php/dav/files/eve/shared/m.txt",
            "/remote.php/dav/files/eve/mine.txt"
        )
        .await
        .status,
        StatusCode::CREATED
    );
    assert_eq!(
        entries(&e, "alice").await.len(),
        1,
        "only the overwritten file"
    );
    // removing a share mount is an unshare, not a trash entry; deleting inside a share trashes in the OWNER's bin
    f(&e, "alice", "PUT", "/shared/x.txt", "x").await;
    assert_eq!(
        f(&e, "eve", "DELETE", "/shared/x.txt", "").await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(entries(&e, "alice").await.len(), 2);
    assert!(entries(&e, "eve").await.is_empty());
    assert_eq!(
        f(&e, "eve", "DELETE", "/shared", "").await.status,
        StatusCode::NO_CONTENT
    );
    assert!(entries(&e, "eve").await.is_empty());
    scan_clean(&e, "alice").await;
    scan_clean(&e, "eve").await;
}

#[tokio::test]
async fn trash_expires_after_the_retention_time_and_leftovers_are_cleaned() {
    let e = env().await;
    for n in ["old", "fresh"] {
        f(&e, "alice", "PUT", &format!("/{n}.txt"), n).await;
        f(&e, "alice", "DELETE", &format!("/{n}.txt"), "").await;
    }
    // age one entry by 40 days
    sqlx::query(
        "UPDATE trash_items SET deleted_at = deleted_at - 40 * 86400 WHERE name = 'old.txt'",
    )
    .execute(&e.pool)
    .await
    .unwrap();
    // orphaned slots (crash/rollback between the rename and the commit): with a meta.json the item is adopted
    // back into the trash, without one it is left alone - expiry never deletes data it does not understand
    let adopt = e.data.join("alice/trash/9999");
    std::fs::create_dir_all(&adopt).unwrap();
    std::fs::write(adopt.join("item"), "rescued").unwrap();
    std::fs::write(adopt.join("meta.json"), r#"{"name":"rescued.txt","orig_dir":"Docs","deleted_at":1700000000,"is_dir":false,"size":7,"mtime":1600000000,"mimetype":"text/plain","orig_fileid":5}"#).unwrap();
    filetime_set_old(&adopt);
    let stray = e.data.join("alice/trash/9998");
    std::fs::create_dir_all(&stray).unwrap();
    std::fs::write(stray.join("item"), "unknown").unwrap();
    filetime_set_old(&stray);
    let store = Store::new(e.pool.clone(), e.data.clone());
    let (trashed, _) = store.expire(30, 90).await.unwrap();
    assert_eq!(trashed, 1, "the 40 day old entry");
    let names = |v: Vec<(String, String)>| v.into_iter().map(|(h, _)| h).collect::<Vec<_>>();
    let left = names(entries(&e, "alice").await);
    assert_eq!(left.len(), 2, "fresh + the adopted orphan: {left:?}");
    assert!(left.iter().any(|h| h.contains("fresh.txt.d")));
    assert!(
        left.iter().any(|h| h.contains("/rescued.txt.d1700000000")),
        "{left:?}"
    );
    assert!(
        stray.join("item").exists(),
        "a slot without meta.json is never deleted"
    );
    assert_eq!(
        std::fs::read_to_string(adopt.join("item")).unwrap(),
        "rescued"
    );
    // the adopted item can be restored like any other (its folder 'Docs' does not exist: top level)
    let rescued = left.iter().find(|h| h.contains("rescued")).unwrap().clone();
    assert_eq!(
        mv(&e, "alice", &rescued, &restore("alice")).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        f(&e, "alice", "GET", "/rescued.txt", "").await.body,
        "rescued"
    );
    // the next expiry applies the retention to what is left; 0 days keeps the trash forever
    assert_eq!(
        store.expire(0, 0).await.unwrap().0,
        0,
        "0 days = keep the trash forever"
    );
    assert_eq!(entries(&e, "alice").await.len(), 1);
}

fn filetime_set_old(p: &std::path::Path) {
    let f = std::fs::File::open(p).unwrap();
    f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
        .unwrap();
}

// ---------------------------------------------------------------------------------------- versions

async fn versions(e: &Env, user: &str, id: &str) -> Vec<(i64, String)> {
    let (st, v) = pf(
        e,
        user,
        &format!("/remote.php/dav/versions/{user}/versions/{id}"),
        "1",
    )
    .await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    v.into_iter()
        .skip(1)
        .map(|(h, b)| {
            (
                h.rsplit('/').next().unwrap().parse().unwrap(),
                prop(&b, "d:getcontentlength"),
            )
        })
        .collect()
}

#[tokio::test]
async fn every_overwrite_keeps_the_previous_content_as_a_version() {
    let e = env().await;
    put_mt(&e, "alice", "/v.txt", "content one", "1700000100").await;
    let id = fileid(&e, "alice", "/v.txt").await;
    // a never-overwritten file lists itself as its only version (stock does the same)
    assert_eq!(
        versions(&e, "alice", &id).await,
        [(1700000100, "11".to_string())]
    );
    put_mt(&e, "alice", "/v.txt", "content two longer", "1700000200").await;
    put_mt(&e, "alice", "/v.txt", "content three", "1700000300").await;
    let list = versions(&e, "alice", &id).await;
    assert_eq!(
        list,
        [
            (1700000100, "11".into()),
            (1700000200, "18".into()),
            (1700000300, "13".into())
        ]
    );
    let (_, v) = pf(
        &e,
        "alice",
        &format!("/remote.php/dav/versions/alice/versions/{id}"),
        "1",
    )
    .await;
    let one = &v[1].1;
    assert_eq!(
        (
            prop(one, "d:getetag").as_str(),
            prop(one, "nc:version-author").as_str(),
            prop(one, "d:getcontenttype").as_str(),
            prop(one, "d:getlastmodified").as_str()
        ),
        (
            "1700000100",
            "alice",
            "text/plain",
            "Tue, 14 Nov 2023 22:15:00 GMT"
        )
    );
    assert!(one.contains("<d:resourcetype/>"));
    // every version can be read, the newest is the live file
    let get = |ts: i64| {
        let e = &e;
        let id = id.clone();
        async move {
            call(
                e,
                "alice",
                "GET",
                &format!("/remote.php/dav/versions/alice/versions/{id}/{ts}"),
                &[],
                "",
            )
            .await
            .body
        }
    };
    assert_eq!(
        (
            get(1700000100).await.as_str(),
            get(1700000200).await.as_str(),
            get(1700000300).await.as_str()
        ),
        ("content one", "content two longer", "content three")
    );
    // the live file keeps its fileid through overwrites
    assert_eq!(fileid(&e, "alice", "/v.txt").await, id);
    // restore the oldest: content, mtime and a NEW etag; what was live becomes a version again
    let etag_before = prop(
        &call(
            &e,
            "alice",
            "PROPFIND",
            "/remote.php/dav/files/alice/v.txt",
            &[("depth", "0")],
            PF,
        )
        .await
        .body,
        "d:getetag",
    );
    let r = mv(
        &e,
        "alice",
        &format!("/remote.php/dav/versions/alice/versions/{id}/1700000100"),
        "/remote.php/dav/versions/alice/restore/target",
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(
        f(&e, "alice", "GET", "/v.txt", "").await.body,
        "content one"
    );
    let after = call(
        &e,
        "alice",
        "PROPFIND",
        "/remote.php/dav/files/alice/v.txt",
        &[("depth", "0")],
        PF,
    )
    .await
    .body;
    assert_ne!(prop(&after, "d:getetag"), etag_before);
    assert_eq!(
        prop(&after, "d:getlastmodified"),
        "Tue, 14 Nov 2023 22:15:00 GMT"
    );
    assert_eq!(
        versions(&e, "alice", &id)
            .await
            .iter()
            .map(|v| v.0)
            .collect::<Vec<_>>(),
        [1700000100, 1700000200, 1700000300]
    );
    assert_eq!(
        get(1700000300).await,
        "content three",
        "the content that was live is still available"
    );
    // restoring the current version changes nothing; unknown versions/ids/users are 404
    assert_eq!(
        mv(
            &e,
            "alice",
            &format!("/remote.php/dav/versions/alice/versions/{id}/1700000100"),
            "/remote.php/dav/versions/alice/restore/t"
        )
        .await
        .status,
        StatusCode::CREATED
    );
    assert_eq!(
        call(
            &e,
            "alice",
            "GET",
            &format!("/remote.php/dav/versions/alice/versions/{id}/42"),
            &[],
            ""
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &e,
            "alice",
            "PROPFIND",
            "/remote.php/dav/versions/alice/versions/999999",
            &[("depth", "1")],
            PF
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &e,
            "eve",
            "PROPFIND",
            &format!("/remote.php/dav/versions/alice/versions/{id}"),
            &[("depth", "1")],
            PF
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &e,
            "eve",
            "PROPFIND",
            &format!("/remote.php/dav/versions/eve/versions/{id}"),
            &[("depth", "1")],
            PF
        )
        .await
        .status,
        StatusCode::NOT_FOUND,
        "another user's file id"
    );
    // a MOVE to anywhere but restore/ is refused
    assert_eq!(
        mv(
            &e,
            "alice",
            &format!("/remote.php/dav/versions/alice/versions/{id}/1700000200"),
            "/remote.php/dav/files/alice/z.txt"
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    // delete an old version; the current one cannot be deleted
    assert_eq!(
        call(
            &e,
            "alice",
            "DELETE",
            &format!("/remote.php/dav/versions/alice/versions/{id}/1700000200"),
            &[],
            ""
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &e,
            "alice",
            "DELETE",
            &format!("/remote.php/dav/versions/alice/versions/{id}/1700000100"),
            &[],
            ""
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        versions(&e, "alice", &id)
            .await
            .iter()
            .map(|v| v.0)
            .collect::<Vec<_>>(),
        [1700000100, 1700000300]
    );
    scan_clean(&e, "alice").await;
}

#[tokio::test]
async fn versions_cost_no_extra_space_empty_files_and_equal_mtimes_make_none_and_the_count_is_capped()
 {
    let e = env_with(3).await;
    put_mt(&e, "alice", "/big.bin", "aaaaaaaaaa", "1700000001").await;
    let id = fileid(&e, "alice", "/big.bin").await;
    put_mt(&e, "alice", "/big.bin", "bbbbbbbbbb", "1700000002").await;
    // the version is a hard link of the old content: same inode, no copy
    use std::os::unix::fs::MetadataExt;
    let vp = e.data.join(format!("alice/versions/{id}/1700000001"));
    assert_eq!(
        std::fs::metadata(&vp).unwrap().nlink(),
        1,
        "the live file is a different inode now; the link keeps the old one alive"
    );
    assert_eq!(std::fs::read_to_string(&vp).unwrap(), "aaaaaaaaaa");
    // same mtime as before: not a new version; empty content: not worth one
    put_mt(&e, "alice", "/big.bin", "cccccccccc", "1700000002").await;
    assert_eq!(versions(&e, "alice", &id).await.len(), 2);
    put_mt(&e, "alice", "/empty.txt", "", "1700000010").await;
    put_mt(&e, "alice", "/empty.txt", "now not empty", "1700000020").await;
    let eid = fileid(&e, "alice", "/empty.txt").await;
    assert_eq!(versions(&e, "alice", &eid).await.len(), 1);
    // at most 3 old versions per file: the oldest go first (the live one is listed on top of them)
    for i in 3..=8 {
        put_mt(
            &e,
            "alice",
            "/big.bin",
            &format!("generation {i}"),
            &format!("17000000{i:02}"),
        )
        .await;
    }
    let list = versions(&e, "alice", &id).await;
    assert_eq!(list.len(), 4, "3 old + live: {list:?}");
    assert_eq!(list.last().unwrap().0, 1700000008);
    assert!(list.iter().all(|v| v.0 >= 1700000005));
}

#[tokio::test]
async fn versions_travel_with_a_trashed_file_and_vanish_with_a_permanent_delete() {
    let e = env().await;
    put_mt(&e, "alice", "/t.txt", "one", "1700000001").await;
    put_mt(&e, "alice", "/t.txt", "two", "1700000002").await;
    let id = fileid(&e, "alice", "/t.txt").await;
    f(&e, "alice", "DELETE", "/t.txt", "").await;
    let v = entries(&e, "alice").await;
    // restored: new fileid, but the history is back
    mv(&e, "alice", &v[0].0, &restore("alice")).await;
    let nid = fileid(&e, "alice", "/t.txt").await;
    assert_ne!(nid, id);
    assert_eq!(
        versions(&e, "alice", &nid)
            .await
            .iter()
            .map(|x| x.0)
            .collect::<Vec<_>>(),
        [1700000001, 1700000002]
    );
    assert!(!e.data.join(format!("alice/versions/{id}")).exists());
    // deleted for good (emptied trash): no versions left behind, and the sweeper finds orphans
    f(&e, "alice", "DELETE", "/t.txt", "").await;
    call(
        &e,
        "alice",
        "DELETE",
        &format!("{}/", trash("alice")),
        &[],
        "",
    )
    .await;
    let store = Store::new(e.pool.clone(), e.data.clone());
    let stray = e.data.join("alice/versions/424242");
    std::fs::create_dir_all(&stray).unwrap();
    std::fs::write(stray.join("1700000000"), "stray").unwrap();
    let (_, removed) = store.expire(30, 90).await.unwrap();
    assert!(removed >= 1);
    assert!(!stray.exists());
    assert!(
        std::fs::read_dir(e.data.join("alice/versions")).map_or(true, |mut d| d.next().is_none()),
        "no orphaned versions"
    );
    // deleting a folder keeps no versions of what was inside (they cannot be addressed any more)
    f(&e, "alice", "MKCOL", "/d", "").await;
    put_mt(&e, "alice", "/d/i.txt", "1", "1700000001").await;
    put_mt(&e, "alice", "/d/i.txt", "2", "1700000002").await;
    f(&e, "alice", "DELETE", "/d", "").await;
    assert!(
        std::fs::read_dir(e.data.join("alice/versions")).map_or(true, |mut d| d.next().is_none())
    );
}

#[tokio::test]
async fn a_recipient_overwriting_a_shared_file_creates_a_version_for_the_owner() {
    let e = env().await;
    f(&e, "alice", "MKCOL", "/s", "").await;
    put_mt(&e, "alice", "/s/doc.txt", "owner version", "1700000001").await;
    let s = call(
        &e,
        "alice",
        "POST",
        "/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json",
        &[("content-type", "application/x-www-form-urlencoded")],
        "shareType=0&shareWith=eve&path=/s&permissions=15",
    )
    .await;
    assert_eq!(s.status, StatusCode::OK);
    put_mt(&e, "eve", "/s/doc.txt", "recipient edit", "1700000002").await;
    let id = fileid(&e, "alice", "/s/doc.txt").await;
    let list = versions(&e, "alice", &id).await;
    assert_eq!(
        list.iter().map(|v| v.0).collect::<Vec<_>>(),
        [1700000001, 1700000002]
    );
    let old = call(
        &e,
        "alice",
        "GET",
        &format!("/remote.php/dav/versions/alice/versions/{id}/1700000001"),
        &[],
        "",
    )
    .await;
    assert_eq!(old.body, "owner version");
    assert_eq!(old.h("content-type"), "text/plain");
}

#[tokio::test]
async fn restoring_a_folder_indexes_only_that_folder() {
    let e = env().await;
    f(&e, "alice", "MKCOL", "/big", "").await;
    for i in 0..20 {
        f(
            &e,
            "alice",
            "PUT",
            &format!("/big/f{i}.txt"),
            &format!("content {i}"),
        )
        .await;
    }
    f(&e, "alice", "MKCOL", "/big/sub", "").await;
    f(&e, "alice", "PUT", "/big/sub/deep.txt", "deep").await;
    f(&e, "alice", "DELETE", "/big", "").await;
    // a file somebody dropped on the disk next to it, not indexed (that is what `scan` is for)
    std::fs::write(e.data.join("alice/files/stray.txt"), "stray").unwrap();
    let v = entries(&e, "alice").await;
    assert_eq!(
        mv(&e, "alice", &v[0].0, &restore("alice")).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        f(&e, "alice", "GET", "/big/f7.txt", "").await.body,
        "content 7"
    );
    assert_eq!(
        size_of(&e, "alice", "/big").await,
        (0..20)
            .map(|i| format!("content {i}").len())
            .sum::<usize>()
            .to_string()
            .parse::<usize>()
            .map(|n| n + 4)
            .unwrap()
            .to_string()
    );
    assert_eq!(size_of(&e, "alice", "/big/sub").await, "4");
    assert_eq!(
        f(&e, "alice", "GET", "/stray.txt", "").await.status,
        StatusCode::NOT_FOUND,
        "a restore is not a full scan"
    );
    let id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE uid = 'alice'")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    let rep = Store::new(e.pool.clone(), e.data.clone())
        .scan(
            &Owner {
                id,
                uid: "alice".into(),
            },
            true,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        (rep.added, rep.updated, rep.removed),
        (1, 0, 0),
        "only the stray file differs"
    );
}
