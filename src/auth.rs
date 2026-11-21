//! Authentication: tokens, app passwords, Basic auth, and the `AuthUser` / `ClientIp` extractors.

use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::password_hash::PasswordVerifier;
use argon2::password_hash::phc::PasswordHash;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::ocs::{self, Version};
use crate::proxy;
use crate::routes::AppState;

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const ALPHABET: &[u8; 62] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Random alphanumeric token from the OS CSPRNG (rejection sampling, no modulo bias).
pub fn random_token(len: usize) -> String {
    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 64];
    while out.len() < len {
        getrandom::fill(&mut buf).expect("OS randomness unavailable");
        for &b in &buf {
            if b < 248 && out.len() < len {
                out.push(ALPHABET[(b % 62) as usize] as char);
            }
        }
    }
    out
}

/// Length of generated app passwords (same as Nextcloud).
pub const APP_PASSWORD_LEN: usize = 72;

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub uid: String,
    pub display_name: String,
    pub is_admin: bool,
    /// Set when the request authenticated with an app password.
    pub app_password_id: Option<i64>,
}

#[derive(Debug)]
pub enum AuthError {
    Unauthorized,
    RateLimited(u64),
}

/// At most a few password hashes at a time: argon2 is deliberately expensive, and a flood of login
/// attempts must not be able to starve the file transfers of CPU and memory.
fn hash_slots() -> &'static tokio::sync::Semaphore {
    static S: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    S.get_or_init(|| tokio::sync::Semaphore::new(4))
}

/// Compute the dummy hash now (at startup), not on the first login of an unknown user.
pub fn warm_up() {
    let _ = dummy_hash();
}

fn dummy_hash() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| crate::db::hash_password("dummy-password-for-timing").expect("hash"))
}

/// Verify a user's real password (argon2, off the async threads). Unknown users cost the same.
pub async fn verify_user_password(
    pool: &sqlx::SqlitePool,
    uid: &str,
    password: &str,
) -> anyhow::Result<Option<AuthUser>> {
    let row: Option<(i64, String, String, bool, String)> = sqlx::query_as(
        "SELECT id, uid, display_name, is_admin, password_hash FROM users WHERE uid = $1 AND enabled = 1",
    )
    .bind(uid)
    .fetch_optional(pool)
    .await?;
    let hash = row
        .as_ref()
        .map(|r| r.4.clone())
        .unwrap_or_else(|| dummy_hash().to_string());
    let pw = password.to_string();
    let _slot = hash_slots().acquire().await?;
    let ok = tokio::task::spawn_blocking(move || match PasswordHash::new(&hash) {
        Ok(h) => argon2::Argon2::default()
            .verify_password(pw.as_bytes(), &h)
            .is_ok(),
        Err(_) => false,
    })
    .await?;
    Ok(match (ok, row) {
        (true, Some((id, uid, display_name, is_admin, _))) => Some(AuthUser {
            id,
            uid,
            display_name,
            is_admin,
            app_password_id: None,
        }),
        _ => None,
    })
}

pub async fn create_app_password(
    pool: &sqlx::SqlitePool,
    user_id: i64,
    name: &str,
) -> anyhow::Result<String> {
    let token = random_token(APP_PASSWORD_LEN);
    sqlx::query(
        "INSERT INTO app_passwords (user_id, name, token_hash, created_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(user_id)
    .bind(name)
    .bind(sha256_hex(&token))
    .bind(unix_now())
    .execute(pool)
    .await?;
    Ok(token)
}

async fn verify_app_password(
    pool: &sqlx::SqlitePool,
    uid: &str,
    token: &str,
) -> anyhow::Result<Option<AuthUser>> {
    let row: Option<(i64, i64, String, String, bool)> = sqlx::query_as(
        "SELECT ap.id, u.id, u.uid, u.display_name, u.is_admin FROM app_passwords ap \
         JOIN users u ON u.id = ap.user_id \
         WHERE ap.token_hash = $1 AND u.uid = $2 AND u.enabled = 1",
    )
    .bind(sha256_hex(token))
    .bind(uid)
    .fetch_optional(pool)
    .await?;
    let Some((ap_id, id, uid, display_name, is_admin)) = row else {
        return Ok(None);
    };
    // Touch last_used_at at most once a minute to keep writes down.
    let now = unix_now();
    sqlx::query("UPDATE app_passwords SET last_used_at = $1 WHERE id = $2 AND (last_used_at IS NULL OR last_used_at < $3)")
        .bind(now)
        .bind(ap_id)
        .bind(now - 60)
        .execute(pool)
        .await?;
    Ok(Some(AuthUser {
        id,
        uid,
        display_name,
        is_admin,
        app_password_id: Some(ap_id),
    }))
}

/// Authenticate a `Basic` header value with an app password, falling back to the real password.
pub async fn authenticate_basic(
    state: &AppState,
    ip: IpAddr,
    header_value: &str,
) -> Result<AuthUser, AuthError> {
    state.auth_gate(ip).map_err(AuthError::RateLimited)?;
    let decoded = header_value
        .strip_prefix("Basic ")
        .or_else(|| header_value.strip_prefix("basic "))
        .and_then(|b| {
            base64::engine::general_purpose::STANDARD
                .decode(b.trim())
                .ok()
        })
        .and_then(|b| String::from_utf8(b).ok());
    let Some((uid, secret)) = decoded.as_deref().and_then(|d| d.split_once(':')) else {
        state.auth_failed(ip);
        return Err(AuthError::Unauthorized);
    };
    let found = match verify_app_password(&state.pool, uid, secret).await {
        Ok(Some(u)) => Some(u),
        Ok(None) => verify_user_password(&state.pool, uid, secret)
            .await
            .ok()
            .flatten(),
        Err(e) => {
            tracing::error!(error = ?e, "app password lookup failed");
            None
        }
    };
    match found {
        Some(u) => {
            state.auth_ok(ip);
            Ok(u)
        }
        None => {
            state.auth_failed(ip);
            Err(AuthError::Unauthorized)
        }
    }
}

/// The address of the TCP peer, i.e. the proxy in front of the server (or the client itself when there is none).
/// This is what trust decisions about `X-Forwarded-*` must use; `ClientIp` is only for rate limits and logs.
pub struct Peer(pub IpAddr);

impl FromRequestParts<AppState> for Peer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Peer(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip())
                .unwrap_or(IpAddr::from([127, 0, 0, 1])),
        ))
    }
}

/// The client address (socket peer, or forwarded for trusted proxies).
pub struct ClientIp(pub IpAddr);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip())
            .unwrap_or(IpAddr::from([127, 0, 0, 1]));
        Ok(ClientIp(proxy::client_ip(
            &state.config,
            peer,
            &parts.headers,
        )))
    }
}

/// CSRF token of a browser session: derived from the (secret) session id, never stored. The web UI
/// renders it into the page and sends it back in `X-CSRF-Token`; it is not readable from cookies.
pub fn csrf_token_for(session_id: &str) -> String {
    sha256_hex(&format!("rb-csrf:{session_id}"))
}

/// Header carrying the CSRF token on requests the web UI makes with its session cookie.
pub const CSRF_HEADER: &str = "x-csrf-token";

/// The logged-in user of a browser session (the session id is the cookie value).
pub async fn session_auth_user(
    pool: &sqlx::SqlitePool,
    sid: &str,
) -> anyhow::Result<Option<AuthUser>> {
    let row: Option<(i64, String, String, bool)> = sqlx::query_as(
        "SELECT u.id, u.uid, u.display_name, u.is_admin FROM web_sessions s JOIN users u ON u.id = s.user_id \
         WHERE s.id_hash = $1 AND s.expires_at > $2 AND u.enabled = 1",
    )
    .bind(sha256_hex(sid))
    .bind(unix_now())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, uid, display_name, is_admin)| AuthUser {
        id,
        uid,
        display_name,
        is_admin,
        app_password_id: None,
    }))
}

/// Endpoints that mint or revoke credentials never accept a browser session cookie.
fn cookie_auth_allowed(path: &str) -> bool {
    !(path.ends_with("/core/getapppassword") || path.ends_with("/core/apppassword"))
}

pub struct AuthRejection(Response);

/// The message stock Nextcloud (SabreDAV) puts into its 401 for a request without credentials.
const SABRE_NOT_AUTHENTICATED: &str = "No public access to this resource., AppAPIAuth has not passed, This request is not for a federated calendar, No 'Authorization: Basic' header found. Either the client didn't send one, or the server is misconfigured, No 'Authorization: Bearer' header found. Either the client didn't send one, or the server is mis-configured, No 'Authorization: Basic' header found. Either the client didn't send one, or the server is misconfigured";

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        self.0
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ClientIp(ip) = ClientIp::from_request_parts(parts, state).await.unwrap();
        let format = ocs::negotiate(parts.uri.query(), &parts.headers);
        let path = parts.uri.path().to_string();
        // fetch/XHR from the web UI must never trigger the browser's native Basic-auth dialog
        let from_web_ui = parts.headers.contains_key(CSRF_HEADER)
            || parts
                .headers
                .get("x-requested-with")
                .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"XMLHttpRequest"));
        let reject = |status: StatusCode, msg: &str, retry: Option<u64>| {
            // each API family answers in its own format, like stock Nextcloud
            let mut r = if path.starts_with("/ocs/") {
                let mut r = ocs::error(Version::V2, format, status.as_u16(), msg);
                *r.status_mut() = status;
                r
            } else if path.starts_with("/remote.php/") {
                crate::dav::sabre_error(
                    status,
                    match status {
                        StatusCode::UNAUTHORIZED => "Sabre\\DAV\\Exception\\NotAuthenticated",
                        StatusCode::FORBIDDEN => "Sabre\\DAV\\Exception\\Forbidden",
                        _ => "Sabre\\DAV\\Exception\\TooManyRequests",
                    },
                    if status == StatusCode::UNAUTHORIZED {
                        SABRE_NOT_AUTHENTICATED
                    } else {
                        msg
                    },
                )
            } else {
                (
                    status,
                    [
                        (header::CONTENT_TYPE, "application/json; charset=utf-8"),
                        (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
                    ],
                    serde_json::json!({"message": msg}).to_string(),
                )
                    .into_response()
            };
            if status == StatusCode::UNAUTHORIZED && !from_web_ui {
                r.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Basic realm=\"Nextcloud\", charset=\"UTF-8\""),
                );
            }
            if let Some(s) = retry
                && let Ok(v) = HeaderValue::from_str(&s.to_string())
            {
                r.headers_mut().insert(header::RETRY_AFTER, v);
            }
            AuthRejection(r)
        };
        // Browser session: only without an Authorization header (a bad Basic header never falls back
        // to the cookie). Reads need nothing more, every other method needs the CSRF token.
        if !parts.headers.contains_key(header::AUTHORIZATION)
            && cookie_auth_allowed(&path)
            && let Some(sid) =
                crate::login::cookie_value(&parts.headers, crate::login::SESSION_COOKIE)
            && let Ok(Some(user)) = session_auth_user(&state.pool, &sid).await
        {
            let safe = matches!(parts.method.as_str(), "GET" | "HEAD" | "OPTIONS");
            let token_ok = parts
                .headers
                .get(CSRF_HEADER)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|t| crate::login::ct_eq(t, &csrf_token_for(&sid)));
            if safe || token_ok {
                return Ok(user);
            }
            return Err(reject(StatusCode::FORBIDDEN, "Invalid CSRF token", None));
        }
        let Some(h) = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
        else {
            return Err(reject(
                StatusCode::UNAUTHORIZED,
                "Current user is not logged in",
                None,
            ));
        };
        authenticate_basic(state, ip, h).await.map_err(|e| match e {
            AuthError::Unauthorized => reject(
                StatusCode::UNAUTHORIZED,
                "Current user is not logged in",
                None,
            ),
            AuthError::RateLimited(s) => reject(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many failed attempts",
                Some(s),
            ),
        })
    }
}

pub async fn user_email(pool: &sqlx::SqlitePool, user_id: i64) -> anyhow::Result<String> {
    Ok(
        sqlx::query_scalar::<_, Option<String>>("SELECT email FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?
            .flatten()
            .unwrap_or_default(),
    )
}

/// Used by the user endpoint: groups of a user.
pub async fn user_groups(pool: &sqlx::SqlitePool, user_id: i64) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT g.gid FROM groups g JOIN group_members m ON m.group_id = g.id WHERE m.user_id = $1 ORDER BY g.gid",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

pub fn user_json(u: &AuthUser, email: &str, groups: &[String]) -> serde_json::Value {
    // TODO(phase 0): align field set/quota semantics with a capture of stock NC 35.
    json!({
        "enabled": true,
        "id": u.uid,
        "email": if email.is_empty() { serde_json::Value::Null } else { json!(email) },
        "displayname": u.display_name,
        "display-name": u.display_name,
        "groups": groups,
        "language": "en",
        "locale": "",
        "backend": "Database",
        "subadmin": [],
        "quota": {"free": -3, "used": 0, "total": -3, "relative": 0, "quota": -3},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_have_length_alphabet_and_differ() {
        let a = random_token(APP_PASSWORD_LEN);
        let b = random_token(APP_PASSWORD_LEN);
        assert_eq!(a.len(), 72);
        assert!(a.bytes().all(|c| ALPHABET.contains(&c)));
        assert_ne!(a, b);
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
