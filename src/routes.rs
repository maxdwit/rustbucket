//! HTTP routes: status, capabilities, health, OCS user/app-password endpoints, avatars.

use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};

use crate::auth::{self, AuthUser};
use crate::config::Config;
use crate::error::AppError;
use crate::ocs::{self, Version};
use crate::throttle::Throttle;

/// Version reported to clients. Mobile apps check this, so it must look like Nextcloud 35.x.
pub const NC_MAJOR: u32 = 35;
pub const NC_MINOR: u32 = 0;
pub const NC_MICRO: u32 = 1;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    pub throttle: Arc<Throttle>,
    /// Failures per IP over a long window that a successful login does NOT clear: somebody with one valid
    /// account must not be able to buy unlimited guesses against the others by logging in in between.
    pub throttle_hard: Arc<Throttle>,
    pub store: crate::storage::Store,
    pub metrics: Arc<crate::ops::Metrics>,
    /// open rooms of the live editor
    pub editors: Arc<crate::editor::Editors>,
}

impl AppState {
    /// May this client try to authenticate now? `Err(seconds)` = throttled.
    pub fn auth_gate(&self, ip: std::net::IpAddr) -> Result<(), u64> {
        let now = std::time::Instant::now();
        self.throttle.check(ip, now)?;
        self.throttle_hard.check(ip, now)
    }

    pub fn auth_failed(&self, ip: std::net::IpAddr) {
        let now = std::time::Instant::now();
        self.throttle.fail(ip, now);
        self.throttle_hard.fail(ip, now);
    }

    /// A success clears the short-window counter only (so a client with one stale device is not locked out for good).
    pub fn auth_ok(&self, ip: std::net::IpAddr) {
        self.throttle.reset(ip);
    }

    /// 5 failed logins per IP and 10 minutes, then blocked until the window ends.
    pub fn new(pool: SqlitePool, config: Arc<Config>) -> Self {
        let throttle = Arc::new(Throttle::new(5, std::time::Duration::from_secs(600)));
        let throttle_hard = Arc::new(Throttle::new(30, std::time::Duration::from_secs(3600)));
        crate::auth::warm_up(); // the dummy hash for unknown users must not be computed lazily on a request
        let store = crate::storage::Store::new(pool.clone(), config.data_dir.clone())
            .with_versions_max(config.versions_max as usize);
        Self {
            pool,
            config,
            throttle,
            throttle_hard,
            store,
            metrics: crate::ops::shared(),
            editors: Arc::default(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/status.php", get(status))
        .route("/health", get(health))
        .route(
            "/ocs/v1.php/cloud/capabilities",
            get(|st, p, q, h| capabilities(Version::V1, st, p, q, h)),
        )
        .route(
            "/ocs/v2.php/cloud/capabilities",
            get(|st, p, q, h| capabilities(Version::V2, st, p, q, h)),
        )
        .route(
            "/ocs/v1.php/cloud/user",
            get(|u, s, q, h| cloud_user(Version::V1, u, s, q, h)),
        )
        .route(
            "/ocs/v2.php/cloud/user",
            get(|u, s, q, h| cloud_user(Version::V2, u, s, q, h)),
        )
        .route(
            "/ocs/v1.php/core/getapppassword",
            get(|u, s, q, h| get_app_password(Version::V1, u, s, q, h)),
        )
        .route(
            "/ocs/v2.php/core/getapppassword",
            get(|u, s, q, h| get_app_password(Version::V2, u, s, q, h)),
        )
        .route(
            "/ocs/v1.php/core/apppassword",
            delete(|u, s, q, h| delete_app_password(Version::V1, u, s, q, h)),
        )
        .route(
            "/ocs/v2.php/core/apppassword",
            delete(|u, s, q, h| delete_app_password(Version::V2, u, s, q, h)),
        )
        .route("/index.php/avatar/{user}/{size}", get(avatar))
        .route("/avatar/{user}/{size}", get(avatar))
        .route("/remote.php/dav/avatars/{user}/{size}", get(avatar))
        // connectivity probe used by the desktop client
        .route(
            "/ocs/v2.php/core/navigation/apps",
            get(|u, q, h| navigation_apps(Version::V2, u, q, h)),
        )
        .route(
            "/ocs/v1.php/core/navigation/apps",
            get(|u, q, h| navigation_apps(Version::V1, u, q, h)),
        )
        .route(
            "/ocs/v2.php/apps/terms_of_service/terms",
            get(|q, h| no_terms(Version::V2, q, h)),
        )
        .route("/index.php/204", get(|| async { StatusCode::NO_CONTENT }))
        .route("/metrics", get(crate::ops::metrics))
        .merge(crate::login::router())
        .merge(crate::dav::router())
        .merge(crate::uploads::router())
        .merge(crate::preview::router())
        .merge(crate::mobile::router())
        .merge(crate::sharing_api::router())
        .merge(crate::trashbin::router())
        .merge(crate::web::router())
        .merge(crate::editor::router())
        .merge(crate::direct::router())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::ops::layer,
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
        )
        .with_state(state)
}

// TODO(phase 0): verify the exact field set and values against a capture of stock NC 35.
pub fn status_json() -> Value {
    json!({
        "installed": true,
        "maintenance": false,
        "needsDbUpgrade": false,
        "version": format!("{NC_MAJOR}.{NC_MINOR}.{NC_MICRO}.1"),
        "versionstring": format!("{NC_MAJOR}.{NC_MINOR}.{NC_MICRO}"),
        "edition": "",
        "productname": "Nextcloud",
        "extendedSupport": false,
    })
}

async fn status() -> Json<Value> {
    Json(status_json())
}

/// Only advertise what is implemented; clients act on every capability they see.
// TODO(phase 0): align with the capabilities capture of stock NC 35.
pub fn capabilities_json() -> Value {
    json!({
        "version": {
            "major": NC_MAJOR, "minor": NC_MINOR, "micro": NC_MICRO,
            "string": format!("{NC_MAJOR}.{NC_MINOR}.{NC_MICRO}"),
            "edition": "", "extendedSupport": false,
        },
        "capabilities": {
            "core": {"pollinterval": 60, "webdav-root": "remote.php/webdav"},
            "files": {
                "bigfilechunking": true,
                "undelete": true,
                "versioning": true,
                "version_labeling": false,
                "version_deletion": true,
            },
            // only the chunking key of the stock dav block: bulkupload would send small files to /remote.php/dav/bulk
            "dav": {"chunking": "1.0"},
            // user and group shares only: no public links, no resharing, no federation
            "files_sharing": {
                "api_enabled": true,
                "public": {"enabled": false},
                "resharing": false,
                "user": {"send_mail": false, "expire_date": {"enabled": false}},
                "group_sharing": true,
                "group": {"enabled": true, "expire_date": {"enabled": false}},
                "default_permissions": 31,
                "federation": {"outgoing": false, "incoming": false,
                               "expire_date": {"enabled": false}, "expire_date_supported": {"enabled": false}},
                "sharee": {"minSearchStringLength": 0, "query_lookup_default": false, "always_show_unique": true},
            },
            // "notifications" is NOT advertised on purpose: Android would try push registration (needs
            // an external push proxy). The endpoints exist (mobile.rs); re-enable with Phase 6 share alerts.
        },
    })
}

async fn capabilities(
    version: Version,
    State(st): State<AppState>,
    auth::Peer(peer): auth::Peer,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    let mut caps = capabilities_json();
    if st.config.editor {
        // the mobile apps open text files in a web view through this (see direct.rs); stock advertises it with absolute URLs
        let base = crate::proxy::external_base_url(&st.config, peer, &headers);
        caps["capabilities"]["files"]["directEditing"] = crate::direct::capability(&base);
    }
    ocs::respond(version, ocs::negotiate(q.as_deref(), &headers), caps)
}

async fn health(State(s): State<AppState>) -> Response {
    match sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&s.pool)
        .await
    {
        Ok(_) => Json(json!({"status": "ok"})).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "health check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"status": "db unavailable"})),
            )
                .into_response()
        }
    }
}

async fn cloud_user(
    version: Version,
    user: AuthUser,
    State(s): State<AppState>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let groups = auth::user_groups(&s.pool, user.id).await?;
    let email = auth::user_email(&s.pool, user.id).await?;
    Ok(ocs::respond(
        version,
        ocs::negotiate(q.as_deref(), &headers),
        auth::user_json(&user, &email, &groups),
    ))
}

/// No web apps to list yet (the clients only use this for their app menu).
async fn navigation_apps(
    version: Version,
    _user: AuthUser,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    ocs::respond(version, ocs::negotiate(q.as_deref(), &headers), json!([]))
}

/// Stock Nextcloud without the terms_of_service app answers with an OCS 404.
async fn no_terms(version: Version, RawQuery(q): RawQuery, headers: HeaderMap) -> Response {
    ocs::error(version, ocs::negotiate(q.as_deref(), &headers), 404, "")
}

/// Legacy app-password endpoint: authenticated with the real password, returns a new app password.
async fn get_app_password(
    version: Version,
    user: AuthUser,
    State(s): State<AppState>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let format = ocs::negotiate(q.as_deref(), &headers);
    if user.app_password_id.is_some() {
        return Ok(ocs::error(
            version,
            format,
            403,
            "Using an app password to get an app password is not allowed",
        ));
    }
    let name = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(200).collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown device".into());
    let token = auth::create_app_password(&s.pool, user.id, &name).await?;
    Ok(ocs::respond(version, format, json!({"apppassword": token})))
}

/// Revokes the app password the request was authenticated with (client "log out").
async fn delete_app_password(
    version: Version,
    user: AuthUser,
    State(s): State<AppState>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let format = ocs::negotiate(q.as_deref(), &headers);
    let Some(id) = user.app_password_id else {
        return Ok(ocs::error(version, format, 400, "No app password in use"));
    };
    sqlx::query("DELETE FROM app_passwords WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(user.id)
        .execute(&s.pool)
        .await?;
    Ok(ocs::respond(version, format, json!([])))
}

/// Generated default avatar: a solid square whose colour is derived from the user id.
async fn avatar(
    _auth: AuthUser,
    State(s): State<AppState>,
    Path((uid, size)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE uid = $1 AND enabled = 1)")
            .bind(&uid)
            .fetch_one(&s.pool)
            .await?;
    if !exists {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let px: u32 = size
        .trim_end_matches(".png")
        .parse()
        .unwrap_or(64)
        .clamp(16, 512);
    let digest = auth::sha256_hex(&uid);
    let c = |i: usize| 64 + (u8::from_str_radix(&digest[i * 2..i * 2 + 2], 16).unwrap_or(0) / 2);
    let rgb = [c(0), c(1), c(2)];
    let png = crate::avatar::solid_png(px, rgb)?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "private, max-age=3600"),
        ],
        png,
    )
        .into_response())
}
