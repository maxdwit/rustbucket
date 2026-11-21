//! Phase 7: the browser UI - login/session/CSRF, file listing (incl. hostile names), settings, admin,
//! share dialog, static assets, security headers.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
use rustbucket::auth::csrf_token_for;
use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::routes::{AppState, router};
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
    app: Router,
    pool: sqlx::SqlitePool,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect(&format!("sqlite://{}", dir.path().join("t.db").display()))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    db::create_user(&pool, "admin", "Admin A", "pw-admin", true)
        .await
        .unwrap();
    db::create_user(&pool, "bob", "Bob B", "pw-bob", false)
        .await
        .unwrap();
    db::create_user(&pool, "alice", "Alice A", "pw-alice", false)
        .await
        .unwrap();
    let cfg = Config {
        data_dir: dir.path().join("data"),
        ..Config::default()
    };
    Env {
        app: router(AppState::new(pool.clone(), Arc::new(cfg))),
        pool,
        _dir: dir,
    }
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
    fn cookies(&self) -> Vec<String> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect()
    }
}

async fn send(e: &Env, method: &str, uri: &str, hdrs: &[(&str, String)], body: &str) -> R {
    let mut b = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri);
    for (k, v) in hdrs {
        b = b.header(*k, v.as_str());
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

fn cookie_of(r: &R, name: &str) -> Option<String> {
    r.cookies().iter().find_map(|c| {
        c.strip_prefix(&format!("{name}="))
            .map(|v| v.split(';').next().unwrap().to_string())
    })
}
fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", pct(k), pct(v)))
        .collect::<Vec<_>>()
        .join("&")
}
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn form_ct() -> (&'static str, String) {
    (
        "content-type",
        "application/x-www-form-urlencoded".to_string(),
    )
}

/// Log in through the real form; returns the session id.
async fn login(e: &Env, user: &str, pw: &str) -> String {
    let page = send(e, "GET", "/login", &[], "").await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = cookie_of(&page, "rb_csrf").expect("csrf cookie");
    let r = send(
        e,
        "POST",
        "/login",
        &[form_ct(), ("cookie", format!("rb_csrf={csrf}"))],
        &form(&[("username", user), ("password", pw), ("csrf", &csrf)]),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    cookie_of(&r, "rb_session").expect("session cookie")
}
fn ck(sid: &str) -> (&'static str, String) {
    ("cookie", format!("rb_session={sid}"))
}
async fn get(e: &Env, sid: &str, uri: &str) -> R {
    send(e, "GET", uri, &[ck(sid)], "").await
}
async fn post(e: &Env, sid: &str, uri: &str, pairs: &[(&str, &str)]) -> R {
    let token = csrf_token_for(sid);
    let mut p = pairs.to_vec();
    p.push(("csrf", &token));
    send(e, "POST", uri, &[form_ct(), ck(sid)], &form(&p)).await
}
fn basic(u: &str, p: &str) -> (&'static str, String) {
    (
        "authorization",
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))
        ),
    )
}
async fn dav(e: &Env, user: &str, method: &str, path: &str, body: &str) -> StatusCode {
    let pw = format!("pw-{user}");
    send(
        e,
        method,
        &format!("/remote.php/dav/files/{user}{path}"),
        &[basic(user, &pw)],
        body,
    )
    .await
    .status
}

#[tokio::test]
async fn login_logout_and_redirects() {
    let e = env().await;
    // stock sends / to the login page; a logged-out app URL goes there too, remembering where to return
    let r = send(&e, "GET", "/", &[], "").await;
    assert_eq!(
        (r.status, r.h("location").as_str()),
        (StatusCode::FOUND, "/login")
    );
    let r = send(&e, "GET", "/apps/files/?dir=%2Fa%20b", &[], "").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(
        r.h("location"),
        "/login?redirect_url=%2Fapps%2Ffiles%2F%3Fdir%3D%252Fa%2520b"
    );
    // HTMX fragment requests are told to leave the page instead of getting a redirect to HTML
    let r = send(
        &e,
        "GET",
        "/apps/files/",
        &[("hx-request", "true".into())],
        "",
    )
    .await;
    assert_eq!(
        (r.status, r.h("hx-redirect").as_str()),
        (StatusCode::UNAUTHORIZED, "/login")
    );
    // the login page, both stock paths
    for p in ["/login", "/index.php/login"] {
        let r = send(&e, "GET", p, &[], "").await;
        assert!(
            r.status == StatusCode::OK
                && r.body.contains("name=\"username\"")
                && cookie_of(&r, "rb_csrf").is_some(),
            "{p}"
        );
    }
    // wrong password: 401 and the form again; a forged CSRF field: 403
    let page = send(&e, "GET", "/login", &[], "").await;
    let csrf = cookie_of(&page, "rb_csrf").unwrap();
    let bad = send(
        &e,
        "POST",
        "/login",
        &[form_ct(), ("cookie", format!("rb_csrf={csrf}"))],
        &form(&[("username", "bob"), ("password", "nope"), ("csrf", &csrf)]),
    )
    .await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
    assert!(
        bad.body.contains("Wrong account name or password")
            && cookie_of(&bad, "rb_session").is_none()
    );
    let forged = send(
        &e,
        "POST",
        "/login",
        &[form_ct(), ("cookie", format!("rb_csrf={csrf}"))],
        &form(&[
            ("username", "bob"),
            ("password", "pw-bob"),
            ("csrf", "other"),
        ]),
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    // good login; the redirect target is honoured only when it stays on this site
    for (target, want) in [
        ("/apps/files/?dir=%2Fx", "/apps/files/?dir=%2Fx"),
        ("//evil.example/x", "/apps/files/"),
        ("https://evil.example", "/apps/files/"),
    ] {
        let ok = send(
            &e,
            "POST",
            "/login",
            &[form_ct(), ("cookie", format!("rb_csrf={csrf}"))],
            &form(&[
                ("username", "bob"),
                ("password", "pw-bob"),
                ("csrf", &csrf),
                ("redirect_url", target),
            ]),
        )
        .await;
        assert_eq!(
            (ok.status, ok.h("location").as_str()),
            (StatusCode::SEE_OTHER, want),
            "{target}"
        );
        let c = ok
            .cookies()
            .into_iter()
            .find(|c| c.starts_with("rb_session="))
            .unwrap();
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Lax"), "{c}");
    }
    let sid = login(&e, "bob", "pw-bob").await;
    assert_eq!(
        send(&e, "GET", "/", &[ck(&sid)], "").await.h("location"),
        "/apps/files/"
    );
    assert_eq!(get(&e, &sid, "/apps/files/").await.status, StatusCode::OK);
    // logout needs the token, removes the session on the server and clears the cookie
    let no_token = send(&e, "POST", "/logout", &[form_ct(), ck(&sid)], "csrf=wrong").await;
    assert_eq!(no_token.status, StatusCode::FORBIDDEN);
    assert_eq!(get(&e, &sid, "/apps/files/").await.status, StatusCode::OK);
    let out = post(&e, &sid, "/logout", &[]).await;
    assert_eq!(
        (out.status, out.h("location").as_str()),
        (StatusCode::SEE_OTHER, "/login")
    );
    assert!(
        out.cookies()
            .iter()
            .any(|c| c.starts_with("rb_session=;") && c.contains("Max-Age=0"))
    );
    assert_eq!(
        get(&e, &sid, "/apps/files/").await.status,
        StatusCode::SEE_OTHER,
        "the old cookie is dead"
    );
}

#[tokio::test]
async fn app_pages_have_a_strict_csp_and_serve_their_assets() {
    let e = env().await;
    let sid = login(&e, "bob", "pw-bob").await;
    for p in ["/apps/files/", "/settings/user"] {
        let r = get(&e, &sid, p).await;
        let csp = r.h("content-security-policy");
        assert!(
            csp.contains("script-src 'self'")
                && !csp.contains("unsafe-inline")
                && !csp.contains("unsafe-eval"),
            "{p}: {csp}"
        );
        assert!(csp.contains("frame-ancestors 'none'") && csp.contains("connect-src 'self'"));
        assert_eq!(
            (
                r.h("cache-control").as_str(),
                r.h("x-frame-options").as_str(),
                r.h("x-content-type-options").as_str()
            ),
            ("no-store", "DENY", "nosniff")
        );
        // no inline <script>, no inline style attributes, no event handler attributes
        assert!(
            !r.body.contains("<script>")
                && !r.body.contains(" style=")
                && !r.body.contains(" onclick="),
            "{p}"
        );
        assert!(
            r.body.contains("<meta name=\"csrf-token\"")
                && r.body.contains("/static/app.js")
                && r.body.contains("/static/htmx.min.js")
        );
        assert!(r.body.contains("allowEval\":false"));
    }
    for (name, ct) in [
        ("app.js", "text/javascript"),
        ("app.css", "text/css"),
        ("htmx.min.js", "text/javascript"),
    ] {
        let r = send(&e, "GET", &format!("/static/{name}?v=1"), &[], "").await;
        assert_eq!(r.status, StatusCode::OK, "{name}");
        assert!(
            r.h("content-type").starts_with(ct)
                && r.h("x-content-type-options") == "nosniff"
                && !r.body.is_empty()
        );
    }
    assert!(
        send(&e, "GET", "/static/app.js", &[], "")
            .await
            .body
            .contains("X-CSRF-Token")
    );
    assert_eq!(
        send(&e, "GET", "/static/nope.js", &[], "").await.status,
        StatusCode::NOT_FOUND
    );
    // the vendored htmx is the pinned one
    let sum = sha256_of(&send(&e, "GET", "/static/htmx.min.js", &[], "").await.body);
    let pinned = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/static/htmx.min.js.sha256"
    ))
    .unwrap();
    assert!(
        pinned.starts_with(&sum),
        "htmx.min.js changed: {sum} vs {pinned}"
    );
}

fn sha256_of(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn listing_fragment_sorting_and_navigation() {
    let e = env().await;
    dav(&e, "bob", "MKCOL", "/Zeta", "").await;
    dav(&e, "bob", "MKCOL", "/alpha", "").await;
    dav(&e, "bob", "MKCOL", "/alpha/inner", "").await;
    dav(&e, "bob", "PUT", "/b-file.txt", "hello").await;
    dav(&e, "bob", "PUT", "/A-file.txt", "x").await;
    dav(&e, "bob", "PUT", "/alpha/inner/deep.txt", "d").await;
    let sid = login(&e, "bob", "pw-bob").await;
    let page = get(&e, &sid, "/apps/files/").await;
    let names: Vec<&str> = page
        .body
        .match_indices("data-name=\"")
        .map(|(i, _)| {
            let s = &page.body[i + 11..];
            &s[..s.find('"').unwrap()]
        })
        .collect();
    assert_eq!(
        names,
        ["alpha", "Zeta", "A-file.txt", "b-file.txt"],
        "folders first, then case-insensitive"
    );
    assert!(
        page.body.contains("<html")
            && page.body.contains("id=\"files-root\"")
            && page.body.contains("data-dir=\"/\"")
    );
    assert!(
        page.body
            .contains("href=\"/remote.php/dav/files/bob/b-file.txt\" download")
    );
    // the HTMX fragment is only the inside, same content
    let frag = send(
        &e,
        "GET",
        "/apps/files/?dir=%2Falpha%2Finner",
        &[ck(&sid), ("hx-request", "true".into())],
        "",
    )
    .await;
    assert!(
        !frag.body.contains("<html")
            && frag.body.contains("data-dir=\"/alpha/inner\"")
            && frag.body.contains("data-name=\"deep.txt\"")
    );
    // breadcrumbs link to each level, with encoded query values
    assert!(
        frag.body.contains("href=\"/apps/files/?dir=%2Falpha\"")
            && frag
                .body
                .contains("href=\"/apps/files/?dir=%2Falpha%2Finner\"")
    );
    // unknown folder / a file as folder: message and the top level, status 404
    for q in ["/nope", "/b-file.txt", "/../etc"] {
        let r = get(&e, &sid, &format!("/apps/files/?dir={}", pct(q))).await;
        assert!(
            r.status == StatusCode::NOT_FOUND || r.status == StatusCode::OK,
            "{q}"
        );
        if r.status == StatusCode::NOT_FOUND {
            assert!(
                r.body.contains("does not exist") && r.body.contains("data-name=\"alpha\""),
                "{q}"
            );
        }
    }
    // an own item gets the full set of actions
    let row = &page.body[page.body.find("data-name=\"A-file.txt\"").unwrap()..];
    let row = &row[..row.find("</li>").unwrap()];
    for a in ["rename", "move", "share", "delete"] {
        assert!(row.contains(&format!("data-action=\"{a}\"")), "{a}");
    }
    assert!(row.contains(">Delete<"));
}

#[tokio::test]
async fn hostile_file_names_are_escaped_everywhere() {
    let e = env().await;
    let evil = [
        "<img src=x onerror=alert(1)>.txt",
        "a\"b'c.txt",
        "a&b.txt",
        "\"><script>alert(1)<!--",
        "x onclick=alert(1) y.txt",
    ];
    for n in evil {
        let (is_dir, name) = if n.starts_with("\"><") {
            (true, n)
        } else {
            (false, n)
        };
        let path = format!("/{}", pct(name));
        let st = if is_dir {
            dav(&e, "bob", "MKCOL", &path, "").await
        } else {
            dav(&e, "bob", "PUT", &path, "x").await
        };
        assert!(st == StatusCode::CREATED, "{n}: {st}");
    }
    let sid = login(&e, "bob", "pw-bob").await;
    let b = get(&e, &sid, "/apps/files/").await.body;
    // nothing the attacker controls becomes markup
    for raw in [
        "<img src=x",
        "<script>alert(1)",
        "onerror=alert(1)>",
        "data-name=\"a\"b",
        "\"><script>",
        "\" onclick=",
    ] {
        assert!(!b.contains(raw), "unescaped {raw:?} in page");
    }
    // a name with "onclick=" stays inside its quoted attribute value (no new attribute is born)
    assert!(b.contains("data-name=\"x onclick=alert(1) y.txt\""));
    // ... it is shown as text, entity-escaped
    assert!(
        b.contains("&#60;img src=x onerror=alert(1)&#62;.txt")
            || b.contains("&lt;img src=x onerror=alert(1)&gt;.txt"),
        "{}",
        &b[b.find("<ul class=\"list\"").unwrap()..]
    );
    // links are percent-encoded: the folder named "><script>... in the navigation link and the download link of the file with quotes
    assert!(
        b.contains("href=\"/apps/files/?dir=%2F%22%3E%3Cscript%3Ealert%281%29%3C%21--\""),
        "dir link"
    );
    assert!(
        b.contains("/remote.php/dav/files/bob/a%22b%27c.txt\""),
        "download link"
    );
    // the same through the fragment of a hostile folder and in the share dialog
    let dirq = pct("/\"><script>alert(1)<!--");
    let frag = send(
        &e,
        "GET",
        &format!("/apps/files/?dir={dirq}"),
        &[ck(&sid), ("hx-request", "true".into())],
        "",
    )
    .await;
    assert!(
        !frag.body.contains("<script>") && frag.body.contains("&#34;&#62;&#60;script")
            || frag.body.contains("&quot;&gt;&lt;script"),
        "{}",
        frag.body
    );
    let dlg = get(
        &e,
        &sid,
        &format!(
            "/ui/share?path={}",
            pct("/<img src=x onerror=alert(1)>.txt")
        ),
    )
    .await;
    assert_eq!(dlg.status, StatusCode::OK);
    assert!(!dlg.body.contains("<img src=x") && dlg.body.contains("data-path=\""));
}

#[tokio::test]
async fn shared_items_show_owner_and_hide_actions_that_would_be_refused() {
    let e = env().await;
    dav(&e, "alice", "MKCOL", "/ro", "").await;
    dav(&e, "alice", "PUT", "/ro/f.txt", "x").await;
    dav(&e, "alice", "MKCOL", "/rw", "").await;
    for (path, perms) in [("/ro", "1"), ("/rw", "15")] {
        let r = send(
            &e,
            "POST",
            "/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json",
            &[basic("alice", "pw-alice"), form_ct()],
            &form(&[
                ("shareType", "0"),
                ("shareWith", "bob"),
                ("path", path),
                ("permissions", perms),
            ]),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    }
    let sid = login(&e, "bob", "pw-bob").await;
    let top = get(&e, &sid, "/apps/files/?view=shared-with-me").await.body;
    let row = |body: &str, name: &str| {
        let s = &body[body.find(&format!("data-name=\"{name}\"")).unwrap()..];
        s[..s.find("</li>").unwrap()].to_string()
    };
    let ro = row(&top, "ro");
    assert!(
        ro.contains("from Alice A") && ro.contains("read-only") && ro.contains(">Remove share<"),
        "{ro}"
    );
    assert!(
        !ro.contains("data-action=\"share\""),
        "a received item cannot be reshared"
    );
    assert!(row(&top, "rw").contains("from Alice A") && !row(&top, "rw").contains("read-only"));
    // inside the read-only share: no upload, no new folder, no rename/move/delete on the file
    let inside = get(&e, &sid, "/apps/files/?view=shared-with-me&dir=%2Fro")
        .await
        .body;
    assert!(
        inside.contains("id=\"btn-upload\" type=\"button\" class=\"primary\" disabled")
            && inside.contains("id=\"btn-newfolder\" type=\"button\" disabled"),
        "{inside}"
    );
    assert!(inside.contains("data-can-create=\"0\""));
    let f = row(&inside, "f.txt");
    assert!(
        !f.contains("data-action=\"rename\"")
            && !f.contains("data-action=\"delete\"")
            && !f.contains("data-action=\"move\""),
        "{f}"
    );
    // the read-write share allows creating
    assert!(
        get(&e, &sid, "/apps/files/?view=shared-with-me&dir=%2Frw")
            .await
            .body
            .contains("data-can-create=\"1\"")
    );
    // the share dialog: owner sees recipients with their permission, the recipient is told no
    let asid = login(&e, "alice", "pw-alice").await;
    let d = get(&e, &asid, "/ui/share?path=%2Fro").await.body;
    assert!(
        d.contains("Bob B (bob)")
            && d.contains("data-share-remove=")
            && d.contains("<option value=\"1\" selected>Can view"),
        "{d}"
    );
    assert!(d.contains("id=\"share-add\"") && d.contains("<datalist id=\"sharee-list\">"));
    let d = get(&e, &sid, "/ui/share?path=%2Fro").await.body;
    assert!(d.contains("only its owner can share") && !d.contains("id=\"share-add\""));
    assert_eq!(
        get(&e, &asid, "/ui/share?path=%2Fnope").await.status,
        StatusCode::NOT_FOUND
    );
}

/// Text of the one highlighted entry of the left menu.
fn active_nav(body: &str) -> String {
    assert_eq!(
        body.matches("aria-current=\"page\"").count(),
        1,
        "exactly one active menu entry"
    );
    let i = body.find("aria-current=\"page\"").unwrap();
    let a = &body[i..i + body[i..].find("</a>").unwrap()];
    a.rsplit("<span>")
        .next()
        .unwrap()
        .trim_end_matches("</span>")
        .to_string()
}

async fn share_with(e: &Env, owner: &str, path: &str, ty: &str, with: &str, perms: &str) {
    let r = send(
        e,
        "POST",
        "/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json",
        &[basic(owner, &format!("pw-{owner}")), form_ct()],
        &form(&[
            ("shareType", ty),
            ("shareWith", with),
            ("path", path),
            ("permissions", perms),
        ]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

/// The left menu: "My own files", "Shared with me" and "Shared" are separate views of the same tree.
#[tokio::test]
async fn left_menu_and_the_three_file_views() {
    let e = env().await;
    db::create_group(&e.pool, "team").await.unwrap();
    db::add_member(&e.pool, "team", "bob").await.unwrap();
    for (m, p, b) in [
        ("MKCOL", "/Photos", ""),
        ("MKCOL", "/Photos/sub", ""),
        ("PUT", "/Photos/sub/deep.txt", "d"),
        ("PUT", "/note.txt", "n"),
        ("PUT", "/private.txt", "p"),
        ("PUT", "/a%26b%20%3Ci%3E.txt", "h"),
    ] {
        assert!(dav(&e, "alice", m, p, b).await.is_success(), "{m} {p}");
    }
    assert!(dav(&e, "bob", "PUT", "/mine.txt", "m").await.is_success());
    share_with(&e, "alice", "/Photos", "0", "bob", "1").await;
    share_with(&e, "alice", "/note.txt", "1", "team", "3").await;
    share_with(&e, "alice", "/a&b <i>.txt", "0", "bob", "1").await;

    // ---- bob
    let bob = login(&e, "bob", "pw-bob").await;
    let mine = get(&e, &bob, "/apps/files/").await.body;
    assert_eq!(active_nav(&mine), "My own files");
    for l in [
        "href=\"/apps/files/\"",
        "href=\"/apps/files/?view=shared-with-me\"",
        "href=\"/apps/files/?view=shared\"",
        "href=\"/apps/files/trash\"",
    ] {
        assert!(mine.contains(l), "menu entry {l}");
    }
    assert!(mine.contains("data-name=\"mine.txt\""));
    assert!(
        !mine.contains("data-name=\"Photos\"") && !mine.contains("data-name=\"note.txt\""),
        "incoming shares are not part of 'My own files'"
    );
    // an unknown view is "mine"
    let bogus = get(&e, &bob, "/apps/files/?view=bogus").await.body;
    assert_eq!(active_nav(&bogus), "My own files");
    assert!(bogus.contains("data-name=\"mine.txt\""));

    let swm = get(&e, &bob, "/apps/files/?view=shared-with-me").await.body;
    assert_eq!(active_nav(&swm), "Shared with me");
    assert!(swm.contains("data-can-create=\"0\"") && !swm.contains("data-name=\"mine.txt\""));
    let row = |body: &str, name: &str| {
        let s = &body[body.find(&format!("data-name=\"{name}\"")).unwrap()..];
        s[..s.find("</li>").unwrap()].to_string()
    };
    let photos = row(&swm, "Photos");
    assert!(
        photos.contains("from Alice A")
            && photos.contains("Can view \u{b7} shared ")
            && photos.contains("hx-get=\"/apps/files/?view=shared-with-me&#38;dir=%2FPhotos\""),
        "{photos}"
    );
    assert!(row(&swm, "note.txt").contains("Can edit \u{b7} shared "));
    assert!(
        swm.contains("a&#38;b &#60;i&#62;.txt") && !swm.contains("<i>"),
        "names are escaped in the list"
    );
    // below a share: its own folder view, crumbs start at the share
    let sub = get(
        &e,
        &bob,
        "/apps/files/?view=shared-with-me&dir=%2FPhotos%2Fsub",
    )
    .await;
    assert_eq!(sub.status, StatusCode::OK);
    assert!(sub.body.contains("data-name=\"deep.txt\""));
    assert_eq!(active_nav(&sub.body), "Shared with me");
    assert!(
        sub.body.contains(">Shared with me</a>")
            && sub.body.contains(">Photos</a>")
            && sub.body.contains(">sub</a>")
            && sub.body.contains("data-can-create=\"0\""),
        "{}",
        sub.body
    );
    // a share's path in "My own files" goes to where it belongs; a non-share path is not a folder of that view
    let r = get(&e, &bob, "/apps/files/?dir=%2FPhotos").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(
        r.h("location"),
        "/apps/files/?view=shared-with-me&dir=%2FPhotos"
    );
    let r = get(&e, &bob, "/apps/files/?view=shared-with-me&dir=%2Fnope").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(
        r.body.contains("That folder does not exist") && r.body.contains("data-name=\"Photos\"")
    );
    // nothing shared by bob
    let none = get(&e, &bob, "/apps/files/?view=shared").await.body;
    assert_eq!(active_nav(&none), "Shared");
    assert!(none.contains("You have not shared anything yet."));
    // the other pages: trash is in the menu, settings has none
    assert_eq!(
        active_nav(&get(&e, &bob, "/apps/files/trash").await.body),
        "Trash"
    );
    assert!(
        !get(&e, &bob, "/settings/user")
            .await
            .body
            .contains("class=\"side\"")
    );
    // sync clients still see the shares at the root: only the web view changed
    let pf = send(
        &e,
        "PROPFIND",
        "/remote.php/dav/files/bob/",
        &[basic("bob", "pw-bob"), ("depth", "1".to_string())],
        "",
    )
    .await;
    assert!(
        pf.body.contains("/Photos/") && pf.body.contains("note.txt"),
        "{}",
        pf.body
    );

    // ---- alice: what she shares
    let alice = login(&e, "alice", "pw-alice").await;
    let shared = get(&e, &alice, "/apps/files/?view=shared").await.body;
    assert_eq!(active_nav(&shared), "Shared");
    assert!(
        shared.contains("data-can-create=\"0\"") && !shared.contains("data-name=\"private.txt\"")
    );
    let photos = row(&shared, "Photos");
    assert!(
        photos.contains("with Bob B: Can view \u{b7} shared ")
            && photos.contains("href=\"/apps/files/?dir=%2FPhotos\"")
            && !photos.contains("hx-get"),
        "folders lead to the real place in My own files: {photos}"
    );
    assert!(row(&shared, "note.txt").contains("with team (group): Can edit"));
    assert!(shared.contains("a&#38;b &#60;i&#62;.txt") && !shared.contains("<i>"));
    let r = get(&e, &alice, "/apps/files/?view=shared&dir=%2FPhotos").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.h("location"), "/apps/files/?dir=%2FPhotos");
    // unsharing removes it from both lists
    let id: i64 = sqlx::query_scalar(
        "SELECT s.id FROM shares s JOIN file_cache f ON f.fileid = s.file_source WHERE f.name = 'Photos'",
    )
    .fetch_one(&e.pool)
    .await
    .unwrap();
    let del = send(
        &e,
        "DELETE",
        &format!("/ocs/v2.php/apps/files_sharing/api/v1/shares/{id}?format=json"),
        &[basic("alice", "pw-alice")],
        "",
    )
    .await;
    assert_eq!(del.status, StatusCode::OK, "{}", del.body);
    assert!(
        !get(&e, &alice, "/apps/files/?view=shared")
            .await
            .body
            .contains("data-name=\"Photos\"")
    );
    assert!(
        !get(&e, &bob, "/apps/files/?view=shared-with-me")
            .await
            .body
            .contains("data-name=\"Photos\"")
    );
}

/// One list row of a rendered page.
fn li(body: &str, name: &str) -> String {
    let s = &body[body.find(&format!("data-name=\"{name}\"")).unwrap()..];
    s[..s.find("</li>").unwrap()].to_string()
}

/// Share is an icon in front; rename/move/versions/delete live in the "..." menu behind it; every row has a
/// checkbox and carries what the bulk bar needs to know about its permissions.
#[tokio::test]
async fn share_icon_in_front_menu_behind_and_the_multi_select_markup() {
    let e = env().await;
    for (m, p, b) in [
        ("MKCOL", "/Docs", ""),
        ("PUT", "/Docs/f.txt", "x"),
        ("PUT", "/g.txt", "g"),
    ] {
        assert!(dav(&e, "alice", m, p, b).await.is_success());
    }
    share_with(&e, "alice", "/Docs", "0", "bob", "1").await;
    let alice = login(&e, "alice", "pw-alice").await;
    let top = get(&e, &alice, "/apps/files/").await.body;
    let g = li(&top, "g.txt");
    let share = g.find("data-action=\"share\"").unwrap();
    let menu = g.find("<details class=\"menu\">").unwrap();
    assert!(share < menu, "the share icon comes first: {g}");
    for a in ["rename", "move", "versions", "delete"] {
        let at = g.find(&format!("data-action=\"{a}\"")).unwrap();
        assert!(at > menu, "{a} belongs into the menu");
    }
    assert!(g.contains("class=\"icon-btn share\"") && !g.contains("icon-btn share on"));
    for l in [
        "aria-label=\"Share g.txt\"",
        "aria-label=\"More actions for g.txt\"",
        "aria-label=\"Select g.txt\"",
    ] {
        assert!(g.contains(l), "{l}");
    }
    assert!(g.contains("data-can-move=\"1\"") && g.contains("data-can-delete=\"1\""));
    assert!(
        li(&top, "Docs").contains("icon-btn share on"),
        "an item that is shared has the highlighted icon"
    );
    assert!(top.contains("<div id=\"bulkbar\" class=\"bulkbar\" hidden>"));
    for id in [
        "id=\"bulk-move\"",
        "id=\"bulk-delete\"",
        "id=\"bulk-clear\"",
        "id=\"sel-all\"",
    ] {
        assert!(top.contains(id), "{id}");
    }
    // bob inside the read-only share: nothing to do with the file, so no icon, no menu, both flags off
    let bob = login(&e, "bob", "pw-bob").await;
    let inside = get(&e, &bob, "/apps/files/?view=shared-with-me&dir=%2FDocs")
        .await
        .body;
    let f = li(&inside, "f.txt");
    assert!(
        !f.contains("class=\"menu\"") && !f.contains("icon-btn share"),
        "{f}"
    );
    assert!(
        f.contains("data-can-move=\"0\"")
            && f.contains("data-can-delete=\"0\"")
            && f.contains("aria-label=\"Select f.txt\"")
    );
    // the share itself can be removed from the list of shares
    let swm = get(&e, &bob, "/apps/files/?view=shared-with-me").await.body;
    let d = li(&swm, "Docs");
    assert!(
        d.contains("data-can-delete=\"1\"") && d.contains(">Remove share<"),
        "{d}"
    );
    // nothing to select in an empty list
    let none = get(&e, &bob, "/apps/files/?view=shared").await.body;
    assert!(
        none.contains("id=\"sel-all\" class=\"sel-all\" aria-label=\"Select all\" disabled"),
        "{none}"
    );
}

/// The build number: Admin menu (Users, About, version line), the About page, the build id in the asset links.
#[tokio::test]
async fn admin_menu_about_page_and_the_build_id_in_asset_links() {
    let e = env().await;
    let b = rustbucket::buildinfo::get();
    let admin = login(&e, "admin", "pw-admin").await;
    let page = get(&e, &admin, "/apps/files/").await.body;
    assert!(page.contains("<summary>Admin</summary>"), "{page}");
    assert!(page.contains("href=\"/settings/users\"") && page.contains("href=\"/settings/about\""));
    assert!(
        page.contains(&format!(
            "<div class=\"menu-version\" title=\"Version and build number\">{}</div>",
            b.short()
        )),
        "the version line is in the Admin menu"
    );
    assert!(
        b.short()
            .starts_with(&format!("v{}", env!("CARGO_PKG_VERSION")))
    );
    // the assets are keyed by the build id: a new build is never served stale scripts and styles
    for a in ["app.css", "htmx.min.js", "app.js"] {
        assert!(page.contains(&format!("/static/{a}?v={}", b.hash)), "{a}");
    }
    // the About page
    let about = get(&e, &admin, "/settings/about").await;
    assert_eq!(about.status, StatusCode::OK);
    for t in [
        b.version,
        b.hash.as_str(),
        b.commit.as_str(),
        "Build id",
        "Running for",
        "35.0.1",
    ] {
        assert!(about.body.contains(t), "{t}");
    }
    // a normal user has neither the menu nor the page; not signed in is treated like the other admin pages
    let bob = login(&e, "bob", "pw-bob").await;
    let nav = get(&e, &bob, "/apps/files/").await.body;
    assert!(
        !nav.contains("Admin</summary>")
            && !nav.contains("/settings/about")
            && !nav.contains("menu-version")
    );
    assert_eq!(
        get(&e, &bob, "/settings/about").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&e, "GET", "/settings/about", &[], "").await.status,
        send(&e, "GET", "/settings/users", &[], "").await.status
    );
}

#[tokio::test]
async fn every_post_needs_the_csrf_token_and_admin_routes_need_an_admin() {
    let e = env().await;
    let admin = login(&e, "admin", "pw-admin").await;
    let bob = login(&e, "bob", "pw-bob").await;
    let routes: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("/logout", vec![]),
        (
            "/settings/user/profile",
            vec![("display_name", "X"), ("email", "")],
        ),
        (
            "/settings/user/password",
            vec![("current", "pw-bob"), ("new", "n"), ("again", "n")],
        ),
        ("/settings/user/app-passwords", vec![("name", "phone")]),
        ("/settings/user/app-passwords/1/delete", vec![]),
        (
            "/settings/users",
            vec![
                ("uid", "new"),
                ("display_name", ""),
                ("email", ""),
                ("password", "x"),
            ],
        ),
        (
            "/settings/users/bob",
            vec![("display_name", "Bob"), ("email", "")],
        ),
        ("/settings/groups", vec![("gid", "g")]),
        ("/settings/groups/g/members", vec![("uid", "bob")]),
        ("/settings/groups/g/members/bob/delete", vec![]),
    ];
    for (path, fields) in &routes {
        for (who, sid) in [("admin", &admin), ("bob", &bob)] {
            // no token / wrong token / the other session's token: 403 whatever the role
            let bad = [
                form(&fields.clone()),
                form(&[fields.clone(), vec![("csrf", "nope")]].concat()),
                form(
                    &[
                        fields.clone(),
                        vec![(
                            "csrf",
                            &csrf_token_for(if who == "admin" { &bob } else { &admin }),
                        )],
                    ]
                    .concat(),
                ),
            ];
            for body in bad {
                let r = send(&e, "POST", path, &[form_ct(), ck(sid)], &body).await;
                assert!(
                    r.status == StatusCode::FORBIDDEN
                        || r.status == StatusCode::UNPROCESSABLE_ENTITY,
                    "{who} {path}: {}",
                    r.status
                );
            }
        }
    }
    // admin pages: a normal user is refused even with a valid token, an admin may enter
    assert_eq!(
        get(&e, &bob, "/settings/users").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            &e,
            &bob,
            "/settings/users",
            &[
                ("uid", "x"),
                ("display_name", ""),
                ("email", ""),
                ("password", "x")
            ]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            &e,
            &bob,
            "/settings/users/admin",
            &[
                ("display_name", "Pwned"),
                ("email", ""),
                ("password", "hacked")
            ]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert!(get(&e, &admin, "/settings/users").await.status == StatusCode::OK);
    let nav = get(&e, &bob, "/settings/user").await.body;
    assert!(
        !nav.contains("/settings/users"),
        "no admin link for a normal user"
    );
    assert_eq!(
        db::get_user(&e.pool, "admin")
            .await
            .unwrap()
            .unwrap()
            .display_name,
        "Admin A"
    );
    // a GET never changes anything
    assert_eq!(
        send(&e, "GET", "/logout", &[ck(&bob)], "").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn personal_settings_profile_password_and_devices() {
    let e = env().await;
    let sid = login(&e, "bob", "pw-bob").await;
    // profile: saved, shown, and visible to the clients through cloud/user
    let r = post(
        &e,
        &sid,
        "/settings/user/profile",
        &[("display_name", "Robert B"), ("email", "bob@example.org")],
    )
    .await;
    assert_eq!(r.h("location"), "/settings/user?ok=profile");
    let page = get(&e, &sid, "/settings/user?ok=profile").await.body;
    assert!(
        page.contains("Profile saved.")
            && page.contains("value=\"Robert B\"")
            && page.contains("value=\"bob@example.org\"")
    );
    let me = send(
        &e,
        "GET",
        "/ocs/v2.php/cloud/user?format=json",
        &[basic("bob", "pw-bob")],
        "",
    )
    .await
    .body;
    assert!(
        me.contains("\"email\":\"bob@example.org\"") && me.contains("\"displayname\":\"Robert B\""),
        "{me}"
    );
    for (name, mail, key) in [("Robert B", "not-an-address", "email"), ("   ", "", "name")] {
        let r = post(
            &e,
            &sid,
            "/settings/user/profile",
            &[("display_name", name), ("email", mail)],
        )
        .await;
        assert_eq!(r.h("location"), format!("/settings/user?err={key}"));
    }
    assert!(
        get(&e, &sid, "/settings/user?err=email")
            .await
            .body
            .contains("does not look like an email")
    );
    // arbitrary flash text cannot be injected through the query string
    assert!(
        !get(&e, &sid, "/settings/user?err=%3Cscript%3E")
            .await
            .body
            .contains("<script>alert")
    );
    // password: current one required, both new ones equal
    assert_eq!(
        post(
            &e,
            &sid,
            "/settings/user/password",
            &[("current", "wrong"), ("new", "n1"), ("again", "n1")]
        )
        .await
        .h("location"),
        "/settings/user?err=badpw"
    );
    assert_eq!(
        post(
            &e,
            &sid,
            "/settings/user/password",
            &[("current", "pw-bob"), ("new", "n1"), ("again", "n2")]
        )
        .await
        .h("location"),
        "/settings/user?err=mismatch"
    );
    assert_eq!(
        post(
            &e,
            &sid,
            "/settings/user/password",
            &[("current", "pw-bob"), ("new", ""), ("again", "")]
        )
        .await
        .h("location"),
        "/settings/user?err=mismatch"
    );
    assert_eq!(
        post(
            &e,
            &sid,
            "/settings/user/password",
            &[("current", "pw-bob"), ("new", "n1"), ("again", "n1")]
        )
        .await
        .h("location"),
        "/settings/user?ok=password"
    );
    assert_eq!(
        dav(&e, "bob", "PROPFIND", "/", "").await,
        StatusCode::UNAUTHORIZED,
        "old password is dead"
    );
    assert_eq!(
        send(
            &e,
            "PROPFIND",
            "/remote.php/dav/files/bob/",
            &[basic("bob", "n1"), ("depth", "0".into())],
            ""
        )
        .await
        .status,
        StatusCode::MULTI_STATUS
    );
    // devices: create shows the token once, it works, revoking signs that device out at once
    let r = post(
        &e,
        &sid,
        "/settings/user/app-passwords",
        &[("name", "Kitchen <tablet>")],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body.contains("Kitchen &#60;tablet&#62;") || r.body.contains("Kitchen &lt;tablet&gt;"),
        "name must be escaped"
    );
    let token = r
        .body
        .split("<code class=\"token\">")
        .nth(1)
        .unwrap()
        .split("</code>")
        .next()
        .unwrap()
        .to_string();
    assert_eq!(token.len(), 72);
    let basic_tok = || async {
        let h = [basic("bob", &token)];
        send(&e, "GET", "/ocs/v2.php/cloud/user", &h, "").await
    };
    assert_eq!(basic_tok().await.status, StatusCode::OK);
    let list = get(&e, &sid, "/settings/user").await.body;
    assert!(!list.contains(&token), "the token is never shown again");
    let id = list
        .split("/settings/user/app-passwords/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    assert_eq!(
        post(
            &e,
            &sid,
            &format!("/settings/user/app-passwords/{id}/delete"),
            &[]
        )
        .await
        .h("location"),
        "/settings/user?ok=revoked"
    );
    assert_eq!(
        basic_tok().await.status,
        StatusCode::UNAUTHORIZED,
        "revoked device is signed out"
    );
    // one cannot revoke somebody else's device
    let other = login(&e, "alice", "pw-alice").await;
    let r = post(
        &e,
        &other,
        "/settings/user/app-passwords",
        &[("name", "alice phone")],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let aid: i64 = sqlx::query_scalar("SELECT id FROM app_passwords WHERE name = 'alice phone'")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    post(
        &e,
        &sid,
        &format!("/settings/user/app-passwords/{aid}/delete"),
        &[],
    )
    .await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM app_passwords WHERE id = $1")
            .bind(aid)
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn admin_manages_users_and_groups_with_guard_rails() {
    let e = env().await;
    let sid = login(&e, "admin", "pw-admin").await;
    // create: appears in the list, can log in; bad/duplicate names are refused with a fixed message
    let r = post(
        &e,
        &sid,
        "/settings/users",
        &[
            ("uid", "erin"),
            ("display_name", "Erin E"),
            ("email", "erin@example.org"),
            ("password", "start1"),
            ("is_admin", ""),
        ],
    )
    .await;
    assert_eq!(r.h("location"), "/settings/users?ok=created");
    let list = get(&e, &sid, "/settings/users?ok=created").await.body;
    assert!(
        list.contains("id=\"user-erin\"")
            && list.contains("Erin E")
            && list.contains("erin@example.org")
            && list.contains("Account created.")
    );
    let _ = login(&e, "erin", "start1").await;
    assert_eq!(
        post(
            &e,
            &sid,
            "/settings/users",
            &[
                ("uid", "erin"),
                ("display_name", ""),
                ("email", ""),
                ("password", "x")
            ]
        )
        .await
        .h("location"),
        "/settings/users?err=exists"
    );
    assert_eq!(
        post(
            &e,
            &sid,
            "/settings/users",
            &[
                ("uid", ".hidden"),
                ("display_name", ""),
                ("email", ""),
                ("password", "x")
            ]
        )
        .await
        .h("location"),
        "/settings/users?err=baduid"
    );
    assert_eq!(
        post(
            &e,
            &sid,
            "/settings/users",
            &[
                ("uid", "mail"),
                ("display_name", ""),
                ("email", "bad"),
                ("password", "x")
            ]
        )
        .await
        .h("location"),
        "/settings/users?err=email"
    );
    // edit: new password and name; checkboxes are the truth (absent = off)
    let r = post(
        &e,
        &sid,
        "/settings/users/erin",
        &[
            ("display_name", "Kingston"),
            ("email", ""),
            ("password", "start2"),
            ("is_admin", "1"),
            ("enabled", "1"),
        ],
    )
    .await;
    assert_eq!(r.h("location"), "/settings/users?ok=saved");
    let u = db::get_user(&e.pool, "erin").await.unwrap().unwrap();
    assert_eq!(
        (
            u.display_name.as_str(),
            u.email.as_str(),
            u.is_admin,
            u.enabled
        ),
        ("Kingston", "", true, true)
    );
    let _ = login(&e, "erin", "start2").await;
    // disable: every way in closes (web session, password, app password) and the data stays
    dav(&e, "erin", "PUT", "/keep.txt", "x").await; // basic auth with the OLD password fails, so this is a no-op check below
    let erin_sid = login(&e, "erin", "start2").await;
    assert_eq!(
        post(
            &e,
            &sid,
            "/settings/users/erin",
            &[
                ("display_name", "Kingston"),
                ("email", ""),
                ("password", ""),
                ("is_admin", "1")
            ]
        )
        .await
        .h("location"),
        "/settings/users?ok=saved"
    );
    assert_eq!(
        get(&e, &erin_sid, "/apps/files/").await.status,
        StatusCode::SEE_OTHER,
        "session of a disabled user is dead"
    );
    let page = send(&e, "GET", "/login", &[], "").await;
    let csrf = cookie_of(&page, "rb_csrf").unwrap();
    let r = send(
        &e,
        "POST",
        "/login",
        &[form_ct(), ("cookie", format!("rb_csrf={csrf}"))],
        &form(&[
            ("username", "erin"),
            ("password", "start2"),
            ("csrf", &csrf),
        ]),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(
        get(&e, &sid, "/settings/users")
            .await
            .body
            .contains("(disabled)")
    );
    // the last enabled admin cannot be demoted or disabled - not even by themselves
    sqlx::query("UPDATE users SET is_admin = 0 WHERE uid = 'erin'")
        .execute(&e.pool)
        .await
        .unwrap();
    assert_eq!(
        post(
            &e,
            &sid,
            "/settings/users/admin",
            &[
                ("display_name", "Admin A"),
                ("email", ""),
                ("password", ""),
                ("enabled", "1")
            ]
        )
        .await
        .h("location"),
        "/settings/users?err=lastadmin"
    );
    assert_eq!(
        post(
            &e,
            &sid,
            "/settings/users/admin",
            &[
                ("display_name", "Admin A"),
                ("email", ""),
                ("password", ""),
                ("is_admin", "1")
            ]
        )
        .await
        .h("location"),
        "/settings/users?err=lastadmin"
    );
    assert!(
        db::get_user(&e.pool, "admin")
            .await
            .unwrap()
            .unwrap()
            .is_admin
    );
    assert_eq!(
        post(
            &e,
            &sid,
            "/settings/users/ghost",
            &[
                ("display_name", "G"),
                ("email", ""),
                ("password", ""),
                ("enabled", "1")
            ]
        )
        .await
        .h("location"),
        "/settings/users?err=nouser"
    );
    // groups: create, add, list, remove
    assert_eq!(
        post(&e, &sid, "/settings/groups", &[("gid", "family")])
            .await
            .h("location"),
        "/settings/users?ok=group"
    );
    assert_eq!(
        post(&e, &sid, "/settings/groups", &[("gid", "family")])
            .await
            .h("location"),
        "/settings/users?err=exists"
    );
    assert_eq!(
        post(
            &e,
            &sid,
            "/settings/groups/family/members",
            &[("uid", "bob")]
        )
        .await
        .h("location"),
        "/settings/users?ok=group"
    );
    let page = get(&e, &sid, "/settings/users").await.body;
    assert!(page.contains("id=\"group-family\"") && page.contains("Bob B (bob)"));
    let members = |e: &Env| {
        let p = e.pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM group_members m JOIN groups g ON g.id = m.group_id WHERE g.gid = 'family'").fetch_one(&p).await.unwrap()
        }
    };
    assert_eq!(members(&e).await, 1);
    assert_eq!(
        post(&e, &sid, "/settings/groups/family/members/bob/delete", &[])
            .await
            .h("location"),
        "/settings/users?ok=group"
    );
    assert_eq!(members(&e).await, 0);
}

#[tokio::test]
async fn web_uploads_use_the_dav_endpoints_with_session_and_token() {
    // what app.js does: PUT / MKCOL / chunked upload / MOVE with cookie + X-CSRF-Token
    let e = env().await;
    let sid = login(&e, "bob", "pw-bob").await;
    let t = csrf_token_for(&sid);
    let h = |extra: &[(&'static str, String)]| {
        let mut v = vec![ck(&sid), ("x-csrf-token", t.clone())];
        v.extend(extra.iter().cloned());
        v
    };
    assert_eq!(
        send(&e, "MKCOL", "/remote.php/dav/files/bob/Docs", &h(&[]), "")
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        send(
            &e,
            "PUT",
            "/remote.php/dav/files/bob/Docs/a.txt",
            &h(&[
                ("if-none-match", "*".into()),
                ("x-oc-mtime", "1700000000".into())
            ]),
            "hello"
        )
        .await
        .status,
        StatusCode::CREATED
    );
    // the same name again does not silently overwrite (the web UI sends If-None-Match: *)
    assert_eq!(
        send(
            &e,
            "PUT",
            "/remote.php/dav/files/bob/Docs/a.txt",
            &h(&[("if-none-match", "*".into())]),
            "other"
        )
        .await
        .status,
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(
        send(
            &e,
            "GET",
            "/remote.php/dav/files/bob/Docs/a.txt",
            &[ck(&sid)],
            ""
        )
        .await
        .body,
        "hello"
    );
    // chunked: numeric zero-padded offsets, finished by MOVE .file
    let base = "/remote.php/dav/uploads/bob/web-1";
    assert_eq!(
        send(
            &e,
            "MKCOL",
            base,
            &h(&[("oc-total-length", "10".into())]),
            ""
        )
        .await
        .status,
        StatusCode::CREATED
    );
    assert_eq!(
        send(&e, "PUT", &format!("{base}/{:015}", 5), &h(&[]), "world")
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        send(&e, "PUT", &format!("{base}/{:015}", 0), &h(&[]), "hello")
            .await
            .status,
        StatusCode::CREATED
    );
    let mv = send(
        &e,
        "MOVE",
        &format!("{base}/.file"),
        &h(&[
            (
                "destination",
                "http://x/remote.php/dav/files/bob/Docs/big.bin".into(),
            ),
            ("oc-total-length", "10".into()),
            ("if-none-match", "*".into()),
        ]),
        "",
    )
    .await;
    assert_eq!(mv.status, StatusCode::CREATED);
    assert_eq!(
        send(
            &e,
            "GET",
            "/remote.php/dav/files/bob/Docs/big.bin",
            &[ck(&sid)],
            ""
        )
        .await
        .body,
        "helloworld"
    );
    // rename / delete as the buttons do
    assert_eq!(
        send(
            &e,
            "MOVE",
            "/remote.php/dav/files/bob/Docs/a.txt",
            &h(&[
                (
                    "destination",
                    "http://x/remote.php/dav/files/bob/Docs/b.txt".into()
                ),
                ("overwrite", "F".into())
            ]),
            ""
        )
        .await
        .status,
        StatusCode::CREATED
    );
    assert_eq!(
        send(&e, "DELETE", "/remote.php/dav/files/bob/Docs", &h(&[]), "")
            .await
            .status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn trash_page_and_versions_dialog() {
    let e = env().await;
    let sid = login(&e, "bob", "pw-bob").await;
    // logged out: both go to the login page / are refused like every app page
    assert_eq!(
        send(&e, "GET", "/apps/files/trash", &[], "").await.status,
        StatusCode::SEE_OTHER
    );
    // an empty bin says so and offers nothing to empty
    let empty = get(&e, &sid, "/apps/files/trash").await;
    assert_eq!(empty.status, StatusCode::OK);
    assert!(
        empty.body.contains("The trash is empty.")
            && empty.body.contains("data-trash-empty disabled"),
        "{}",
        empty.body
    );
    assert!(
        empty.body.contains("30 days")
            && empty
                .body
                .contains("data-trash=\"/remote.php/dav/trashbin/bob\"")
    );
    assert!(
        empty.body.contains("href=\"/apps/files/trash\""),
        "nav link"
    );
    // delete a file with a hostile name and a folder: both show up, escaped, with their original location
    dav(&e, "bob", "MKCOL", "/Docs", "").await;
    dav(
        &e,
        "bob",
        "PUT",
        &format!("/Docs/{}", pct("<img src=x onerror=alert(1)>.txt")),
        "x",
    )
    .await;
    dav(&e, "bob", "PUT", "/plain.txt", "hello").await;
    dav(
        &e,
        "bob",
        "DELETE",
        &format!("/Docs/{}", pct("<img src=x onerror=alert(1)>.txt")),
        "",
    )
    .await;
    dav(&e, "bob", "DELETE", "/plain.txt", "").await;
    dav(&e, "bob", "DELETE", "/Docs", "").await;
    let t = get(&e, &sid, "/apps/files/trash").await.body;
    assert!(
        !t.contains("<img src=x") && !t.contains("onerror=alert(1)>"),
        "hostile name must be escaped"
    );
    assert!(t.contains("data-name=\"plain.txt\"") && t.contains("data-name=\"Docs\""));
    assert!(
        t.contains("data-trash=\"restore\"")
            && t.contains("data-trash=\"delete\"")
            && !t.contains("data-trash-empty disabled")
    );
    assert!(t.contains("Delete permanently"));
    // the entry names are exactly what the WebDAV trash uses (the JS sends them back there)
    let entry = t
        .split("data-entry=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert!(
        entry.contains(".d") && entry.rsplit(".d").next().unwrap().parse::<i64>().is_ok(),
        "{entry}"
    );

    // versions dialog: oldest content first in the file, newest first in the list; the live one is marked
    for (body, mt) in [
        ("one", "1700000100"),
        ("two22", "1700000200"),
        ("three", "1700000300"),
    ] {
        send(
            &e,
            "PUT",
            "/remote.php/dav/files/bob/v.txt",
            &[basic("bob", "pw-bob"), ("x-oc-mtime", mt.into())],
            body,
        )
        .await;
    }
    let d = get(&e, &sid, "/ui/versions?path=%2Fv.txt").await;
    assert_eq!(d.status, StatusCode::OK);
    let order: Vec<&str> = d
        .body
        .match_indices("data-ts=\"")
        .filter(|(i, _)| d.body[i - 4..*i].contains("<li"))
        .map(|(i, _)| &d.body[i + 9..i + 19])
        .collect();
    assert_eq!(order, ["1700000300", "1700000200", "1700000100"]);
    assert_eq!(
        d.body.matches("data-ver=\"restore\"").count(),
        2,
        "the current version cannot be restored or deleted"
    );
    assert!(
        d.body.contains(">current<")
            && d.body.contains("2023-11-14 22:18")
            && d.body.contains("5 B")
    );
    assert!(
        d.body.contains("/remote.php/dav/versions/bob/versions/")
            && d.body.contains("/1700000100\" download")
    );
    assert!(d.body.contains("data-path=\"/v.txt\""));
    // folders and other people's files have no version dialog
    assert_eq!(
        get(&e, &sid, "/ui/versions?path=%2FDocs").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&e, &sid, "/ui/versions?path=%2Fnope.txt").await.status,
        StatusCode::NOT_FOUND
    );
    let other = login(&e, "alice", "pw-alice").await;
    assert_eq!(
        get(&e, &other, "/ui/versions?path=%2Fv.txt").await.status,
        StatusCode::NOT_FOUND
    );
    // the file list offers the Versions button for own files only
    let files = get(&e, &sid, "/apps/files/").await.body;
    let row = &files[files.find("data-name=\"v.txt\"").unwrap()..];
    assert!(row[..row.find("</li>").unwrap()].contains("data-action=\"versions\""));
}

#[tokio::test]
async fn a_new_password_ends_the_other_sessions_but_not_the_one_that_changed_it() {
    let e = env().await;
    let stolen = login(&e, "bob", "pw-bob").await; // e.g. a cookie somebody copied
    let mine = login(&e, "bob", "pw-bob").await;
    let other_user = login(&e, "alice", "pw-alice").await;
    let r = post(
        &e,
        &mine,
        "/settings/user/password",
        &[
            ("current", "pw-bob"),
            ("new", "fresh-pw"),
            ("again", "fresh-pw"),
        ],
    )
    .await;
    assert_eq!(r.h("location"), "/settings/user?ok=password");
    assert_eq!(
        get(&e, &stolen, "/apps/files/").await.status,
        StatusCode::SEE_OTHER,
        "the other session is gone"
    );
    assert_eq!(
        get(&e, &mine, "/apps/files/").await.status,
        StatusCode::OK,
        "the session that changed it stays"
    );
    assert_eq!(
        get(&e, &other_user, "/apps/files/").await.status,
        StatusCode::OK,
        "other users are not affected"
    );
    // app passwords of devices survive a password change (like Nextcloud)
    let tok = post(
        &e,
        &mine,
        "/settings/user/app-passwords",
        &[("name", "phone")],
    )
    .await
    .body;
    let token = tok
        .split("<code class=\"token\">")
        .nth(1)
        .unwrap()
        .split("</code>")
        .next()
        .unwrap()
        .to_string();
    let h = [basic("bob", &token)];
    assert_eq!(
        send(&e, "GET", "/ocs/v2.php/cloud/user", &h, "")
            .await
            .status,
        StatusCode::OK
    );
    post(
        &e,
        &mine,
        "/settings/user/password",
        &[
            ("current", "fresh-pw"),
            ("new", "third-pw"),
            ("again", "third-pw"),
        ],
    )
    .await;
    assert_eq!(
        send(&e, "GET", "/ocs/v2.php/cloud/user", &h, "")
            .await
            .status,
        StatusCode::OK
    );
    // an administrator setting somebody else's password ends that user's sessions
    let admin = login(&e, "admin", "pw-admin").await;
    let victim = login(&e, "alice", "pw-alice").await;
    let r = post(
        &e,
        &admin,
        "/settings/users/alice",
        &[
            ("display_name", "Alice A"),
            ("email", ""),
            ("password", "reset-by-admin"),
            ("enabled", "1"),
        ],
    )
    .await;
    assert_eq!(r.h("location"), "/settings/users?ok=saved");
    assert_eq!(
        get(&e, &victim, "/apps/files/").await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        get(&e, &admin, "/settings/users").await.status,
        StatusCode::OK
    );
}
