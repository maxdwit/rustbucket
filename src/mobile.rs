//! Small endpoints the Android/iOS apps probe, answered like stock Nextcloud 35 does when the
//! feature is empty: notifications (none), user status (none set), remote wipe check (never),
//! and the storage stats (shares live in `sharing_api`).

use axum::Json;
use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::{Value, json};

use crate::auth::AuthUser;
use crate::dav::{internal, owner};
use crate::ocs::{self, Version};
use crate::routes::AppState;

fn predefined_statuses() -> Value {
    json!([
        {"id": "meeting", "icon": "\u{1f4c5}", "message": "In a meeting", "clearAt": {"type": "period", "time": 3600}},
        {"id": "commuting", "icon": "\u{1f68c}", "message": "Commuting", "clearAt": {"type": "period", "time": 1800}},
        {"id": "be-right-back", "icon": "\u{23f3}", "message": "Be right back", "clearAt": {"type": "period", "time": 900}},
        {"id": "remote-work", "icon": "\u{1f3e1}", "message": "Working remotely", "clearAt": {"type": "end-of", "time": "day"}},
        {"id": "sick-leave", "icon": "\u{1f912}", "message": "Out sick", "clearAt": {"type": "end-of", "time": "day"}},
        {"id": "vacationing", "icon": "\u{1f334}", "message": "Vacationing", "clearAt": null},
    ])
}

pub fn router() -> Router<AppState> {
    let mut r = Router::new()
        .route("/index.php/core/wipe/check", post(not_wiped))
        .route("/index.php/core/wipe/success", post(not_wiped))
        .route("/index.php/apps/files/api/v1/stats", get(stats))
        .route("/apps/files/api/v1/stats", get(stats));
    for (prefix, v) in [("/ocs/v1.php", Version::V1), ("/ocs/v2.php", Version::V2)] {
        r = r
            .route(
                &format!("{prefix}/apps/notifications/api/v2/notifications"),
                get(move |_u: AuthUser, q: RawQuery, h: HeaderMap| async move {
                    ocs::respond(v, ocs::negotiate(q.0.as_deref(), &h), json!([]))
                })
                .delete(
                    move |_u: AuthUser, q: RawQuery, h: HeaderMap| async move {
                        ocs::respond(v, ocs::negotiate(q.0.as_deref(), &h), json!([]))
                    },
                ),
            )
            .route(
                &format!("{prefix}/apps/notifications/api/v2/notifications/{{id}}"),
                get(move |_u: AuthUser, q: RawQuery, h: HeaderMap| async move {
                    ocs::error_data(v, ocs::negotiate(q.0.as_deref(), &h), 404, "", Value::Null)
                })
                .merge(delete(
                    move |_u: AuthUser, q: RawQuery, h: HeaderMap| async move {
                        ocs::respond(v, ocs::negotiate(q.0.as_deref(), &h), json!([]))
                    },
                )),
            )
            .route(
                &format!("{prefix}/apps/user_status/api/v1/user_status"),
                get(move |_u: AuthUser, q: RawQuery, h: HeaderMap| async move {
                    ocs::error(
                        v,
                        ocs::negotiate(q.0.as_deref(), &h),
                        404,
                        "No status for the current user",
                    )
                }),
            )
            .route(
                &format!("{prefix}/apps/user_status/api/v1/predefined_statuses"),
                get(move |_u: AuthUser, q: RawQuery, h: HeaderMap| async move {
                    ocs::respond(v, ocs::negotiate(q.0.as_deref(), &h), predefined_statuses())
                }),
            );
    }
    r
}

/// Stock answers 404 `[]` unless the token is scheduled for a remote wipe; we never schedule one.
async fn not_wiped() -> Response {
    (
        StatusCode::NOT_FOUND,
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
        ],
        "[]",
    )
        .into_response()
}

async fn stats(State(st): State<AppState>, user: AuthUser) -> Response {
    let used = match st.store.get(&owner(&user), "").await {
        Ok(n) => n.map_or(0, |n| n.size.max(0) as u64),
        Err(e) => return internal(e),
    };
    let free = st.store.free_bytes();
    let total = free + used;
    let relative = if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64 * 10_000.0).round() / 100.0
    };
    let mut r = Json(json!({
        "message": "ok",
        "data": {
            "free": free, "used": used, "quota": -3, "total": total, "relative": relative,
            "owner": user.uid, "ownerDisplayName": user.display_name,
            "mountType": "", "mountPoint": "",
        }
    }))
    .into_response();
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, max-age=300, must-revalidate"),
    );
    r
}
