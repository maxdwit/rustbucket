//! Phase 9: security headers on every response, the token-protected /metrics endpoint.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::routes::{AppState, router};
use tower::ServiceExt;

async fn app(cfg: impl FnOnce(&mut Config)) -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect(&format!("sqlite://{}", dir.path().join("t.db").display()))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    db::create_user(&pool, "alice", "Alice", "pw", false)
        .await
        .unwrap();
    let mut c = Config {
        data_dir: dir.path().join("data"),
        ..Config::default()
    };
    cfg(&mut c);
    (dir, router(AppState::new(pool, Arc::new(c))))
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
async fn send(app: &Router, method: &str, uri: &str, hdrs: &[(&str, String)], body: &str) -> R {
    let mut b = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri);
    for (k, v) in hdrs {
        b = b.header(*k, v.as_str());
    }
    let r = app
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
fn basic() -> (&'static str, String) {
    (
        "authorization",
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("alice:pw")
        ),
    )
}

#[tokio::test]
async fn every_response_carries_the_security_headers() {
    let (_d, app) = app(|_| {}).await;
    send(
        &app,
        "PUT",
        "/remote.php/dav/files/alice/page.html",
        &[basic()],
        "<script>alert(1)</script>",
    )
    .await;
    let cases = [
        send(&app, "GET", "/status.php", &[], "").await,
        send(&app, "GET", "/login", &[], "").await,
        send(&app, "GET", "/nope", &[], "").await,
        send(&app, "GET", "/ocs/v2.php/cloud/user", &[], "").await, // 401
        send(
            &app,
            "PROPFIND",
            "/remote.php/dav/files/alice/",
            &[basic(), ("depth", "0".into())],
            "",
        )
        .await,
        send(
            &app,
            "GET",
            "/remote.php/dav/files/alice/page.html",
            &[basic()],
            "",
        )
        .await,
    ];
    for r in &cases {
        assert_eq!(r.h("x-content-type-options"), "nosniff");
        assert_eq!(r.h("referrer-policy"), "no-referrer");
        assert!(!r.h("x-frame-options").is_empty());
        assert_eq!(r.h("x-robots-tag"), "noindex, nofollow");
        assert_eq!(r.h("x-permitted-cross-domain-policies"), "none");
        assert!(
            r.h("strict-transport-security").is_empty(),
            "no HSTS unless the public URL is https"
        );
    }
    // user uploads can never run as part of this site: attachment + a CSP that forbids everything
    let dl = &cases[5];
    assert_eq!(dl.status, StatusCode::OK);
    assert!(dl.h("content-disposition").starts_with("attachment"));
    assert_eq!(
        dl.h("content-security-policy"),
        "default-src 'none'; sandbox"
    );
    assert!(
        cases[4]
            .h("content-security-policy")
            .contains("default-src 'none'")
    );
    // pages keep their own, more specific policy (the middleware only fills gaps)
    let login = &cases[1];
    assert!(
        login
            .h("content-security-policy")
            .contains("form-action 'self'"),
        "{}",
        login.h("content-security-policy")
    );
    assert_eq!(login.h("x-frame-options"), "DENY");
}

#[tokio::test]
async fn hsts_only_behind_an_https_public_url() {
    let (_d, app) = app(|c| c.base_url = Some("https://cloud.example".into())).await;
    let r = send(&app, "GET", "/status.php", &[], "").await;
    assert_eq!(
        r.h("strict-transport-security"),
        "max-age=15552000; includeSubDomains"
    );
}

#[tokio::test]
async fn metrics_need_a_configured_token() {
    // not configured: the endpoint does not exist
    let (_d, off) = app(|_| {}).await;
    assert_eq!(
        send(&off, "GET", "/metrics", &[], "").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &off,
            "GET",
            "/metrics",
            &[("authorization", "Bearer anything".into())],
            ""
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    let (_d2, app) = app(|c| c.metrics_token = Some("s3cret-token".into())).await;
    assert_eq!(
        send(&app, "GET", "/metrics", &[], "").await.status,
        StatusCode::UNAUTHORIZED
    );
    for bad in [
        "Bearer nope",
        "Bearer s3cret-toke",
        "s3cret-token",
        "Basic czNjcmV0LXRva2Vu",
    ] {
        assert_eq!(
            send(
                &app,
                "GET",
                "/metrics",
                &[("authorization", bad.into())],
                ""
            )
            .await
            .status,
            StatusCode::UNAUTHORIZED,
            "{bad}"
        );
    }
    // traffic shows up in the counters; the body is Prometheus text
    send(&app, "GET", "/status.php", &[], "").await;
    send(&app, "GET", "/ocs/v2.php/cloud/user", &[], "").await; // 401
    send(&app, "GET", "/does-not-exist", &[], "").await; // 404
    let ok = send(
        &app,
        "GET",
        "/metrics",
        &[("authorization", "Bearer s3cret-token".into())],
        "",
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK);
    assert!(
        ok.h("content-type")
            .starts_with("text/plain; version=0.0.4")
            && ok.h("cache-control") == "no-store"
    );
    let get = |name: &str| -> f64 {
        ok.body
            .lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("no {name} in\n{}", ok.body))
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    assert!(get("rustbucket_http_responses_total{class=\"2xx\"}") >= 1.0);
    assert!(
        get("rustbucket_http_responses_total{class=\"4xx\"}") >= 5.0,
        "the 401s and the 404"
    );
    assert!(get("rustbucket_http_rejected_total") >= 5.0);
    assert_eq!(get("rustbucket_users"), 1.0);
    assert!(get("rustbucket_http_request_duration_seconds_count") >= 7.0);
    assert!(
        ok.body
            .contains("# TYPE rustbucket_http_responses_total counter")
            && ok.body.contains("rustbucket_build_info{version=\"")
    );
    assert!(
        ok.body.contains("rustbucket_data_free_bytes")
            && ok.body.contains("rustbucket_trash_items 0")
    );
    // the metrics response itself has the headers too
    assert_eq!(ok.h("x-content-type-options"), "nosniff");
}
