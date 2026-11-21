//! End-to-end tests of Phase 2: Login Flow v2, app passwords, Basic auth, throttle.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use http_body_util::BodyExt;
use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::routes::{AppState, router};
use serde_json::Value;
use tower::ServiceExt;

async fn app() -> (tempfile::TempDir, Router, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("t.db").display());
    let pool = db::connect(&url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    db::create_user(&pool, "alice", "Alice A.", "s3cret", false)
        .await
        .unwrap();
    db::create_group(&pool, "team").await.unwrap();
    db::add_member(&pool, "team", "alice").await.unwrap();
    let cfg = Config {
        base_url: Some("https://rb.test".into()),
        ..Config::default()
    };
    let app = router(AppState::new(pool.clone(), Arc::new(cfg)));
    (dir, app, pool)
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}
impl Resp {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not json: {}", self.text()))
    }
    fn cookies(&self) -> Vec<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
            .collect()
    }
}

async fn send(app: &Router, req: Request<Body>) -> Resp {
    let r = app.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let headers = r.headers().clone();
    let body = r.into_body().collect().await.unwrap().to_bytes().to_vec();
    Resp {
        status,
        headers,
        body,
    }
}

fn basic(user: &str, pw: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pw}"))
    )
}

fn form(pairs: &[(&str, &str)]) -> Body {
    Body::from(
        pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&"),
    )
}

fn post(uri: &str, cookie: &str, pairs: &[(&str, &str)]) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, cookie)
        .body(form(pairs))
        .unwrap()
}

async fn init_flow(app: &Router) -> (String, String) {
    let r = send(
        app,
        Request::post("/index.php/login/v2")
            .header(header::USER_AGENT, "Test-Client/1.0")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let j = r.json();
    assert_eq!(
        j["poll"]["endpoint"],
        "https://rb.test/index.php/login/v2/poll"
    );
    let login = j["login"].as_str().unwrap();
    assert!(login.starts_with("https://rb.test/index.php/login/v2/flow/"));
    (
        j["poll"]["token"].as_str().unwrap().into(),
        login.trim_start_matches("https://rb.test").into(),
    )
}

fn field<'a>(html: &'a str, name: &str) -> &'a str {
    let key = format!("name=\"{name}\" value=\"");
    let i = html
        .find(&key)
        .unwrap_or_else(|| panic!("no field {name} in {html}"))
        + key.len();
    &html[i..i + html[i..].find('"').unwrap()]
}

#[tokio::test]
async fn full_login_flow_then_revoke() {
    let (_d, app, _pool) = app().await;
    let (poll_token, flow_path) = init_flow(&app).await;

    // poll before grant: 404
    let poll = || post("/index.php/login/v2/poll", "", &[("token", &poll_token)]);
    assert_eq!(send(&app, poll()).await.status, StatusCode::NOT_FOUND);

    // login page, with CSRF cookie + hidden field
    let page = send(&app, Request::get(&flow_path).body(Body::empty()).unwrap()).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text().contains("Test-Client/1.0") && page.text().contains("name=\"password\""));
    assert!(page.headers.get(header::CONTENT_SECURITY_POLICY).is_some());
    let csrf_cookie = page
        .cookies()
        .into_iter()
        .find(|c| c.starts_with("rb_csrf="))
        .unwrap();
    let csrf = field(&page.text(), "csrf").to_string();
    assert_eq!(csrf_cookie, format!("rb_csrf={csrf}"));

    // CSRF mismatch is rejected
    let bad = send(
        &app,
        post(
            &flow_path,
            &csrf_cookie,
            &[
                ("username", "alice"),
                ("password", "s3cret"),
                ("csrf", "nope"),
            ],
        ),
    )
    .await;
    assert_eq!(bad.status, StatusCode::FORBIDDEN);
    // wrong password: 401 page, no session
    let wrong = send(
        &app,
        post(
            &flow_path,
            &csrf_cookie,
            &[
                ("username", "alice"),
                ("password", "wrong"),
                ("csrf", &csrf),
            ],
        ),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.text().contains("Wrong account name or password"));
    // right password: redirect + session cookie
    let ok = send(
        &app,
        post(
            &flow_path,
            &csrf_cookie,
            &[
                ("username", "alice"),
                ("password", "s3cret"),
                ("csrf", &csrf),
            ],
        ),
    )
    .await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    let session = ok
        .cookies()
        .into_iter()
        .find(|c| c.starts_with("rb_session="))
        .unwrap();
    let cookies = format!("{csrf_cookie}; {session}");
    assert!(
        ok.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("HttpOnly")
    );

    // grant page, then grant
    let grant = send(
        &app,
        Request::get(&flow_path)
            .header(header::COOKIE, &cookies)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        grant.text().contains("Grant access") && grant.text().contains("Alice")
            || grant.text().contains("alice")
    );
    let done = send(
        &app,
        post(&format!("{flow_path}/grant"), &cookies, &[("csrf", &csrf)]),
    )
    .await;
    assert_eq!(done.status, StatusCode::OK);
    assert!(done.text().contains("Account connected"));

    // poll delivers the credentials exactly once
    let creds = send(&app, poll()).await;
    assert_eq!(creds.status, StatusCode::OK);
    let c = creds.json();
    assert_eq!(c["server"], "https://rb.test");
    assert_eq!(c["loginName"], "alice");
    let app_pw = c["appPassword"].as_str().unwrap().to_string();
    assert_eq!(app_pw.len(), 72);
    assert_eq!(send(&app, poll()).await.status, StatusCode::NOT_FOUND);

    // app password works for the OCS user endpoint
    let me = send(
        &app,
        Request::get("/ocs/v2.php/cloud/user?format=json")
            .header(header::AUTHORIZATION, basic("alice", &app_pw))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(me.status, StatusCode::OK);
    let d = &me.json()["ocs"]["data"];
    assert_eq!(d["id"], "alice");
    assert_eq!(d["display-name"], "Alice A.");
    assert_eq!(d["groups"][0], "team");

    // avatar needs auth, is a PNG
    let av = send(
        &app,
        Request::get("/index.php/avatar/alice/64")
            .header(header::AUTHORIZATION, basic("alice", &app_pw))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(av.status, StatusCode::OK);
    assert_eq!(&av.body[1..4], b"PNG");
    assert_eq!(
        send(
            &app,
            Request::get("/index.php/avatar/alice/64")
                .body(Body::empty())
                .unwrap()
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );

    // stubs the clients ask for after login
    let nav = send(
        &app,
        Request::get("/ocs/v2.php/core/navigation/apps?format=json&absolute=true")
            .header(header::AUTHORIZATION, basic("alice", &app_pw))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(nav.status, StatusCode::OK);
    assert_eq!(nav.json()["ocs"]["data"], serde_json::json!([]));
    let terms = send(
        &app,
        Request::get("/ocs/v2.php/apps/terms_of_service/terms?format=json")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(terms.status, StatusCode::NOT_FOUND);
    assert_eq!(terms.json()["ocs"]["meta"]["statuscode"], 404);

    // wrong user with a valid token fails
    let other = send(
        &app,
        Request::get("/ocs/v2.php/cloud/user")
            .header(header::AUTHORIZATION, basic("bob", &app_pw))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(other.status, StatusCode::UNAUTHORIZED);

    // revoking the app password logs the client out
    let del = send(
        &app,
        Request::delete("/ocs/v2.php/core/apppassword?format=json")
            .header(header::AUTHORIZATION, basic("alice", &app_pw))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(del.status, StatusCode::OK);
    let after = send(
        &app,
        Request::get("/ocs/v2.php/cloud/user")
            .header(header::AUTHORIZATION, basic("alice", &app_pw))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    assert!(after.headers.get(header::WWW_AUTHENTICATE).is_some());
}

#[tokio::test]
async fn getapppassword_requires_real_password_and_is_revocable() {
    let (_d, app, _pool) = app().await;
    let r = send(
        &app,
        Request::get("/ocs/v2.php/core/getapppassword?format=json")
            .header(header::AUTHORIZATION, basic("alice", "s3cret"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let pw = r.json()["ocs"]["data"]["apppassword"]
        .as_str()
        .unwrap()
        .to_string();
    // using an app password to get another one is refused
    let r2 = send(
        &app,
        Request::get("/ocs/v2.php/core/getapppassword?format=json")
            .header(header::AUTHORIZATION, basic("alice", &pw))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r2.status, StatusCode::FORBIDDEN);
    // deleting with the real password (no app password in use) is a client error
    let r3 = send(
        &app,
        Request::delete("/ocs/v2.php/core/apppassword")
            .header(header::AUTHORIZATION, basic("alice", "s3cret"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r3.status, StatusCode::BAD_REQUEST);
    // secrets are not stored in clear
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM app_passwords WHERE token_hash = $1")
        .bind(&pw)
        .fetch_one(&_pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn bad_basic_auth_is_throttled() {
    let (_d, app, _pool) = app().await;
    let req = |pw: &str| {
        Request::get("/ocs/v2.php/cloud/user")
            .header(header::AUTHORIZATION, basic("alice", pw))
            .body(Body::empty())
            .unwrap()
    };
    for _ in 0..5 {
        assert_eq!(
            send(&app, req("nope")).await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    let blocked = send(&app, req("s3cret")).await; // even the right password is refused now
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(blocked.headers.get(header::RETRY_AFTER).is_some());
}

#[tokio::test]
async fn expired_or_bogus_flow_tokens_404() {
    let (_d, app, pool) = app().await;
    let r = send(
        &app,
        Request::get(format!("/index.php/login/v2/flow/{}", "x".repeat(128)))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let (poll, flow) = init_flow(&app).await;
    sqlx::query("UPDATE login_flow_tokens SET expires_at = 1")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        send(&app, Request::get(&flow).body(Body::empty()).unwrap())
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            post("/index.php/login/v2/poll", "", &[("token", &poll)])
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

/// Regression: without a fixed `base_url`, a trusted proxy's `X-Forwarded-Proto` must decide the scheme of the URLs the
/// login flow hands out, even when the forwarded *client* address is a different (untrusted) one. The trust check has to
/// use the TCP peer (the proxy), not the client address taken from `X-Forwarded-For`.
#[tokio::test]
async fn forwarded_proto_is_honoured_from_a_trusted_peer_only() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("t.db").display());
    let pool = db::connect(&url).await.unwrap();
    db::migrate(&pool).await.unwrap();
    let mk = |trusted: &[&str]| {
        let cfg = Config {
            trusted_proxies: trusted.iter().map(|n| n.parse().unwrap()).collect(),
            ..Config::default()
        };
        router(AppState::new(pool.clone(), Arc::new(cfg)))
    };
    let req = || {
        Request::post("/index.php/login/v2")
            .header(header::HOST, "internal-name:8080")
            .header("x-forwarded-host", "files.example.com:8443")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-for", "203.0.113.9")
            .body(Body::empty())
            .unwrap()
    };
    // in these tests the TCP peer is 127.0.0.1 (no ConnectInfo): trusted -> the forwarded scheme and host are used
    let r = send(&mk(&["127.0.0.1/32"]), req()).await.json();
    assert_eq!(
        r["poll"]["endpoint"],
        "https://files.example.com:8443/index.php/login/v2/poll"
    );
    assert!(
        r["login"]
            .as_str()
            .unwrap()
            .starts_with("https://files.example.com:8443/")
    );
    // peer not trusted: the forwarded headers are ignored (plain Host, http)
    let r = send(&mk(&["198.51.100.0/24"]), req()).await.json();
    assert_eq!(
        r["poll"]["endpoint"],
        "http://internal-name:8080/index.php/login/v2/poll"
    );
}
