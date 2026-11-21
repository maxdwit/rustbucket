//! OCS share API (`/ocs/v{1,2}.php/apps/files_sharing/api/v1/{shares,sharees}`): user and group
//! shares only. Statuses, messages and JSON shapes follow recordings of stock Nextcloud 35.0.3.

#![allow(clippy::result_large_err)] // helpers return a ready-made error `Response`

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};

use crate::auth::{AuthUser, unix_now};
use crate::dav::{internal, owner};
use crate::ocs::{self, Format, Version};
use crate::preview::previewable;
use crate::routes::AppState;
use crate::shares::{ALL, CREATE, DELETE, READ, TYPE_GROUP, TYPE_USER};
use crate::storage::Node;

pub fn router() -> axum::Router<AppState> {
    let mut r = axum::Router::new();
    for (prefix, v) in [("/ocs/v1.php", Version::V1), ("/ocs/v2.php", Version::V2)] {
        let base = format!("{prefix}/apps/files_sharing/api/v1");
        r = r
            .route(
                &format!("{base}/shares"),
                get(move |u, s, q, h| list(v, u, s, q, h))
                    .post(move |u, s, q, h, b| create(v, u, s, q, h, b)),
            )
            .route(
                &format!("{base}/shares/{{id}}"),
                get(move |u, s, p, q, h| one(v, u, s, p, q, h))
                    .put(move |u, s, p, q, h, b| update(v, u, s, p, q, h, b))
                    .delete(move |u, s, p, q, h| remove(v, u, s, p, q, h)),
            )
            .route(
                &format!("{base}/sharees"),
                get(move |u, s, q, h| sharees(v, u, s, q, h)),
            );
    }
    r
}

#[derive(sqlx::FromRow)]
struct ShareRow {
    id: i64,
    share_type: i64,
    owner_id: i64,
    file_source: i64,
    share_with: String,
    permissions: i64,
    stime: i64,
    note: String,
    owner_uid: String,
    owner_display: String,
    with_display: String,
    target: Option<String>,
}

// `target` is the recipient's mount point; for the owner's own view of a user share it is that
// recipient's, for group shares there is none until a member first looks.
macro_rules! share_select {
    ($tail:literal) => {
        concat!(
            "SELECT s.id, s.share_type, s.owner_id, s.file_source, s.share_with, s.permissions, s.stime, s.note, \
     o.uid AS owner_uid, o.display_name AS owner_display, COALESCE(w.display_name, s.share_with) AS with_display, \
     (SELECT t.target FROM share_targets t WHERE t.share_id = s.id AND t.user_id = w.id) AS target \
     FROM shares s JOIN users o ON o.id = s.owner_id \
     LEFT JOIN users w ON s.share_type = 0 AND w.uid = s.share_with ",
            $tail
        )
    };
}

fn params(query: Option<&str>, headers: &HeaderMap, body: &[u8]) -> HashMap<String, String> {
    fn pairs(s: &str, out: &mut HashMap<String, String>) {
        for kv in s.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            let dec = |x: &str| {
                percent_decode_str(&x.replace('+', " "))
                    .decode_utf8_lossy()
                    .into_owned()
            };
            out.insert(dec(k), dec(v));
        }
    }
    let mut out = HashMap::new();
    if let Some(q) = query {
        pairs(q, &mut out);
    }
    let is_json = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"));
    if is_json {
        if let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(body) {
            for (k, v) in m {
                let s = match v {
                    Value::String(s) => s,
                    Value::Null => continue,
                    other => other.to_string(),
                };
                out.insert(k, s);
            }
        }
    } else {
        pairs(&String::from_utf8_lossy(body), &mut out);
    }
    out
}

fn err(v: Version, f: Format, code: u16, msg: &str) -> Response {
    ocs::error(v, f, code, msg)
}

/// One share as JSON. `recipient`: the share as its recipient sees it (mount point, reduced permissions).
fn share_json(r: &ShareRow, n: &Node, recipient: bool, created: bool) -> Value {
    let owner_path = format!("/{}", n.path);
    let target = format!("/{}", r.target.clone().unwrap_or_else(|| n.name.clone()));
    let perms = r.permissions as u32;
    let item_permissions = if recipient {
        perms | DELETE
    } else if n.is_dir {
        ALL
    } else {
        ALL & !CREATE
    };
    let is_user = r.share_type == TYPE_USER;
    let mut share = json!({
        "id": r.id.to_string(),
        "share_type": r.share_type,
        "uid_owner": r.owner_uid,
        "displayname_owner": r.owner_display,
        "permissions": perms,
        "can_edit": !recipient,
        "can_delete": !(recipient && !is_user), // a group recipient cannot delete the share for everybody
        "stime": r.stime,
        "parent": null,
        "expiration": null,
        "token": null,
        "uid_file_owner": r.owner_uid,
        "note": r.note,
        "label": "",
        "displayname_file_owner": r.owner_display,
        "path": owner_path,
        "item_type": if n.is_dir { "folder" } else { "file" },
        "item_permissions": item_permissions,
        "is-mount-root": recipient,
        "mount-type": if recipient { "shared" } else { "" },
        "mimetype": if n.is_dir { "httpd/unix-directory" } else { n.mimetype.as_str() },
        "has_preview": !n.is_dir && previewable(&n.mimetype),
        "storage_id": if recipient { format!("shared::{target}") } else { format!("home::{}", r.owner_uid) },
        "storage": r.owner_id,
        "item_source": n.fileid,
        "file_source": n.fileid,
        "file_parent": n.parent_id,
        "file_target": target,
        "item_size": n.size,
        "item_mtime": n.mtime,
        "share_with": r.share_with,
        "share_with_displayname": r.with_display,
        "mail_send": i32::from(created), // stock: 1 in the answer to a create, 0 everywhere else
        "hide_download": 0,
        "attributes": null,
    });
    if is_user {
        share["share_with_displayname_unique"] = json!(r.share_with);
    }
    share
}

async fn render_rows(
    st: &AppState,
    rows: Vec<ShareRow>,
    recipient: bool,
) -> Result<Vec<Value>, Response> {
    let mut out = Vec::new();
    for r in rows {
        if let Some(n) = st.store.node_by_id(r.file_source).await.map_err(internal)? {
            out.push(share_json(&r, &n, recipient, false));
        }
    }
    Ok(out)
}

async fn row(st: &AppState, id: i64) -> Result<Option<ShareRow>, Response> {
    sqlx::query_as(share_select!("WHERE s.id = $1"))
        .bind(id)
        .fetch_optional(st.store.pool())
        .await
        .map_err(internal)
}

/// Shares that apply to the user as a recipient (and are not hidden by them), with their own target.
async fn incoming(st: &AppState, user: &AuthUser) -> Result<Vec<ShareRow>, Response> {
    let me = owner(user);
    st.store.mounts(&me).await.map_err(internal)?; // creates missing group-share targets
    sqlx::query_as(
        "SELECT s.id, s.share_type, s.owner_id, s.file_source, s.share_with, s.permissions, s.stime, s.note, \
                o.uid AS owner_uid, o.display_name AS owner_display, COALESCE(w.display_name, s.share_with) AS with_display, t.target AS target \
         FROM shares s JOIN users o ON o.id = s.owner_id \
         LEFT JOIN users w ON s.share_type = 0 AND w.uid = s.share_with \
         JOIN share_targets t ON t.share_id = s.id AND t.user_id = $1 AND t.hidden = 0 \
         WHERE (s.share_type = 0 AND s.share_with = $2) \
            OR (s.share_type = 1 AND s.share_with IN \
                (SELECT g.gid FROM groups g JOIN group_members m ON m.group_id = g.id WHERE m.user_id = $1)) \
         ORDER BY s.id DESC", // stock lists incoming shares newest first
    )
    .bind(me.id)
    .bind(&user.uid)
    .fetch_all(st.store.pool())
    .await
    .map_err(internal)
}

fn parse_perms(
    p: &HashMap<String, String>,
    default: u32,
    is_dir: bool,
) -> Result<u32, &'static str> {
    let perms = match p.get("permissions") {
        None => default,
        Some(s) => s
            .trim()
            .parse::<u32>()
            .map_err(|_| "Valid permissions are required for sharing")?,
    };
    if perms == 0 || perms > ALL || perms & READ == 0 {
        return Err("Valid permissions are required for sharing");
    }
    // files cannot be created into or deleted from: stock reduces a file share to read/update/share
    Ok(if is_dir {
        perms
    } else {
        perms & !(CREATE | DELETE)
    })
}

async fn create(
    v: Version,
    user: AuthUser,
    State(st): State<AppState>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let f = ocs::negotiate(q.as_deref(), &headers);
    let p = params(q.as_deref(), &headers, &body);
    let share_type = match p
        .get("shareType")
        .and_then(|s| s.trim().parse::<i64>().ok())
    {
        Some(t @ (TYPE_USER | TYPE_GROUP)) => t,
        Some(3) => return err(v, f, 403, "Public sharing is not allowed"),
        _ => return err(v, f, 400, "Unknown share type"),
    };
    let Some(path_raw) = p.get("path").filter(|s| !s.is_empty()) else {
        return err(v, f, 404, "Please specify a file or folder path");
    };
    let Ok(path) = crate::paths::normalize(path_raw) else {
        return err(v, f, 404, "Wrong path, file/folder does not exist");
    };
    let me = owner(&user);
    let res = match st.store.resolve(&me, &path).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let node = match st.store.get(&res.owner, &res.path).await {
        Ok(Some(n)) => n,
        Ok(None) => return err(v, f, 404, "Wrong path, file/folder does not exist"),
        Err(e) => return internal(e),
    };
    if res.is_shared() {
        return err(v, f, 403, "Resharing is not allowed");
    }
    if path.is_empty() {
        return err(v, f, 403, "You cannot share your root folder");
    }
    let Some(with) = p.get("shareWith").filter(|s| !s.is_empty()).cloned() else {
        return err(v, f, 404, "Please specify a valid account to share with");
    };
    let pool = st.store.pool();
    let default = if node.is_dir { ALL } else { 19 };
    let perms = match parse_perms(&p, default, node.is_dir) {
        Ok(x) => x,
        Err(m) => return err(v, f, 403, m),
    };
    let recipient_id: Option<i64> = if share_type == TYPE_USER {
        if with == user.uid {
            return err(v, f, 403, "Cannot share with yourself");
        }
        match sqlx::query_scalar("SELECT id FROM users WHERE uid = $1")
            .bind(&with)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(id)) => Some(id),
            Ok(None) => return err(v, f, 404, "Please specify a valid account to share with"),
            Err(e) => return internal(e),
        }
    } else {
        match sqlx::query_scalar::<_, i64>("SELECT id FROM groups WHERE gid = $1")
            .bind(&with)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(_)) => None,
            Ok(None) => return err(v, f, 404, "Please specify a valid group"),
            Err(e) => return internal(e),
        }
    };
    let dup: Result<Option<i64>, _> = sqlx::query_scalar(
        "SELECT id FROM shares WHERE owner_id = $1 AND file_source = $2 AND share_type = $3 AND share_with = $4",
    )
    .bind(me.id)
    .bind(node.fileid)
    .bind(share_type)
    .bind(&with)
    .fetch_optional(pool)
    .await;
    match dup {
        Ok(Some(_)) => return err(v, f, 403, "Path is already shared with this user"),
        Ok(None) => {}
        Err(e) => return internal(e),
    }
    let note = p.get("note").cloned().unwrap_or_default();
    let id = match sqlx::query(
        "INSERT INTO shares (share_type, owner_id, file_source, share_with, permissions, stime, note) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(share_type)
    .bind(me.id)
    .bind(node.fileid)
    .bind(&with)
    .bind(i64::from(perms))
    .bind(unix_now())
    .bind(&note)
    .execute(pool)
    .await
    {
        Ok(r) => r.last_insert_rowid(),
        Err(e) => return internal(e),
    };
    if let Some(rid) = recipient_id {
        // pick the mount name now so the answer already carries the final file_target ("proj (2)")
        let target = match st.store.default_target(rid, &node).await {
            Ok(t) => t,
            Err(e) => return internal(e),
        };
        if let Err(e) = st.store.set_target(id, rid, &target).await {
            return internal(e);
        }
    }
    match row(&st, id).await {
        Ok(Some(r)) => ocs::respond(v, f, share_json(&r, &node, false, true)),
        Ok(None) => internal("share vanished"),
        Err(r) => r,
    }
}

async fn list(
    v: Version,
    user: AuthUser,
    State(st): State<AppState>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    let f = ocs::negotiate(q.as_deref(), &headers);
    let p = params(q.as_deref(), &headers, &[]);
    let truthy = |k: &str| matches!(p.get(k).map(String::as_str), Some("true" | "1"));
    let me = owner(&user);
    if truthy("shared_with_me") {
        let rows = match incoming(&st, &user).await {
            Ok(r) => r,
            Err(r) => return r,
        };
        return match render_rows(&st, rows, true).await {
            Ok(list) => ocs::respond(v, f, Value::Array(list)),
            Err(r) => r,
        };
    }
    let mut rows: Vec<ShareRow> =
        match sqlx::query_as(share_select!("WHERE s.owner_id = $1 ORDER BY s.id"))
            .bind(me.id)
            .fetch_all(st.store.pool())
            .await
        {
            Ok(r) => r,
            Err(e) => return internal(e),
        };
    if let Some(path) = p.get("path").filter(|s| !s.is_empty()) {
        let Ok(path) = crate::paths::normalize(path) else {
            return err(v, f, 404, "Wrong path, file/folder does not exist");
        };
        let res = match st.store.resolve(&me, &path).await {
            Ok(r) => r,
            Err(e) => return internal(e),
        };
        let node = match st.store.get(&res.owner, &res.path).await {
            Ok(Some(n)) => n,
            Ok(None) => return err(v, f, 404, "Wrong path, file/folder does not exist"),
            Err(e) => return internal(e),
        };
        if res.is_shared() {
            return ocs::respond(v, f, json!([]));
        }
        let ids: Vec<i64> = if truthy("subfiles") && node.is_dir {
            match st.store.children(&node).await {
                Ok(k) => k.into_iter().map(|n| n.fileid).collect(),
                Err(e) => return internal(e),
            }
        } else {
            vec![node.fileid]
        };
        rows.retain(|r| ids.contains(&r.file_source));
    }
    match render_rows(&st, rows, false).await {
        Ok(list) => ocs::respond(v, f, Value::Array(list)),
        Err(r) => r,
    }
}

/// A share the user may look at: they own it or receive it. `None` = stock's "Wrong share ID".
async fn visible(
    st: &AppState,
    user: &AuthUser,
    id: i64,
) -> Result<Option<(ShareRow, bool)>, Response> {
    let Some(r) = row(st, id).await? else {
        return Ok(None);
    };
    if r.owner_id == user.id {
        return Ok(Some((r, false)));
    }
    let mine = incoming(st, user).await?;
    Ok(mine.into_iter().find(|m| m.id == id).map(|m| (m, true)))
}

async fn one(
    v: Version,
    user: AuthUser,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    let f = ocs::negotiate(q.as_deref(), &headers);
    match visible(&st, &user, id).await {
        Ok(Some((r, recipient))) => match st.store.node_by_id(r.file_source).await {
            Ok(Some(n)) => ocs::respond(v, f, json!([share_json(&r, &n, recipient, false)])),
            Ok(None) => err(v, f, 404, "Wrong share ID, share does not exist"),
            Err(e) => internal(e),
        },
        Ok(None) => err(v, f, 404, "Wrong share ID, share does not exist"),
        Err(r) => r,
    }
}

async fn update(
    v: Version,
    user: AuthUser,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let f = ocs::negotiate(q.as_deref(), &headers);
    let p = params(q.as_deref(), &headers, &body);
    let r = match visible(&st, &user, id).await {
        Ok(Some((r, false))) => r,
        Ok(Some((_, true))) => {
            return err(v, f, 403, "You are not allowed to edit incoming shares");
        }
        Ok(None) => return err(v, f, 404, "Wrong share ID, share does not exist"),
        Err(r) => return r,
    };
    let node = match st.store.node_by_id(r.file_source).await {
        Ok(Some(n)) => n,
        Ok(None) => return err(v, f, 404, "Wrong share ID, share does not exist"),
        Err(e) => return internal(e),
    };
    const KNOWN: [&str; 8] = [
        "permissions",
        "note",
        "attributes",
        "expireDate",
        "label",
        "hideDownload",
        "password",
        "publicUpload",
    ];
    if !KNOWN.iter().any(|k| p.contains_key(*k)) {
        return err(v, f, 400, "Wrong or no update parameter given");
    }
    if p.contains_key("permissions") {
        let perms = match parse_perms(&p, r.permissions as u32, node.is_dir) {
            Ok(x) => x,
            Err(m) => return err(v, f, 400, m),
        };
        if let Err(e) = sqlx::query("UPDATE shares SET permissions = $1 WHERE id = $2")
            .bind(i64::from(perms))
            .bind(id)
            .execute(st.store.pool())
            .await
        {
            return internal(e);
        }
    }
    if let Some(note) = p.get("note")
        && let Err(e) = sqlx::query("UPDATE shares SET note = $1 WHERE id = $2")
            .bind(note)
            .bind(id)
            .execute(st.store.pool())
            .await
    {
        return internal(e);
    }
    match row(&st, id).await {
        Ok(Some(r)) => ocs::respond(v, f, share_json(&r, &node, false, false)),
        Ok(None) => err(v, f, 404, "Wrong share ID, share does not exist"),
        Err(r) => r,
    }
}

async fn remove(
    v: Version,
    user: AuthUser,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    let f = ocs::negotiate(q.as_deref(), &headers);
    match visible(&st, &user, id).await {
        Ok(Some((r, false))) => match sqlx::query("DELETE FROM shares WHERE id = $1")
            .bind(r.id)
            .execute(st.store.pool())
            .await
        {
            Ok(_) => ocs::respond(v, f, json!([])),
            Err(e) => internal(e),
        },
        // a recipient removing the share only removes it for themselves
        Ok(Some((r, true))) => match st.store.unshare_for(&owner(&user), r.file_source).await {
            Ok(()) => ocs::respond(v, f, json!([])),
            Err(e) => internal(e),
        },
        Ok(None) => err(v, f, 404, "Wrong share ID, share does not exist"),
        Err(r) => r,
    }
}

async fn sharees(
    v: Version,
    user: AuthUser,
    State(st): State<AppState>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    let f = ocs::negotiate(q.as_deref(), &headers);
    let p = params(q.as_deref(), &headers, &[]);
    let search = p.get("search").cloned().unwrap_or_default();
    let per_page: i64 = p
        .get("perPage")
        .and_then(|s| s.parse().ok())
        .unwrap_or(200)
        .clamp(1, 500);
    let pool = st.store.pool();
    let users: Vec<(String, String)> = match sqlx::query_as(
        "SELECT uid, display_name FROM users WHERE uid <> $1 AND \
         (instr(lower(uid), lower($2)) > 0 OR instr(lower(display_name), lower($2)) > 0) \
         ORDER BY display_name, uid LIMIT $3",
    )
    .bind(&user.uid)
    .bind(&search)
    .bind(per_page)
    .fetch_all(pool)
    .await
    {
        Ok(u) => u,
        Err(e) => return internal(e),
    };
    let groups: Vec<String> = match sqlx::query_scalar(
        "SELECT gid FROM groups WHERE instr(lower(gid), lower($1)) > 0 ORDER BY gid LIMIT $2",
    )
    .bind(&search)
    .bind(per_page)
    .fetch_all(pool)
    .await
    {
        Ok(g) => g,
        Err(e) => return internal(e),
    };
    let needle = search.to_lowercase();
    let (mut exact_users, mut list_users) = (Vec::new(), Vec::new());
    for (uid, display) in users {
        let entry = json!({
            "label": display, "subline": "", "icon": "icon-user",
            "value": {"shareType": TYPE_USER, "shareWith": uid},
            "shareWithDisplayNameUnique": uid, "status": [],
        });
        if !needle.is_empty() && (uid.to_lowercase() == needle || display.to_lowercase() == needle)
        {
            exact_users.push(entry);
        } else {
            list_users.push(entry);
        }
    }
    let (mut exact_groups, mut list_groups) = (Vec::new(), Vec::new());
    for gid in groups {
        let entry = json!({"label": gid, "value": {"shareType": TYPE_GROUP, "shareWith": gid}});
        if !needle.is_empty() && gid.to_lowercase() == needle {
            exact_groups.push(entry);
        } else {
            list_groups.push(entry);
        }
    }
    ocs::respond(
        v,
        f,
        json!({
            "exact": {"users": exact_users, "groups": exact_groups, "remotes": [], "remote_groups": [],
                      "emails": [], "circles": [], "rooms": []},
            "users": list_users, "groups": list_groups, "remotes": [], "remote_groups": [],
            "emails": [], "lookup": [], "circles": [], "rooms": [], "lookupEnabled": false,
        }),
    )
}

/// Entry point for fuzzing the share API's parameter parser (query string + form/JSON body).
#[doc(hidden)]
pub fn fuzz_params(query: &str, json: bool, body: &[u8]) -> usize {
    let mut h = HeaderMap::new();
    if json {
        h.insert("content-type", "application/json".parse().unwrap());
    }
    params(Some(query), &h, body).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_are_validated_and_files_are_reduced() {
        let p = |s: &str| HashMap::from([("permissions".to_string(), s.to_string())]);
        assert_eq!(parse_perms(&HashMap::new(), 31, true), Ok(31));
        assert_eq!(parse_perms(&HashMap::new(), 19, false), Ok(19));
        assert_eq!(parse_perms(&p("1"), 31, true), Ok(1));
        assert_eq!(parse_perms(&p("31"), 19, false), Ok(19)); // create/delete make no sense on a file
        for bad in ["0", "32", "99", "x", "-1", "2"] {
            assert!(parse_perms(&p(bad), 31, true).is_err(), "{bad}");
        }
    }

    #[test]
    fn params_come_from_query_form_and_json() {
        let h = HeaderMap::new();
        let m = params(Some("a=1&b=x%20y"), &h, b"c=%2Fp&d=a+b");
        assert_eq!(
            (
                m["a"].as_str(),
                m["b"].as_str(),
                m["c"].as_str(),
                m["d"].as_str()
            ),
            ("1", "x y", "/p", "a b")
        );
        let mut jh = HeaderMap::new();
        jh.insert("content-type", "application/json".parse().unwrap());
        let m = params(None, &jh, br#"{"path":"/x","permissions":15,"note":null}"#);
        assert_eq!(
            (
                m["path"].as_str(),
                m["permissions"].as_str(),
                m.contains_key("note")
            ),
            ("/x", "15", false)
        );
    }
}
