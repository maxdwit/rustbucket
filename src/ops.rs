//! Operations: security headers on every response, request metrics and the optional `/metrics` endpoint.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::routes::AppState;

/// Counters shared by all requests (cheap atomics, no locks on the hot path).
pub struct Metrics {
    started: Instant,
    by_class: [AtomicU64; 6], // 1xx..5xx and "other"
    in_flight: AtomicI64,
    duration_micros: AtomicU64,
    requests: AtomicU64,
    rejected: AtomicU64, // 401 / 403 / 429
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            by_class: Default::default(),
            in_flight: AtomicI64::new(0),
            duration_micros: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }
    }
}

/// Keeps the in-flight gauge right even when the client goes away and the request future is dropped
/// (aborted uploads/downloads): without the guard each abort would leak one count.
struct InFlight {
    m: Arc<Metrics>,
    started: Instant,
    done: bool,
}

impl InFlight {
    fn new(m: Arc<Metrics>) -> Self {
        m.in_flight.fetch_add(1, Ordering::Relaxed);
        Self {
            m,
            started: Instant::now(),
            done: false,
        }
    }

    fn finish(mut self, status: StatusCode) {
        self.m.record(status, self.started.elapsed());
        self.done = true;
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.m.in_flight.fetch_sub(1, Ordering::Relaxed);
        if !self.done {
            // cancelled before a response existed: count it under "other" so the totals still add up
            self.m.by_class[5].fetch_add(1, Ordering::Relaxed);
            self.m.requests.fetch_add(1, Ordering::Relaxed);
            self.m
                .duration_micros
                .fetch_add(self.started.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
    }
}

impl Metrics {
    /// Seconds since the server started.
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    fn record(&self, status: StatusCode, took: std::time::Duration) {
        let class = match status.as_u16() / 100 {
            c @ 1..=5 => (c - 1) as usize,
            _ => 5,
        };
        self.by_class[class].fetch_add(1, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.duration_micros
            .fetch_add(took.as_micros() as u64, Ordering::Relaxed);
        if matches!(status.as_u16(), 401 | 403 | 429) {
            self.rejected.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Prometheus text format of the counters (the caller appends database gauges).
    pub fn render(&self) -> String {
        let mut s = String::new();
        let line = |s: &mut String, help: &str, ty: &str, name: &str, labels: &str, v: String| {
            if !help.is_empty() {
                s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {ty}\n"));
            }
            s.push_str(&format!("{name}{labels} {v}\n"));
        };
        for (i, class) in ["1xx", "2xx", "3xx", "4xx", "5xx", "other"]
            .iter()
            .enumerate()
        {
            line(
                &mut s,
                if i == 0 {
                    "HTTP responses by status class."
                } else {
                    ""
                },
                "counter",
                "rustbucket_http_responses_total",
                &format!("{{class=\"{class}\"}}"),
                self.by_class[i].load(Ordering::Relaxed).to_string(),
            );
        }
        line(
            &mut s,
            "Requests currently being served.",
            "gauge",
            "rustbucket_http_requests_in_flight",
            "",
            self.in_flight.load(Ordering::Relaxed).to_string(),
        );
        line(
            &mut s,
            "Total time spent serving requests.",
            "counter",
            "rustbucket_http_request_duration_seconds_sum",
            "",
            format!(
                "{:.6}",
                self.duration_micros.load(Ordering::Relaxed) as f64 / 1e6
            ),
        );
        line(
            &mut s,
            "Requests served.",
            "counter",
            "rustbucket_http_request_duration_seconds_count",
            "",
            self.requests.load(Ordering::Relaxed).to_string(),
        );
        line(
            &mut s,
            "Responses 401/403/429 (failed logins, CSRF, throttling).",
            "counter",
            "rustbucket_http_rejected_total",
            "",
            self.rejected.load(Ordering::Relaxed).to_string(),
        );
        line(
            &mut s,
            "Seconds since start.",
            "gauge",
            "rustbucket_uptime_seconds",
            "",
            self.started.elapsed().as_secs().to_string(),
        );
        let b = crate::buildinfo::get();
        line(
            &mut s,
            "Build information.",
            "gauge",
            "rustbucket_build_info",
            &format!(
                "{{version=\"{}\",build_date=\"{}\",commit=\"{}\",build_id=\"{}\"}}",
                b.version, b.build_date, b.commit, b.hash
            ),
            "1".into(),
        );
        s
    }
}

fn set_if_absent(h: &mut HeaderMap, name: HeaderName, value: &'static str) {
    if !h.contains_key(&name) {
        h.insert(name, HeaderValue::from_static(value));
    }
}

/// Middleware: security headers on every response, plus the request counters.
pub async fn layer(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let dav = req.uri().path().starts_with("/remote.php/");
    let https = st
        .config
        .base_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"));
    let guard = InFlight::new(st.metrics.clone());
    let mut resp = next.run(req).await;
    guard.finish(resp.status());
    let h = resp.headers_mut();
    set_if_absent(h, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set_if_absent(h, header::REFERRER_POLICY, "no-referrer");
    set_if_absent(h, header::X_FRAME_OPTIONS, "SAMEORIGIN");
    set_if_absent(
        h,
        HeaderName::from_static("x-robots-tag"),
        "noindex, nofollow",
    );
    set_if_absent(
        h,
        HeaderName::from_static("x-permitted-cross-domain-policies"),
        "none",
    );
    if dav {
        // anything served from the DAV tree (user uploads!) must never run as a page of this site
        set_if_absent(
            h,
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        );
    }
    if https {
        set_if_absent(
            h,
            header::STRICT_TRANSPORT_SECURITY,
            "max-age=15552000; includeSubDomains",
        );
    }
    resp
}

/// `GET /metrics` with `Authorization: Bearer <RUSTBUCKET_METRICS_TOKEN>`; 404 while no token is configured.
pub async fn metrics(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(token) = st.config.metrics_token.as_deref().filter(|t| !t.is_empty()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !crate::login::ct_eq(given, token) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized",
        )
            .into_response();
    }
    let mut body = st.metrics.render();
    let count = |sql: &'static str| {
        let pool = st.pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(&pool)
                .await
                .unwrap_or(-1)
        }
    };
    for (name, help, v) in [
        (
            "rustbucket_users",
            "Accounts.",
            count("SELECT COUNT(*) FROM users").await,
        ),
        (
            "rustbucket_files",
            "Indexed files and folders.",
            count("SELECT COUNT(*) FROM file_cache WHERE path <> ''").await,
        ),
        (
            "rustbucket_shares",
            "Shares.",
            count("SELECT COUNT(*) FROM shares").await,
        ),
        (
            "rustbucket_trash_items",
            "Items in all trash bins.",
            count("SELECT COUNT(*) FROM trash_items").await,
        ),
        (
            "rustbucket_web_sessions",
            "Browser sessions.",
            count("SELECT COUNT(*) FROM web_sessions").await,
        ),
        (
            "rustbucket_app_passwords",
            "App passwords (connected devices).",
            count("SELECT COUNT(*) FROM app_passwords").await,
        ),
    ] {
        body.push_str(&format!(
            "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}\n"
        ));
    }
    body.push_str(&format!(
        "# HELP rustbucket_data_free_bytes Free space on the data volume.\n# TYPE rustbucket_data_free_bytes gauge\nrustbucket_data_free_bytes {}\n# HELP rustbucket_db_pool_connections Open database connections.\n# TYPE rustbucket_db_pool_connections gauge\nrustbucket_db_pool_connections {}\n",
        st.store.free_bytes(),
        st.pool.size()
    ));
    (
        [
            (
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

pub fn shared() -> Arc<Metrics> {
    Arc::new(Metrics::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_request_does_not_leak_the_in_flight_gauge() {
        let m = shared();
        let a = InFlight::new(m.clone());
        let b = InFlight::new(m.clone());
        assert_eq!(m.in_flight.load(Ordering::Relaxed), 2);
        a.finish(StatusCode::OK);
        drop(b); // the client went away mid-request
        assert_eq!(m.in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(m.requests.load(Ordering::Relaxed), 2);
        let text = m.render();
        assert!(text.contains("rustbucket_http_responses_total{class=\"2xx\"} 1"));
        assert!(text.contains("rustbucket_http_responses_total{class=\"other\"} 1"));
        assert!(text.contains("rustbucket_http_requests_in_flight 0"));
    }
}
