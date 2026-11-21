//! Browser-session authentication of the API (`AuthUser`): the web UI calls DAV/OCS with its session
//! cookie plus a CSRF header. Basic auth stays strictly separate.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
use rustbucket::auth::{csrf_token_for, sha256_hex};
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
    db::create_user(&pool, "alice", "Alice", "pw", false)
        .await
        .unwrap();
    db::create_user(&pool, "eve", "Eve", "pw2", false)
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

async fn session(e: &Env, uid: &str, sid: &str, ttl: i64) {
    let id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_one(&e.pool)
        .await
        .unwrap();
    let now = rustbucket::auth::unix_now();
    sqlx::query("INSERT INTO web_sessions (id_hash, user_id, created_at, expires_at) VALUES ($1, $2, $3, $4)")
        .bind(sha256_hex(sid)).bind(id).bind(now).bind(now + ttl).execute(&e.pool).await.unwrap();
}

struct R {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
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

fn cookie(sid: &str) -> (&'static str, String) {
    ("cookie", format!("rb_session={sid}"))
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
fn csrf(sid: &str) -> (&'static str, String) {
    ("x-csrf-token", csrf_token_for(sid))
}

#[tokio::test]
async fn session_cookie_authenticates_reads_without_a_token() {
    let e = env().await;
    session(&e, "alice", "sid-alice", 3600).await;
    for uri in [
        "/ocs/v2.php/cloud/user?format=json",
        "/remote.php/dav/files/alice/",
    ] {
        let m = if uri.starts_with("/remote") {
            "PROPFIND"
        } else {
            "GET"
        };
        let r = send(
            &e,
            m,
            uri,
            &[cookie("sid-alice"), ("depth", "0".into())],
            "",
        )
        .await;
        // PROPFIND is not a "safe" method here: it needs the token like every write
        assert_eq!(
            r.status,
            if m == "GET" {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            },
            "{m} {uri}"
        );
    }
    let r = send(
        &e,
        "GET",
        "/ocs/v2.php/cloud/user?format=json",
        &[cookie("sid-alice")],
        "",
    )
    .await;
    assert!(r.body.contains("\"id\":\"alice\""), "{}", r.body);
    assert_eq!(
        send(
            &e,
            "HEAD",
            "/remote.php/dav/files/alice/",
            &[cookie("sid-alice")],
            ""
        )
        .await
        .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn every_unsafe_method_needs_the_csrf_token_of_that_session() {
    let e = env().await;
    session(&e, "alice", "sid-alice", 3600).await;
    session(&e, "eve", "sid-eve", 3600).await;
    let uri = "/remote.php/dav/files/alice/f.txt";
    for m in [
        "PUT",
        "MKCOL",
        "DELETE",
        "MOVE",
        "COPY",
        "PROPFIND",
        "LOCK",
        "PROPPATCH",
        "POST",
    ] {
        // no token, a made-up token, and the token of another session are all refused
        for bad in [
            None,
            Some("nope".to_string()),
            Some(csrf_token_for("sid-eve")),
        ] {
            let mut h = vec![
                cookie("sid-alice"),
                (
                    "destination",
                    "https://x/remote.php/dav/files/alice/g.txt".into(),
                ),
            ];
            if let Some(t) = bad {
                h.push(("x-csrf-token", t));
            }
            let r = send(&e, m, uri, &h, "x").await;
            assert_eq!(r.status, StatusCode::FORBIDDEN, "{m}: {}", r.body);
            assert!(
                r.body.contains("Forbidden"),
                "{m}: sabre error expected: {}",
                r.body
            );
        }
    }
    // with the right token the very same requests work
    let h = |extra: Vec<(&'static str, String)>| {
        let mut v = vec![cookie("sid-alice"), csrf("sid-alice")];
        v.extend(extra);
        v
    };
    assert_eq!(
        send(&e, "PUT", uri, &h(vec![]), "hello").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        send(&e, "MKCOL", "/remote.php/dav/files/alice/d", &h(vec![]), "")
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        send(
            &e,
            "MOVE",
            uri,
            &h(vec![(
                "destination",
                "https://x/remote.php/dav/files/alice/d/g.txt".into()
            )]),
            ""
        )
        .await
        .status,
        StatusCode::CREATED
    );
    assert_eq!(
        send(
            &e,
            "GET",
            "/remote.php/dav/files/alice/d/g.txt",
            &[cookie("sid-alice")],
            ""
        )
        .await
        .body,
        "hello"
    );
    assert_eq!(
        send(
            &e,
            "DELETE",
            "/remote.php/dav/files/alice/d",
            &h(vec![]),
            ""
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    // the session of another user cannot reach max's tree
    let r = send(&e, "PUT", uri, &[cookie("sid-eve"), csrf("sid-eve")], "x").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn basic_auth_is_separate_from_the_cookie() {
    let e = env().await;
    session(&e, "alice", "sid-alice", 3600).await;
    // a wrong Basic header never falls back to a valid cookie
    let r = send(
        &e,
        "GET",
        "/ocs/v2.php/cloud/user",
        &[basic("alice", "wrong"), cookie("sid-alice")],
        "",
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // a correct Basic header works with or without a cookie, and needs no CSRF token for writes
    let r = send(
        &e,
        "PUT",
        "/remote.php/dav/files/alice/b.txt",
        &[basic("alice", "pw"), cookie("sid-alice")],
        "x",
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    // credential endpoints refuse cookie auth entirely (a GET that mints an app password)
    for p in [
        "/ocs/v2.php/core/getapppassword",
        "/ocs/v1.php/core/getapppassword",
    ] {
        assert_eq!(
            send(&e, "GET", p, &[cookie("sid-alice")], "").await.status,
            StatusCode::UNAUTHORIZED,
            "{p}"
        );
    }
    assert_eq!(
        send(
            &e,
            "DELETE",
            "/ocs/v2.php/core/apppassword",
            &[cookie("sid-alice"), csrf("sid-alice")],
            ""
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn dead_sessions_and_the_browser_login_dialog() {
    let e = env().await;
    session(&e, "alice", "sid-old", -10).await; // expired
    session(&e, "eve", "sid-eve", 3600).await;
    // expired / unknown / forged cookies: 401
    for sid in ["sid-old", "unknown", ""] {
        assert_eq!(
            send(&e, "GET", "/ocs/v2.php/cloud/user", &[cookie(sid)], "")
                .await
                .status,
            StatusCode::UNAUTHORIZED,
            "{sid:?}"
        );
    }
    // a disabled user's session stops working at once
    sqlx::query("UPDATE users SET enabled = 0 WHERE uid = 'eve'")
        .execute(&e.pool)
        .await
        .unwrap();
    assert_eq!(
        send(
            &e,
            "GET",
            "/ocs/v2.php/cloud/user",
            &[cookie("sid-eve")],
            ""
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    // ... and so do their password and app passwords
    assert_eq!(
        send(
            &e,
            "GET",
            "/ocs/v2.php/cloud/user",
            &[basic("eve", "pw2")],
            ""
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    // plain clients get the Basic challenge, the web UI's fetch/XHR must not (native dialog)
    let plain = send(&e, "GET", "/ocs/v2.php/cloud/user", &[], "").await;
    assert!(plain.headers.get("www-authenticate").is_some());
    for h in [
        ("x-requested-with", "XMLHttpRequest".to_string()),
        ("x-csrf-token", "x".to_string()),
    ] {
        let r = send(&e, "GET", "/remote.php/dav/files/alice/", &[h], "").await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        assert!(r.headers.get("www-authenticate").is_none());
    }
}

#[tokio::test]
async fn a_successful_login_does_not_buy_unlimited_guesses() {
    // somebody with one valid account interleaves a success after every few wrong guesses: the short window
    // never fills, but the long-window counter (not cleared by successes) must still block the address
    let e = env().await;
    let mut blocked_at = None;
    for round in 0..12 {
        for _ in 0..4 {
            let r = send(
                &e,
                "GET",
                "/ocs/v2.php/cloud/user",
                &[basic("eve", "wrong-guess")],
                "",
            )
            .await;
            if r.status == StatusCode::TOO_MANY_REQUESTS {
                blocked_at.get_or_insert(round);
            }
        }
        let ok = send(
            &e,
            "GET",
            "/ocs/v2.php/cloud/user",
            &[basic("alice", "pw")],
            "",
        )
        .await;
        if ok.status == StatusCode::TOO_MANY_REQUESTS {
            blocked_at.get_or_insert(round);
        }
    }
    let round = blocked_at.expect("the long-window limit never kicked in");
    assert!(
        (6..=8).contains(&round),
        "30 failures = about 8 rounds of 4, blocked in round {round}"
    );
    // now even the valid login from this address is refused, with a Retry-After
    let r = send(
        &e,
        "GET",
        "/ocs/v2.php/cloud/user",
        &[basic("alice", "pw")],
        "",
    )
    .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(r.headers.get("retry-after").is_some());
}
