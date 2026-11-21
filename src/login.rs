//! Login Flow v2 (init / poll / browser login + grant pages) and browser sessions.

use askama::Template;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::auth::{self, ClientIp, Peer, sha256_hex, unix_now};
use crate::error::AppError;
use crate::proxy;
use crate::routes::AppState;

const FLOW_TTL_SECS: i64 = 20 * 60;
pub(crate) const SESSION_TTL_SECS: i64 = 24 * 60 * 60;
pub(crate) const SESSION_COOKIE: &str = "rb_session";
const CSRF_COOKIE: &str = "rb_csrf";
const FLOW_TOKEN_LEN: usize = 128;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/index.php/login/v2", post(init))
        .route("/index.php/login/v2/poll", post(poll))
        .route(
            "/index.php/login/v2/flow/{token}",
            get(flow_page).post(flow_login),
        )
        .route("/index.php/login/v2/flow/{token}/grant", post(flow_grant))
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    device: String,
    action: String,
    csrf: String,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "grant.html")]
struct GrantPage {
    device: String,
    user: String,
    action: String,
    csrf: String,
}

#[derive(Template)]
#[template(path = "done.html")]
struct DonePage {
    device: String,
    user: String,
}

#[derive(Template)]
#[template(path = "notfound.html")]
struct NotFoundPage;

fn page(
    status: StatusCode,
    t: impl Template,
    extra: Vec<HeaderValue>,
) -> Result<Response, AppError> {
    let body = t.render().map_err(anyhow::Error::from)?;
    let mut r = (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    for v in extra {
        h.append(header::SET_COOKIE, v);
    }
    Ok(r)
}

pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

pub(crate) fn set_cookie(name: &str, value: &str, max_age: i64, secure: bool) -> HeaderValue {
    let sec = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax{sec}"
    ))
    .expect("cookie header")
}

pub(crate) fn is_secure(state: &AppState, headers: &HeaderMap, ip: std::net::IpAddr) -> bool {
    proxy::external_base_url(&state.config, ip, headers).starts_with("https://")
}

pub(crate) fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// CSRF: double-submit cookie (SameSite=Lax) compared with the hidden form field.
pub(crate) fn csrf_ok(headers: &HeaderMap, form_value: &str) -> bool {
    matches!(cookie_value(headers, CSRF_COOKIE), Some(c) if !c.is_empty() && ct_eq(&c, form_value))
}

struct Flow {
    id: i64,
    device_name: String,
    user_id: Option<i64>,
}

async fn find_flow(state: &AppState, token: &str) -> Result<Option<Flow>, AppError> {
    if token.len() != FLOW_TOKEN_LEN {
        return Ok(None);
    }
    let row: Option<(i64, String, Option<i64>)> = sqlx::query_as(
        "SELECT id, device_name, user_id FROM login_flow_tokens WHERE flow_hash = $1 AND expires_at > $2",
    )
    .bind(sha256_hex(token))
    .bind(unix_now())
    .fetch_optional(&state.pool)
    .await?;
    Ok(row.map(|(id, device_name, user_id)| Flow {
        id,
        device_name,
        user_id,
    }))
}

async fn session_user(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<(i64, String)>, AppError> {
    let Some(sid) = cookie_value(headers, SESSION_COOKIE) else {
        return Ok(None);
    };
    Ok(sqlx::query_as(
        "SELECT u.id, u.uid FROM web_sessions s JOIN users u ON u.id = s.user_id \
         WHERE s.id_hash = $1 AND s.expires_at > $2 AND u.enabled = 1",
    )
    .bind(sha256_hex(&sid))
    .bind(unix_now())
    .fetch_optional(&state.pool)
    .await?)
}

fn flow_url(token: &str) -> String {
    format!("/index.php/login/v2/flow/{token}")
}

/// `POST /index.php/login/v2` - start a flow; the client opens `login` in the browser and polls.
async fn init(
    State(state): State<AppState>,
    Peer(peer): Peer,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let now = unix_now();
    sqlx::query("DELETE FROM login_flow_tokens WHERE expires_at <= $1")
        .bind(now)
        .execute(&state.pool)
        .await?;
    sqlx::query("DELETE FROM web_sessions WHERE expires_at <= $1")
        .bind(now)
        .execute(&state.pool)
        .await?;
    let device: String = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(200).collect())
        .filter(|s: &String| !s.is_empty())
        .unwrap_or_else(|| "unknown device".into());
    let poll = auth::random_token(FLOW_TOKEN_LEN);
    let flow = auth::random_token(FLOW_TOKEN_LEN);
    sqlx::query("INSERT INTO login_flow_tokens (poll_hash, flow_hash, device_name, expires_at) VALUES ($1, $2, $3, $4)")
        .bind(sha256_hex(&poll))
        .bind(sha256_hex(&flow))
        .bind(&device)
        .bind(now + FLOW_TTL_SECS)
        .execute(&state.pool)
        .await?;
    let base = proxy::external_base_url(&state.config, peer, &headers);
    Ok(Json(json!({
        "poll": {"token": poll, "endpoint": format!("{base}/index.php/login/v2/poll")},
        "login": format!("{base}{}", flow_url(&flow)),
    }))
    .into_response())
}

#[derive(Deserialize)]
struct PollForm {
    token: String,
}

/// `POST /index.php/login/v2/poll` - 404 until the user granted access, then the credentials once.
async fn poll(
    State(state): State<AppState>,
    Peer(peer): Peer,
    headers: HeaderMap,
    Form(f): Form<PollForm>,
) -> Result<Response, AppError> {
    let row: Option<(i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT t.id, u.uid, t.app_password FROM login_flow_tokens t LEFT JOIN users u ON u.id = t.user_id \
         WHERE t.poll_hash = $1 AND t.expires_at > $2",
    )
    .bind(sha256_hex(&f.token))
    .bind(unix_now())
    .fetch_optional(&state.pool)
    .await?;
    let Some((id, Some(uid), Some(app_password))) = row else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    sqlx::query("DELETE FROM login_flow_tokens WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(Json(json!({
        "server": proxy::external_base_url(&state.config, peer, &headers),
        "loginName": uid,
        "appPassword": app_password,
    }))
    .into_response())
}

/// Shows the login form, or - with a valid browser session - the grant page.
async fn flow_page(
    State(state): State<AppState>,
    Peer(peer): Peer,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let Some(flow) = find_flow(&state, &token).await? else {
        return page(StatusCode::NOT_FOUND, NotFoundPage, vec![]);
    };
    let secure = is_secure(&state, &headers, peer);
    let (csrf, cookies) = match cookie_value(&headers, CSRF_COOKIE).filter(|c| !c.is_empty()) {
        Some(c) => (c, vec![]),
        None => {
            let c = auth::random_token(32);
            let h = set_cookie(CSRF_COOKIE, &c, SESSION_TTL_SECS, secure);
            (c, vec![h])
        }
    };
    if flow.user_id.is_some() {
        return page(StatusCode::NOT_FOUND, NotFoundPage, cookies);
    }
    match session_user(&state, &headers).await? {
        Some((_, uid)) => page(
            StatusCode::OK,
            GrantPage {
                device: flow.device_name,
                user: uid,
                action: format!("{}/grant", flow_url(&token)),
                csrf,
            },
            cookies,
        ),
        None => page(
            StatusCode::OK,
            LoginPage {
                device: flow.device_name,
                action: flow_url(&token),
                csrf,
                error: None,
            },
            cookies,
        ),
    }
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    csrf: String,
}

async fn flow_login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Peer(peer): Peer,
    Path(token): Path<String>,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Result<Response, AppError> {
    let Some(flow) = find_flow(&state, &token).await? else {
        return page(StatusCode::NOT_FOUND, NotFoundPage, vec![]);
    };
    if !csrf_ok(&headers, &f.csrf) {
        return Ok((StatusCode::FORBIDDEN, "invalid CSRF token").into_response());
    }
    if let Err(secs) = state.auth_gate(ip) {
        let mut r = (
            StatusCode::TOO_MANY_REQUESTS,
            format!("Too many failed attempts, retry in {secs} s"),
        )
            .into_response();
        if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
            r.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return Ok(r);
    }
    let Some(user) =
        auth::verify_user_password(&state.pool, f.username.trim(), &f.password).await?
    else {
        state.auth_failed(ip);
        return page(
            StatusCode::UNAUTHORIZED,
            LoginPage {
                device: flow.device_name,
                action: flow_url(&token),
                csrf: f.csrf,
                error: Some("Wrong account name or password.".into()),
            },
            vec![],
        );
    };
    state.auth_ok(ip);
    let sid = auth::random_token(48);
    let now = unix_now();
    sqlx::query("INSERT INTO web_sessions (id_hash, user_id, created_at, expires_at) VALUES ($1, $2, $3, $4)")
        .bind(sha256_hex(&sid))
        .bind(user.id)
        .bind(now)
        .bind(now + SESSION_TTL_SECS)
        .execute(&state.pool)
        .await?;
    let secure = is_secure(&state, &headers, peer);
    let mut r = (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, flow_url(&token))],
    )
        .into_response();
    r.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(SESSION_COOKIE, &sid, SESSION_TTL_SECS, secure),
    );
    Ok(r)
}

#[derive(Deserialize)]
struct GrantForm {
    csrf: String,
}

async fn flow_grant(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    Form(f): Form<GrantForm>,
) -> Result<Response, AppError> {
    let Some(flow) = find_flow(&state, &token).await? else {
        return page(StatusCode::NOT_FOUND, NotFoundPage, vec![]);
    };
    if !csrf_ok(&headers, &f.csrf) {
        return Ok((StatusCode::FORBIDDEN, "invalid CSRF token").into_response());
    }
    let Some((user_id, uid)) = session_user(&state, &headers).await? else {
        return Ok((
            StatusCode::SEE_OTHER,
            [(header::LOCATION, flow_url(&token))],
        )
            .into_response());
    };
    if flow.user_id.is_some() {
        return page(StatusCode::NOT_FOUND, NotFoundPage, vec![]); // already granted (double submit)
    }
    let app_password = auth::create_app_password(&state.pool, user_id, &flow.device_name).await?;
    sqlx::query("UPDATE login_flow_tokens SET user_id = $1, app_password = $2 WHERE id = $3 AND user_id IS NULL")
        .bind(user_id)
        .bind(&app_password)
        .bind(flow.id)
        .execute(&state.pool)
        .await?;
    page(
        StatusCode::OK,
        DonePage {
            device: flow.device_name,
            user: uid,
        },
        vec![],
    )
}
