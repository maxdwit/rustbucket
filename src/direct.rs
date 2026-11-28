//! Direct editing for the Nextcloud mobile apps ("Open in the text editor"): the stock apps ask the server which
//! editors it has (`/ocs/v2.php/apps/files/api/v1/directEditing`), call `directEditing/open` for a file and show the returned
//! one-time URL in a web view. Here that URL leads to our own editor page.
//!
//! The URL carries a token that works once and for five minutes. Opening it creates a *direct session* (its own cookie,
//! `rb_direct`, its own table): it is accepted by the editor page and the editor's WebSocket for that one file and by nothing
//! else, so a leaked web view cannot browse files or call any other endpoint.

use std::collections::HashMap;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

use crate::auth::{self, AuthUser, Peer, sha256_hex, unix_now};
use crate::login::{is_secure, set_cookie};
use crate::ocs::{self, Version};
use crate::routes::AppState;
use crate::shares::READ;
use crate::storage::Owner;

/// Cookie of a direct session.
pub const DIRECT_COOKIE: &str = "rb_direct";
const TOKEN_TTL_SECS: i64 = 300;
/// A direct session lasts as long as somebody may reasonably keep one file open on a tablet.
const SESSION_TTL_SECS: i64 = 12 * 3600;

pub fn router() -> Router<AppState> {
    let mut r = Router::new();
    for (prefix, v) in [("/ocs/v1.php", Version::V1), ("/ocs/v2.php", Version::V2)] {
        let base = format!("{prefix}/apps/files/api/v1/directEditing");
        r = r
            .route(&base, get(move |u, s, q, h| list(v, u, s, q, h)))
            .route(
                &format!("{base}/open"),
                axum::routing::post(move |u, s, p, q, h, b| open(v, u, s, p, q, h, b)),
            );
    }
    r.route("/apps/files/directEditing/{token}", get(redeem))
        .route("/index.php/apps/files/directEditing/{token}", get(redeem))
}

/// MIME types the editor offers itself for: those of its file extensions as the server stores them, plus the plain ones.
pub fn editor_mimetypes() -> Vec<String> {
    let mut out: Vec<String> = vec!["text/plain".into(), "text/markdown".into()];
    for ext in crate::editor::EDITABLE {
        let m = mime_guess::from_ext(ext.trim_start_matches('.'))
            .first_or_octet_stream()
            .essence_str()
            .to_string();
        if m != "application/octet-stream" && !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

/// The editors object of the stock answer (also what the capability's etag is computed from).
fn editors() -> Value {
    json!({
        "text": {
            "id": "text",
            "name": "Text",
            "mimetypes": editor_mimetypes(),
            "optionalMimetypes": [],
            "secure": false,
        }
    })
}

/// Entry for `capabilities.files.directEditing`.
pub fn capability(base: &str) -> Value {
    json!({
        "url": format!("{base}/ocs/v2.php/apps/files/api/v1/directEditing"),
        "etag": sha256_hex(&editors().to_string())[..32].to_string(),
        "supportsFileId": true,
    })
}

async fn list(
    v: Version,
    _user: AuthUser,
    State(st): State<AppState>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    let f = ocs::negotiate(q.as_deref(), &headers);
    if !st.config.editor {
        return ocs::error(v, f, 404, "");
    }
    ocs::respond(v, f, json!({"editors": editors(), "creators": {}}))
}

async fn open(
    v: Version,
    user: AuthUser,
    State(st): State<AppState>,
    Peer(peer): Peer,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let f = ocs::negotiate(q.as_deref(), &headers);
    let fail = |msg: &str| {
        ocs::error_data(
            v,
            f,
            403,
            "",
            json!({"message": format!("Failed to open file: {msg}")}),
        )
    };
    if !st.config.editor {
        return fail("the editor is switched off");
    }
    let p: HashMap<String, String> = crate::sharing_api::params(q.as_deref(), &headers, &body);
    if p.get("editorId").map(String::as_str) != Some("text") {
        return fail("Editor is unknown");
    }
    let me = Owner {
        id: user.id,
        uid: user.uid.clone(),
    };
    // by path, as the apps send it; a file id (when given and the path is missing) works too
    let file_id = if let Some(path) = p.get("path").filter(|s| !s.is_empty()) {
        let Ok(path) = crate::paths::normalize(path) else {
            return fail("bad path");
        };
        let res = match st.store.resolve(&me, &path).await {
            Ok(r) => r,
            Err(e) => return fail(&e.to_string()),
        };
        match st.store.get(&res.owner, &res.path).await {
            Ok(Some(n)) if res.can(READ) => n.fileid,
            _ => return fail(&format!("/{}", path.trim_start_matches('/'))),
        }
    } else if let Some(id) = p.get("fileId").and_then(|s| s.parse::<i64>().ok()) {
        id
    } else {
        return fail("no file given");
    };
    match crate::editor::access(&st, &me, file_id).await {
        Some(_) => {}
        None => return fail("this file cannot be edited here"),
    }
    let token = auth::random_token(64);
    let now = unix_now();
    let _ = sqlx::query("DELETE FROM direct_tokens WHERE expires_at <= $1")
        .bind(now)
        .execute(&st.pool)
        .await;
    if let Err(e) = sqlx::query(
        "INSERT INTO direct_tokens (token_hash, user_id, file_id, expires_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(sha256_hex(&token))
    .bind(user.id)
    .bind(file_id)
    .bind(now + TOKEN_TTL_SECS)
    .execute(&st.pool)
    .await
    {
        tracing::error!(error = %e, "cannot store direct editing token");
        return ocs::error(v, f, 500, "");
    }
    let base = crate::proxy::external_base_url(&st.config, peer, &headers);
    ocs::respond(
        v,
        f,
        json!({"url": format!("{base}/apps/files/directEditing/{token}")}),
    )
}

/// The one-time URL: exchange the token for a direct session and go to the editor.
async fn redeem(
    State(st): State<AppState>,
    Peer(peer): Peer,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    let now = unix_now();
    let row: Option<(i64, i64)> = sqlx::query_as(
        "DELETE FROM direct_tokens WHERE token_hash = $1 AND expires_at > $2 RETURNING user_id, file_id",
    )
    .bind(sha256_hex(&token))
    .bind(now)
    .fetch_optional(&st.pool)
    .await
    .unwrap_or(None);
    let Some((user_id, file_id)) = row else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let sid = auth::random_token(48);
    let _ = sqlx::query("DELETE FROM direct_sessions WHERE expires_at <= $1")
        .bind(now)
        .execute(&st.pool)
        .await;
    if let Err(e) = sqlx::query(
        "INSERT INTO direct_sessions (id_hash, user_id, file_id, expires_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(sha256_hex(&sid))
    .bind(user_id)
    .bind(file_id)
    .bind(now + SESSION_TTL_SECS)
    .execute(&st.pool)
    .await
    {
        tracing::error!(error = %e, "cannot create direct session");
        return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
    }
    let secure = is_secure(&st, &headers, peer);
    let mut r = (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("/apps/files/edit?id={file_id}&app=1"),
        )],
    )
        .into_response();
    r.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(DIRECT_COOKIE, &sid, SESSION_TTL_SECS, secure),
    );
    r.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    r.headers_mut()
        .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    r
}

/// The user and file of a valid direct session cookie.
pub async fn session(pool: &sqlx::SqlitePool, sid: &str) -> Option<(AuthUser, i64)> {
    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT s.user_id, s.file_id FROM direct_sessions s JOIN users u ON u.id = s.user_id \
         WHERE s.id_hash = $1 AND s.expires_at > $2 AND u.enabled = 1",
    )
    .bind(sha256_hex(sid))
    .bind(unix_now())
    .fetch_optional(pool)
    .await
    .ok()?;
    let (user_id, file_id) = row?;
    let (id, uid, display_name, is_admin): (i64, String, String, bool) =
        sqlx::query_as("SELECT id, uid, display_name, is_admin FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(pool)
            .await
            .ok()?;
    Some((
        AuthUser {
            id,
            uid,
            display_name,
            is_admin,
            app_password_id: None,
        },
        file_id,
    ))
}
