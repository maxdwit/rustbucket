//! Phase 6: user and group shares. Behaviour follows recordings of stock Nextcloud 35.0.3
//! (tests/fixtures/nc35/shares-*.txt notes, scripts/diff_shares.py).

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use http_body_util::BodyExt;
use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::routes::{AppState, router};
use serde_json::Value;
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
    app: Router,
    data: std::path::PathBuf,
    pool: sqlx::SqlitePool,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let pool = db::connect(&format!("sqlite://{}", dir.path().join("t.db").display()))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    for (u, d) in [("alice", "Alice A"), ("bob", "Bob B"), ("carol", "Carol C")] {
        db::create_user(&pool, u, d, &format!("pw-{u}"), false)
            .await
            .unwrap();
    }
    db::create_group(&pool, "team").await.unwrap();
    db::add_member(&pool, "team", "bob").await.unwrap();
    let cfg = Config {
        data_dir: data.clone(),
        ..Config::default()
    };
    Env {
        app: router(AppState::new(pool.clone(), Arc::new(cfg))),
        data,
        pool,
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
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not json: {}", self.text()))
    }
    fn data(&self) -> Value {
        self.json()["ocs"]["data"].clone()
    }
    fn code(&self) -> u64 {
        self.json()["ocs"]["meta"]["statuscode"].as_u64().unwrap()
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
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:pw-{user}"))
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

const OCS: &str = "/ocs/v2.php/apps/files_sharing/api/v1";

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

async fn dav(
    e: &Env,
    user: &str,
    method: &str,
    path: &str,
    hdrs: &[(&str, &str)],
    body: &[u8],
) -> R {
    call(
        e,
        user,
        method,
        &format!("/remote.php/dav/files/{user}{}", enc(path)),
        hdrs,
        body.to_vec(),
    )
    .await
}
async fn mv(e: &Env, user: &str, from: &str, to: &str) -> R {
    let d = format!("https://x/remote.php/dav/files/{user}{to}");
    dav(e, user, "MOVE", from, &[("destination", &d)], b"").await
}
async fn cp(e: &Env, user: &str, from: &str, to: &str) -> R {
    let d = format!("https://x/remote.php/dav/files/{user}{}", enc(to));
    dav(e, user, "COPY", from, &[("destination", &d)], b"").await
}
/// Create a share through the OCS API; returns the response.
async fn share(e: &Env, owner: &str, form: &str) -> R {
    call(
        e,
        owner,
        "POST",
        &format!("{OCS}/shares?format=json"),
        &[
            ("content-type", "application/x-www-form-urlencoded"),
            ("ocs-apirequest", "true"),
        ],
        form.as_bytes().to_vec(),
    )
    .await
}
async fn ocs(e: &Env, user: &str, method: &str, path: &str, body: &str) -> R {
    call(
        e,
        user,
        method,
        &format!(
            "{OCS}{path}{}format=json",
            if path.contains('?') { "&" } else { "?" }
        ),
        &[
            ("content-type", "application/x-www-form-urlencoded"),
            ("ocs-apirequest", "true"),
        ],
        body.as_bytes().to_vec(),
    )
    .await
}

const PF: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns"><d:prop><d:getetag/><oc:fileid/><oc:permissions/><oc:share-types/><oc:owner-id/><oc:owner-display-name/><nc:mount-type/><nc:is-mount-root/><nc:sharees/></d:prop></d:propfind>"#;

/// href -> the response block of a PROPFIND multistatus
fn blocks(xml: &str) -> Vec<(String, String)> {
    xml.split("<d:response>")
        .skip(1)
        .map(|b| {
            let href = b[b.find("<d:href>").unwrap() + 8..b.find("</d:href>").unwrap()].to_string();
            (href, b.to_string())
        })
        .collect()
}
fn prop(block: &str, name: &str) -> String {
    match block.find(&format!("<{name}>")) {
        Some(i) => block[i + name.len() + 2..i + block[i..].find("</").unwrap()].to_string(),
        None => String::new(),
    }
}
async fn pf(e: &Env, user: &str, path: &str, depth: &str) -> Vec<(String, String)> {
    let r = dav(
        e,
        user,
        "PROPFIND",
        path,
        &[("depth", depth)],
        PF.as_bytes(),
    )
    .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS, "{}", r.text());
    blocks(&r.text())
}
fn block<'a>(v: &'a [(String, String)], href_suffix: &str) -> &'a str {
    &v.iter()
        .find(|(h, _)| h.ends_with(href_suffix))
        .unwrap_or_else(|| {
            panic!(
                "no {href_suffix} in {:?}",
                v.iter().map(|x| &x.0).collect::<Vec<_>>()
            )
        })
        .1
}
async fn root_etag(e: &Env, user: &str) -> String {
    prop(&pf(e, user, "/", "0").await[0].1, "d:getetag").replace("&quot;", "\"")
}
fn disk(e: &Env, user: &str, path: &str) -> std::path::PathBuf {
    e.data
        .join(user)
        .join("files")
        .join(path.trim_start_matches('/'))
}
async fn put(e: &Env, user: &str, path: &str, body: &str) -> StatusCode {
    dav(e, user, "PUT", path, &[], body.as_bytes()).await.status
}
async fn mkcol(e: &Env, user: &str, path: &str) -> StatusCode {
    dav(e, user, "MKCOL", path, &[], b"").await.status
}

#[tokio::test]
async fn share_api_create_list_update_delete_and_stock_errors() {
    let e = env().await;
    mkcol(&e, "alice", "/proj").await;
    put(&e, "alice", "/proj/a.txt", "a-content").await;
    put(&e, "alice", "/note.txt", "n").await;

    // folder: default 31; file: default 19 (stock); shape of the answer
    let r = share(&e, "alice", "shareType=0&shareWith=bob&path=/proj").await;
    assert_eq!((r.status, r.code()), (StatusCode::OK, 200), "{}", r.text());
    let d = r.data();
    for (k, v) in [
        ("share_type", Value::from(0)),
        ("uid_owner", "alice".into()),
        ("displayname_owner", "Alice A".into()),
        ("permissions", 31.into()),
        ("item_permissions", 31.into()),
        ("path", "/proj".into()),
        ("item_type", "folder".into()),
        ("file_target", "/proj".into()),
        ("share_with", "bob".into()),
        ("share_with_displayname", "Bob B".into()),
        ("mimetype", "httpd/unix-directory".into()),
        ("is-mount-root", false.into()),
        ("mount-type", "".into()),
        ("storage_id", "home::alice".into()),
        ("token", Value::Null),
        ("attributes", Value::Null),
    ] {
        assert_eq!(d[k], v, "field {k}");
    }
    assert!(
        d["id"].is_string()
            && d["stime"].as_i64().unwrap() > 0
            && d["item_source"] == d["file_source"]
    );
    let f = share(&e, "alice", "shareType=0&shareWith=bob&path=/note.txt").await;
    assert_eq!(
        (
            f.data()["permissions"].clone(),
            f.data()["item_permissions"].clone(),
            f.data()["item_type"].clone()
        ),
        (19.into(), 27.into(), "file".into())
    );
    // permissions on a file are reduced to read/update/share
    let f2 = share(
        &e,
        "alice",
        "shareType=0&shareWith=carol&path=/note.txt&permissions=31",
    )
    .await;
    assert_eq!(f2.data()["permissions"], 19);

    // stock error table
    for (form, code, msg) in [
        (
            "shareType=0&shareWith=nobody&path=/proj",
            404,
            "Please specify a valid account to share with",
        ),
        (
            "shareType=0&shareWith=bob&path=/nope",
            404,
            "Wrong path, file/folder does not exist",
        ),
        (
            "shareType=0&shareWith=alice&path=/proj",
            403,
            "Cannot share with yourself",
        ),
        (
            "shareType=0&shareWith=carol&path=/proj&permissions=99",
            403,
            "Valid permissions are required for sharing",
        ),
        (
            "shareType=1&shareWith=nogroup&path=/proj",
            404,
            "Please specify a valid group",
        ),
        (
            "shareType=0&shareWith=bob&path=/proj",
            403,
            "Path is already shared with this user",
        ),
        (
            "shareType=0&shareWith=bob",
            404,
            "Please specify a file or folder path",
        ),
        (
            "shareType=3&path=/proj",
            403,
            "Public sharing is not allowed",
        ),
        (
            "shareType=9&shareWith=bob&path=/proj",
            400,
            "Unknown share type",
        ),
    ] {
        let r = share(&e, "alice", form).await;
        assert_eq!(
            (
                r.status.as_u16() as u64,
                r.json()["ocs"]["meta"]["message"].clone()
            ),
            (code, msg.into()),
            "{form}"
        );
    }

    // list: owner sees both, filtered by path; recipient view differs (mount point, reduced item permissions)
    assert_eq!(
        ocs(&e, "alice", "GET", "/shares", "")
            .await
            .data()
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let by_path = ocs(&e, "alice", "GET", "/shares?path=/proj", "")
        .await
        .data();
    assert_eq!(by_path.as_array().unwrap().len(), 1);
    let inc = ocs(&e, "bob", "GET", "/shares?shared_with_me=true", "")
        .await
        .data();
    let inc = inc.as_array().unwrap();
    assert_eq!(inc.len(), 2);
    let proj = inc.iter().find(|s| s["path"] == "/proj").unwrap();
    assert_eq!(
        (
            proj["is-mount-root"].clone(),
            proj["mount-type"].clone(),
            proj["item_permissions"].clone(),
            proj["storage_id"].clone(),
            proj["uid_owner"].clone()
        ),
        (
            true.into(),
            "shared".into(),
            31.into(),
            "shared::/proj".into(),
            "alice".into()
        )
    );
    let note = inc.iter().find(|s| s["path"] == "/note.txt").unwrap();
    assert_eq!(note["item_permissions"], 19 | 8); // read-update-share plus the right to remove it from the view

    // get one: owner and recipient may, strangers get stock's 404
    let id = d["id"].as_str().unwrap();
    assert_eq!(
        ocs(&e, "alice", "GET", &format!("/shares/{id}"), "")
            .await
            .data()[0]["id"],
        id
    );
    assert_eq!(
        ocs(&e, "bob", "GET", &format!("/shares/{id}"), "")
            .await
            .data()[0]["file_target"],
        "/proj"
    );
    let stranger = ocs(&e, "carol", "GET", &format!("/shares/{id}"), "").await;
    assert_eq!(
        (
            stranger.status,
            stranger.json()["ocs"]["meta"]["message"].clone()
        ),
        (
            StatusCode::NOT_FOUND,
            "Wrong share ID, share does not exist".into()
        )
    );

    // update: owner only; no parameter -> 400
    assert_eq!(
        ocs(&e, "bob", "PUT", &format!("/shares/{id}"), "permissions=1")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        ocs(&e, "alice", "PUT", &format!("/shares/{id}"), "")
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let up = ocs(
        &e,
        "alice",
        "PUT",
        &format!("/shares/{id}"),
        "permissions=1&note=hi",
    )
    .await;
    assert_eq!(
        (up.data()["permissions"].clone(), up.data()["note"].clone()),
        (1.into(), "hi".into())
    );

    // delete: owner removes it for everybody
    assert_eq!(
        ocs(&e, "alice", "DELETE", &format!("/shares/{id}"), "")
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        ocs(&e, "alice", "GET", &format!("/shares/{id}"), "")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert!(
        pf(&e, "bob", "/", "1")
            .await
            .iter()
            .all(|(h, _)| !h.contains("proj"))
    );

    // sharees: users by id or name, groups, exact match moves to "exact"
    let s = ocs(
        &e,
        "alice",
        "GET",
        "/sharees?search=b&itemType=file&lookup=false&perPage=200",
        "",
    )
    .await
    .data();
    assert_eq!(s["users"][0]["value"]["shareWith"], "bob");
    assert_eq!(s["users"][0]["label"], "Bob B");
    assert_eq!(s["lookupEnabled"], false);
    let s = ocs(&e, "alice", "GET", "/sharees?search=team", "")
        .await
        .data();
    assert_eq!(
        s["exact"]["groups"][0]["value"],
        serde_json::json!({"shareType": 1, "shareWith": "team"})
    );
    let s = ocs(&e, "alice", "GET", "/sharees?search=alice", "")
        .await
        .data();
    assert!(
        s["users"].as_array().unwrap().is_empty()
            && s["exact"]["users"].as_array().unwrap().is_empty()
    ); // never yourself
}

#[tokio::test]
async fn recipient_sees_mount_with_stock_props_and_the_owner_sees_share_types() {
    let e = env().await;
    mkcol(&e, "alice", "/proj").await;
    put(&e, "alice", "/proj/a.txt", "a-content").await;
    mkcol(&e, "alice", "/proj/sub").await;
    share(
        &e,
        "alice",
        "shareType=0&shareWith=bob&path=/proj&permissions=15",
    )
    .await;
    put(&e, "bob", "/own.txt", "x").await;

    let root = pf(&e, "bob", "/", "1").await;
    let m = block(&root, "/files/bob/proj/");
    let alice_id = prop(&pf(&e, "alice", "/proj", "0").await[0].1, "oc:fileid");
    assert_eq!(prop(m, "oc:fileid"), alice_id); // the owner's fileid and etag are shown
    assert_eq!(
        prop(m, "d:getetag"),
        prop(&pf(&e, "alice", "/proj", "0").await[0].1, "d:getetag")
    );
    assert_eq!(
        (
            prop(m, "oc:permissions").as_str(),
            prop(m, "oc:owner-id").as_str(),
            prop(m, "oc:owner-display-name").as_str(),
            prop(m, "nc:mount-type").as_str(),
            prop(m, "nc:is-mount-root").as_str()
        ),
        ("SGDNVCK", "alice", "Alice A", "shared", "true")
    );
    assert!(block(&root, "/files/bob/own.txt").contains("<oc:owner-id>bob</oc:owner-id>"));
    assert_eq!(
        prop(block(&root, "/files/bob/own.txt"), "oc:permissions"),
        "RGDNVW"
    );
    assert_eq!(
        prop(block(&root, "/files/bob/own.txt"), "nc:is-mount-root"),
        "false"
    );

    // inside the share: hrefs are in bob's tree, letters per item, owner stays alice
    let inside = pf(&e, "bob", "/proj", "infinity").await;
    let hrefs: Vec<&str> = inside.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(
        hrefs,
        [
            "/remote.php/dav/files/bob/proj/",
            "/remote.php/dav/files/bob/proj/a.txt",
            "/remote.php/dav/files/bob/proj/sub/"
        ]
    );
    assert_eq!(
        prop(block(&inside, "/proj/a.txt"), "oc:permissions"),
        "SGDNVW"
    );
    assert_eq!(
        prop(block(&inside, "/proj/sub/"), "oc:permissions"),
        "SGDNVCK"
    );
    assert_eq!(prop(block(&inside, "/proj/a.txt"), "oc:owner-id"), "alice");
    assert_eq!(
        prop(block(&inside, "/proj/a.txt"), "nc:is-mount-root"),
        "false"
    );

    // the owner sees share-types and sharees on the shared folder, nothing special elsewhere
    let mine = pf(&e, "alice", "/", "1").await;
    let p = block(&mine, "/files/alice/proj/");
    assert!(p.contains("<oc:share-type>0</oc:share-type>"), "{p}");
    assert!(p.contains("<nc:sharee><nc:id>bob</nc:id><nc:display-name>Bob B</nc:display-name><nc:type>0</nc:type></nc:sharee>"));
    assert_eq!(prop(p, "oc:permissions"), "RGDNVCK");
    // GET / HEAD work through the mount and report the owner's etag
    let g = dav(&e, "bob", "GET", "/proj/a.txt", &[], b"").await;
    assert_eq!((g.status, g.body), (StatusCode::OK, b"a-content".to_vec()));
    assert_eq!(
        dav(&e, "bob", "GET", "/proj/nope.txt", &[], b"")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        dav(&e, "carol", "GET", "/proj/a.txt", &[], b"")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn permission_matrix() {
    let e = env().await;
    for p in [1u32, 3, 5, 9, 13, 15, 31] {
        let m = format!("/m{p}");
        mkcol(&e, "alice", &m).await;
        mkcol(&e, "alice", &format!("{m}/s")).await;
        for f in ["f", "del", "ren", "mov", "cpy"] {
            put(&e, "alice", &format!("{m}/{f}.txt"), "orig").await;
        }
        let r = share(
            &e,
            "alice",
            &format!("shareType=0&shareWith=bob&path={m}&permissions={p}"),
        )
        .await;
        assert_eq!(r.data()["permissions"], p, "{}", r.text());
        let (upd, cre, del) = (p & 2 != 0, p & 4 != 0, p & 8 != 0);
        let ok = |allowed: bool, yes: StatusCode| if allowed { yes } else { StatusCode::FORBIDDEN };
        let t = format!("perms={p}");
        assert_eq!(
            dav(&e, "bob", "GET", &format!("{m}/f.txt"), &[], b"")
                .await
                .status,
            StatusCode::OK,
            "{t} read"
        );
        assert_eq!(
            put(&e, "bob", &format!("{m}/new.txt"), "n").await,
            ok(cre, StatusCode::CREATED),
            "{t} PUT new"
        );
        assert_eq!(
            put(&e, "bob", &format!("{m}/f.txt"), "changed").await,
            ok(upd, StatusCode::NO_CONTENT),
            "{t} PUT overwrite"
        );
        assert_eq!(
            mkcol(&e, "bob", &format!("{m}/d")).await,
            ok(cre, StatusCode::CREATED),
            "{t} MKCOL"
        );
        assert_eq!(
            dav(&e, "bob", "DELETE", &format!("{m}/del.txt"), &[], b"")
                .await
                .status,
            ok(del, StatusCode::NO_CONTENT),
            "{t} DELETE"
        );
        assert_eq!(
            mv(&e, "bob", &format!("{m}/ren.txt"), &format!("{m}/ren2.txt"))
                .await
                .status,
            ok(upd || (cre && del), StatusCode::CREATED),
            "{t} rename"
        );
        assert_eq!(
            mv(
                &e,
                "bob",
                &format!("{m}/mov.txt"),
                &format!("{m}/s/mov.txt")
            )
            .await
            .status,
            ok(upd, StatusCode::CREATED),
            "{t} move"
        );
        assert_eq!(
            cp(&e, "bob", &format!("{m}/cpy.txt"), &format!("{m}/cpy2.txt"))
                .await
                .status,
            ok(cre, StatusCode::CREATED),
            "{t} copy within"
        );
        // the owner's disk shows exactly what was allowed - and nothing was half-done
        let on = |f: &str| disk(&e, "alice", &format!("{m}/{f}")).exists();
        assert_eq!(
            (
                on("new.txt"),
                on("d"),
                !on("del.txt"),
                on("ren2.txt"),
                on("s/mov.txt"),
                on("cpy2.txt")
            ),
            (cre, cre, del, upd || (cre && del), upd, cre),
            "{t} disk"
        );
        assert_eq!(
            std::fs::read_to_string(disk(&e, "alice", &format!("{m}/f.txt"))).unwrap(),
            if upd { "changed" } else { "orig" },
            "{t} content"
        );
        // reading out of a read-only share into one's own tree is always fine
        assert_eq!(
            cp(
                &e,
                "bob",
                &format!("{m}/f.txt"),
                &format!("/copy-of-{p}.txt")
            )
            .await
            .status,
            StatusCode::CREATED,
            "{t} copy out"
        );
    }
}

#[tokio::test]
async fn deleting_the_mount_is_unshare_and_never_touches_the_owners_data() {
    let e = env().await;
    mkcol(&e, "alice", "/proj").await;
    put(&e, "alice", "/proj/a.txt", "precious").await;
    mkcol(&e, "alice", "/proj/sub").await;
    put(&e, "alice", "/proj/sub/b.txt", "deep").await;
    let r = share(
        &e,
        "alice",
        "shareType=0&shareWith=bob&path=/proj&permissions=31",
    )
    .await;
    let id = r.data()["id"].as_str().unwrap().to_string();
    let before = (
        std::fs::read(disk(&e, "alice", "proj/a.txt")).unwrap(),
        std::fs::read(disk(&e, "alice", "proj/sub/b.txt")).unwrap(),
    );
    assert_eq!(
        dav(&e, "bob", "DELETE", "/proj", &[], b"").await.status,
        StatusCode::NO_CONTENT
    );
    let after = (
        std::fs::read(disk(&e, "alice", "proj/a.txt")).unwrap(),
        std::fs::read(disk(&e, "alice", "proj/sub/b.txt")).unwrap(),
    );
    assert_eq!(before, after);
    // the share is gone for bob (and, stock: for the owner's list too); alice's folder is intact
    assert_eq!(
        ocs(&e, "alice", "GET", &format!("/shares/{id}"), "")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert!(
        ocs(&e, "bob", "GET", "/shares?shared_with_me=true", "")
            .await
            .data()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        dav(&e, "bob", "GET", "/proj/a.txt", &[], b"").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        dav(&e, "alice", "GET", "/proj/a.txt", &[], b"")
            .await
            .status,
        StatusCode::OK
    );
    // deleting things *inside* a share needs the delete bit and does remove the owner's file
    share(
        &e,
        "alice",
        "shareType=0&shareWith=bob&path=/proj&permissions=31",
    )
    .await;
    assert_eq!(
        dav(&e, "bob", "DELETE", "/proj/sub", &[], b"").await.status,
        StatusCode::NO_CONTENT
    );
    assert!(!disk(&e, "alice", "proj/sub").exists());
}

#[tokio::test]
async fn mount_points_can_be_renamed_moved_and_never_collide() {
    let e = env().await;
    mkcol(&e, "alice", "/proj").await;
    put(&e, "alice", "/proj/a.txt", "a").await;
    put(&e, "alice", "/note.txt", "n").await;
    // bob already has "proj" and "note.txt": the shares get stock's numbered names
    mkcol(&e, "bob", "/proj").await;
    put(&e, "bob", "/note.txt", "bobs").await;
    let s1 = share(
        &e,
        "alice",
        "shareType=0&shareWith=bob&path=/proj&permissions=31",
    )
    .await;
    assert_eq!(s1.data()["file_target"], "/proj (2)");
    share(&e, "alice", "shareType=0&shareWith=bob&path=/note.txt").await;
    let root = pf(&e, "bob", "/", "1").await;
    let hrefs: Vec<&str> = root.iter().map(|(h, _)| h.as_str()).collect();
    for want in [
        "/files/bob/proj/",
        "/files/bob/proj%20(2)/",
        "/files/bob/note.txt",
        "/files/bob/note%20(2).txt",
    ] {
        assert!(
            hrefs.iter().any(|h| h.ends_with(want)),
            "{want} in {hrefs:?}"
        );
    }
    assert_eq!(
        dav(&e, "bob", "GET", "/note (2).txt", &[], b"").await.body,
        b"n"
    );
    assert_eq!(
        dav(&e, "bob", "GET", "/note.txt", &[], b"").await.body,
        b"bobs"
    );
    // ... and nobody can create something over a mount point
    assert_eq!(
        mkcol(&e, "bob", "/proj (2)").await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        put(&e, "bob", "/proj (2)", "x").await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        cp(&e, "bob", "/note.txt", "/proj (2)").await.status,
        StatusCode::FORBIDDEN
    );

    // rename the mount; the content follows, the owner's folder keeps its name
    assert_eq!(
        mv(&e, "bob", "/proj (2)", "/Shared proj").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        dav(&e, "bob", "GET", "/Shared proj/a.txt", &[], b"")
            .await
            .body,
        b"a"
    );
    assert_eq!(
        dav(&e, "bob", "GET", "/proj (2)/a.txt", &[], b"")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert!(disk(&e, "alice", "proj/a.txt").exists());
    assert_eq!(
        ocs(&e, "bob", "GET", "/shares?shared_with_me=true", "")
            .await
            .data()
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["path"] == "/proj")
            .unwrap()["file_target"],
        "/Shared proj"
    );
    // move it into a folder of bob's own (stock allows mounts anywhere in the tree)
    mkcol(&e, "bob", "/Docs").await;
    assert_eq!(
        mv(&e, "bob", "/Shared proj", "/Docs/proj-sub").await.status,
        StatusCode::CREATED
    );
    let docs = pf(&e, "bob", "/Docs", "1").await;
    assert_eq!(
        prop(block(&docs, "/Docs/proj-sub/"), "nc:is-mount-root"),
        "true"
    );
    assert_eq!(
        dav(&e, "bob", "GET", "/Docs/proj-sub/a.txt", &[], b"")
            .await
            .body,
        b"a"
    );
    assert!(
        pf(&e, "bob", "/", "infinity")
            .await
            .iter()
            .any(|(h, _)| h.ends_with("/Docs/proj-sub/a.txt"))
    );
    // a mount cannot be moved into another share, onto an existing name, or into a missing folder
    assert_eq!(
        mv(&e, "bob", "/Docs/proj-sub", "/Docs/proj-sub2/x")
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        mv(&e, "bob", "/Docs/proj-sub", "/note.txt").await.status,
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(
        mv(&e, "bob", "/Docs/proj-sub", "/Docs/proj-sub/inner")
            .await
            .status,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn group_shares_follow_membership_and_can_be_hidden_per_user() {
    let e = env().await;
    mkcol(&e, "alice", "/grp").await;
    put(&e, "alice", "/grp/in.txt", "inside").await;
    let r = share(
        &e,
        "alice",
        "shareType=1&shareWith=team&path=/grp&permissions=15",
    )
    .await;
    assert_eq!(
        (
            r.data()["share_type"].clone(),
            r.data()["share_with"].clone()
        ),
        (1.into(), "team".into())
    );
    let has = |v: &[(String, String)]| v.iter().any(|(h, _)| h.ends_with("/grp/"));
    assert!(has(&pf(&e, "bob", "/", "1").await));
    assert!(!has(&pf(&e, "carol", "/", "1").await));
    assert_eq!(
        dav(&e, "carol", "GET", "/grp/in.txt", &[], b"")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // a member added later sees it at once; a removed member loses it (no hooks, evaluated per request)
    db::add_member(&e.pool, "team", "carol").await.unwrap();
    assert!(has(&pf(&e, "carol", "/", "1").await));
    assert_eq!(
        put(&e, "carol", "/grp/c.txt", "from carol").await,
        StatusCode::CREATED
    );
    sqlx::query(
        "DELETE FROM group_members WHERE user_id = (SELECT id FROM users WHERE uid = 'bob')",
    )
    .execute(&e.pool)
    .await
    .unwrap();
    assert!(!has(&pf(&e, "bob", "/", "1").await));
    assert_eq!(
        put(&e, "bob", "/grp/b.txt", "x").await,
        StatusCode::NOT_FOUND
    );
    db::add_member(&e.pool, "team", "bob").await.unwrap();
    assert!(has(&pf(&e, "bob", "/", "1").await));
    // the recipient's list shows it as a group share
    let inc = ocs(&e, "bob", "GET", "/shares?shared_with_me=true", "")
        .await
        .data();
    assert_eq!(inc[0]["share_type"], 1);
    // bob removes it for himself only: carol keeps it, the share row stays, alice's files stay
    assert_eq!(
        dav(&e, "bob", "DELETE", "/grp", &[], b"").await.status,
        StatusCode::NO_CONTENT
    );
    assert!(!has(&pf(&e, "bob", "/", "1").await));
    assert!(has(&pf(&e, "carol", "/", "1").await));
    assert_eq!(
        ocs(&e, "alice", "GET", "/shares?path=/grp", "")
            .await
            .data()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(disk(&e, "alice", "grp/c.txt")).unwrap(),
        "from carol"
    );
    assert!(
        ocs(&e, "bob", "GET", "/shares?shared_with_me=true", "")
            .await
            .data()
            .as_array()
            .unwrap()
            .is_empty()
    );
    // direct share + group share of the same folder: one mount with the union of the permissions
    share(
        &e,
        "alice",
        "shareType=0&shareWith=carol&path=/grp&permissions=1",
    )
    .await;
    assert_eq!(
        prop(
            block(&pf(&e, "carol", "/", "1").await, "/grp/"),
            "oc:permissions"
        ),
        "SGDNVCK"
    );
    assert_eq!(
        pf(&e, "carol", "/", "1")
            .await
            .iter()
            .filter(|(h, _)| h.contains("grp"))
            .count(),
        1
    );
}

#[tokio::test]
async fn the_share_follows_owner_renames_and_dies_with_the_item() {
    let e = env().await;
    mkcol(&e, "alice", "/proj").await;
    put(&e, "alice", "/proj/a.txt", "a").await;
    share(
        &e,
        "alice",
        "shareType=0&shareWith=bob&path=/proj&permissions=15",
    )
    .await;
    assert_eq!(
        mv(&e, "alice", "/proj", "/renamed by owner").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        dav(&e, "bob", "GET", "/proj/a.txt", &[], b"").await.body,
        b"a"
    ); // bob's mount is unchanged
    assert_eq!(
        put(&e, "bob", "/proj/b.txt", "b").await,
        StatusCode::CREATED
    );
    assert!(disk(&e, "alice", "renamed by owner/b.txt").exists());
    mkcol(&e, "alice", "/archive").await;
    assert_eq!(
        mv(&e, "alice", "/renamed by owner", "/archive/moved")
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        dav(&e, "bob", "GET", "/proj/b.txt", &[], b"").await.body,
        b"b"
    );
    assert_eq!(
        ocs(&e, "alice", "GET", "/shares", "").await.data()[0]["path"],
        "/archive/moved"
    );
    // the owner deletes the folder: the share is gone
    assert_eq!(
        dav(&e, "alice", "DELETE", "/archive/moved", &[], b"")
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        dav(&e, "bob", "GET", "/proj/b.txt", &[], b"").await.status,
        StatusCode::NOT_FOUND
    );
    assert!(
        ocs(&e, "bob", "GET", "/shares?shared_with_me=true", "")
            .await
            .data()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        ocs(&e, "alice", "GET", "/shares", "")
            .await
            .data()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn moves_and_copies_between_own_tree_and_shares() {
    let e = env().await;
    mkcol(&e, "alice", "/rw").await;
    put(&e, "alice", "/rw/in-share.txt", "from alice").await;
    mkcol(&e, "alice", "/ro").await;
    put(&e, "alice", "/ro/in-ro.txt", "ro content").await;
    mkcol(&e, "alice", "/ro/dir").await;
    put(&e, "alice", "/ro/dir/deep.txt", "deep").await;
    share(
        &e,
        "alice",
        "shareType=0&shareWith=bob&path=/rw&permissions=15",
    )
    .await;
    share(
        &e,
        "alice",
        "shareType=0&shareWith=bob&path=/ro&permissions=1",
    )
    .await;
    put(&e, "bob", "/mine.txt", "bobs file").await;
    mkcol(&e, "bob", "/mydir").await;
    put(&e, "bob", "/mydir/x.txt", "x").await;

    // own -> share (MOVE): needs create there; the file is really moved into alice's tree
    assert_eq!(
        mv(&e, "bob", "/mine.txt", "/rw/mine.txt").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        std::fs::read_to_string(disk(&e, "alice", "rw/mine.txt")).unwrap(),
        "bobs file"
    );
    assert!(!disk(&e, "bob", "mine.txt").exists());
    // own folder -> share (COPY), with content
    assert_eq!(
        cp(&e, "bob", "/mydir", "/rw/mydir").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        std::fs::read_to_string(disk(&e, "alice", "rw/mydir/x.txt")).unwrap(),
        "x"
    );
    assert!(disk(&e, "bob", "mydir/x.txt").exists());
    // share -> own (MOVE): needs delete in the share
    assert_eq!(
        mv(&e, "bob", "/rw/in-share.txt", "/taken.txt").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        std::fs::read_to_string(disk(&e, "bob", "taken.txt")).unwrap(),
        "from alice"
    );
    assert!(!disk(&e, "alice", "rw/in-share.txt").exists());
    // read-only share: nothing in, nothing moved out, but copying out is fine
    assert_eq!(
        mv(&e, "bob", "/taken.txt", "/ro/taken.txt").await.status,
        StatusCode::FORBIDDEN
    );
    assert!(disk(&e, "bob", "taken.txt").exists() && !disk(&e, "alice", "ro/taken.txt").exists());
    assert_eq!(
        mv(&e, "bob", "/ro/in-ro.txt", "/stolen.txt").await.status,
        StatusCode::FORBIDDEN
    );
    assert!(disk(&e, "alice", "ro/in-ro.txt").exists() && !disk(&e, "bob", "stolen.txt").exists());
    assert_eq!(
        cp(&e, "bob", "/ro/dir", "/dir-copy").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        std::fs::read_to_string(disk(&e, "bob", "dir-copy/deep.txt")).unwrap(),
        "deep"
    );
    assert!(disk(&e, "alice", "ro/dir/deep.txt").exists());
    // between two shares of the same owner
    assert_eq!(
        cp(&e, "bob", "/ro/in-ro.txt", "/rw/from-ro.txt")
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        std::fs::read_to_string(disk(&e, "alice", "rw/from-ro.txt")).unwrap(),
        "ro content"
    );
    // the index agrees with the disk for both users after all that
    std::fs::write(e.data.join(".rustbucket-data"), "").unwrap();
    for u in ["alice", "bob"] {
        let store = rustbucket::storage::Store::new(e.pool.clone(), e.data.clone());
        let id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE uid = $1")
            .bind(u)
            .fetch_one(&e.pool)
            .await
            .unwrap();
        let rep = store
            .scan(
                &rustbucket::storage::Owner { id, uid: u.into() },
                true,
                false,
            )
            .await
            .unwrap();
        assert_eq!((rep.added, rep.updated, rep.removed), (0, 0, 0), "{u}");
    }
}

#[tokio::test]
async fn the_recipients_root_etag_changes_with_everything_that_matters_to_the_sync_client() {
    let e = env().await;
    mkcol(&e, "alice", "/proj").await;
    put(&e, "alice", "/proj/a.txt", "a").await;
    let e0 = root_etag(&e, "bob").await;
    let r = share(
        &e,
        "alice",
        "shareType=0&shareWith=bob&path=/proj&permissions=31",
    )
    .await;
    let e1 = root_etag(&e, "bob").await;
    assert_ne!(e0, e1, "a new share must change the root etag");
    assert_eq!(
        root_etag(&e, "bob").await,
        e1,
        "stable while nothing changes"
    );
    put(&e, "alice", "/proj/a.txt", "changed by owner").await;
    let e2 = root_etag(&e, "bob").await;
    assert_ne!(e1, e2, "owner edit inside the share");
    put(&e, "bob", "/proj/b.txt", "by bob").await;
    let e3 = root_etag(&e, "bob").await;
    assert_ne!(e2, e3, "recipient edit inside the share");
    // the mount's own etag is the owner's, so the client's descent finds the change
    assert_eq!(
        prop(&pf(&e, "bob", "/proj", "0").await[0].1, "d:getetag"),
        prop(&pf(&e, "alice", "/proj", "0").await[0].1, "d:getetag")
    );
    // a mount deeper in the tree changes every ancestor, an unrelated folder keeps its etag
    mkcol(&e, "bob", "/Docs").await;
    mkcol(&e, "bob", "/Other").await;
    mv(&e, "bob", "/proj", "/Docs/proj").await;
    let docs = prop(&pf(&e, "bob", "/Docs", "0").await[0].1, "d:getetag");
    let other = prop(&pf(&e, "bob", "/Other", "0").await[0].1, "d:getetag");
    put(&e, "alice", "/proj/c.txt", "c").await;
    assert_ne!(
        prop(&pf(&e, "bob", "/Docs", "0").await[0].1, "d:getetag"),
        docs
    );
    assert_ne!(root_etag(&e, "bob").await, e3);
    assert_eq!(
        prop(&pf(&e, "bob", "/Other", "0").await[0].1, "d:getetag"),
        other
    );
    // unsharing changes it again
    let before = root_etag(&e, "bob").await;
    ocs(
        &e,
        "alice",
        "DELETE",
        &format!("/shares/{}", r.data()["id"].as_str().unwrap()),
        "",
    )
    .await;
    assert_ne!(root_etag(&e, "bob").await, before);
    // HEAD agrees with PROPFIND for the root
    let h = dav(&e, "bob", "HEAD", "/", &[], b"").await;
    assert_eq!(h.h("etag"), root_etag(&e, "bob").await);
}

fn png() -> Vec<u8> {
    let img = image::RgbaImage::from_fn(300, 200, |x, y| {
        image::Rgba([(x % 255) as u8, (y % 255) as u8, 90, 255])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

#[tokio::test]
async fn search_and_previews_include_shared_files_but_only_for_recipients() {
    let e = env().await;
    mkcol(&e, "alice", "/Photos").await;
    dav(
        &e,
        "alice",
        "PUT",
        "/Photos/pic.png",
        &[("x-oc-mtime", "1700000000")],
        &png(),
    )
    .await;
    share(
        &e,
        "alice",
        "shareType=0&shareWith=bob&path=/Photos&permissions=1",
    )
    .await;
    dav(
        &e,
        "bob",
        "PUT",
        "/own.png",
        &[("x-oc-mtime", "1600000000")],
        &png(),
    )
    .await;
    let body = r#"<d:searchrequest xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:basicsearch><d:select><d:prop><oc:fileid/><d:getcontenttype/><oc:owner-id/></d:prop></d:select><d:from><d:scope><d:href>/files/bob</d:href><d:depth>infinity</d:depth></d:scope></d:from><d:where><d:like><d:prop><d:getcontenttype/></d:prop><d:literal>image/%</d:literal></d:like></d:where><d:orderby><d:order><d:prop><d:getlastmodified/></d:prop><d:descending/></d:order></d:orderby></d:basicsearch></d:searchrequest>"#;
    let s = call(
        &e,
        "bob",
        "SEARCH",
        "/remote.php/dav/",
        &[],
        body.as_bytes().to_vec(),
    )
    .await;
    let hrefs: Vec<(String, String)> = blocks(&s.text());
    assert_eq!(
        hrefs.iter().map(|(h, _)| h.as_str()).collect::<Vec<_>>(),
        [
            "/remote.php/dav/files/bob/Photos/pic.png",
            "/remote.php/dav/files/bob/own.png"
        ]
    );
    assert_eq!(prop(&hrefs[0].1, "oc:owner-id"), "alice");
    // previews: by path through the mount and by the owner's fileid; strangers get nothing
    let id = prop(
        &pf(&e, "alice", "/Photos/pic.png", "0").await[0].1,
        "oc:fileid",
    );
    let by_path = call(
        &e,
        "bob",
        "GET",
        "/index.php/core/preview.png?file=/Photos/pic.png&x=64&y=64&a=1",
        &[],
        vec![],
    )
    .await;
    let by_id = call(
        &e,
        "bob",
        "GET",
        &format!("/index.php/core/preview?fileId={id}&x=64&y=64&a=1"),
        &[],
        vec![],
    )
    .await;
    assert_eq!(
        (by_path.status, by_id.status),
        (StatusCode::OK, StatusCode::OK)
    );
    assert_eq!(by_path.body, by_id.body);
    assert_eq!(
        call(
            &e,
            "carol",
            "GET",
            &format!("/index.php/core/preview?fileId={id}&x=64&y=64"),
            &[],
            vec![]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    // the cache belongs to the owner's file, shared by every recipient
    let cached = walk(&e.data.join(".appdata/preview"));
    assert_eq!(cached.len(), 1, "{cached:?}");
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

#[tokio::test]
async fn chunked_upload_and_lock_respect_share_permissions() {
    let e = env().await;
    mkcol(&e, "alice", "/ro").await;
    mkcol(&e, "alice", "/rw").await;
    share(
        &e,
        "alice",
        "shareType=0&shareWith=bob&path=/ro&permissions=1",
    )
    .await;
    share(
        &e,
        "alice",
        "shareType=0&shareWith=bob&path=/rw&permissions=15",
    )
    .await;
    let up = |p: &str| format!("/remote.php/dav/uploads/bob{p}");
    for (target, want) in [
        ("/ro/big.bin", StatusCode::FORBIDDEN),
        ("/rw/big.bin", StatusCode::CREATED),
    ] {
        assert_eq!(
            call(
                &e,
                "bob",
                "MKCOL",
                &up(&format!("/u{}", want.as_u16())),
                &[],
                vec![]
            )
            .await
            .status,
            StatusCode::CREATED
        );
        call(
            &e,
            "bob",
            "PUT",
            &up(&format!("/u{}/0", want.as_u16())),
            &[],
            b"chunk".to_vec(),
        )
        .await;
        let d = format!("https://x/remote.php/dav/files/bob{target}");
        let r = call(
            &e,
            "bob",
            "MOVE",
            &up(&format!("/u{}/.file", want.as_u16())),
            &[("destination", &d), ("oc-total-length", "5")],
            vec![],
        )
        .await;
        assert_eq!(r.status, want, "{target}");
        if want == StatusCode::CREATED {
            assert_eq!(r.h("x-nc-ownerid"), "alice");
            assert_eq!(r.h("x-nc-permissions"), "SGDNVW");
            assert_eq!(
                std::fs::read(disk(&e, "alice", "rw/big.bin")).unwrap(),
                b"chunk"
            );
        } else {
            assert!(!disk(&e, "alice", "ro/big.bin").exists());
            assert!(
                std::fs::read_dir(e.data.join("bob/tmp")).map_or(true, |mut d| d.next().is_none())
            );
        }
    }
    assert_eq!(
        dav(&e, "bob", "LOCK", "/ro/lock.txt", &[], b"")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        dav(&e, "bob", "LOCK", "/rw/lock.txt", &[], b"")
            .await
            .status,
        StatusCode::CREATED
    );
    assert!(
        disk(&e, "alice", "rw/lock.txt").exists() && !disk(&e, "alice", "ro/lock.txt").exists()
    );
}
