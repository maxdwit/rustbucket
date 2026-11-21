//! WebDAV endpoints of the trash bin and of file versions, shaped like stock Nextcloud 35:
//!
//! * `/remote.php/dav/trashbin/{user}/trash[/{name}.d{time}[/...]]` - PROPFIND, GET, DELETE (one item /
//!   the whole bin), `MOVE` to `.../restore/{x}` restores.
//! * `/remote.php/dav/versions/{user}/versions/{fileid}[/{mtime}]` - PROPFIND, GET, DELETE, `MOVE` to
//!   `.../restore/{x}` makes that version the live file (the current content becomes a version).

#![allow(clippy::result_large_err)] // helpers return a ready-made error `Response`

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use percent_encoding::percent_decode_str;
use tokio_util::io::ReaderStream;

use crate::auth::AuthUser;
use crate::dav::{fs_error, internal, not_found, owner, sabre_error};
use crate::ocs::escape;
use crate::propfind::{self, NS_DAV, NS_NC, NS_OC, PropRequest};
use crate::routes::AppState;
use crate::storage::Node;
use crate::trash::TrashItem;

const ALLOW: &str = "OPTIONS, GET, HEAD, DELETE, PROPFIND, MOVE";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/remote.php/dav/trashbin/{user}", any(trashbin))
        .route("/remote.php/dav/trashbin/{user}/", any(trashbin))
        .route("/remote.php/dav/trashbin/{user}/{*rest}", any(trashbin))
        .route("/remote.php/dav/versions/{user}", any(versions))
        .route("/remote.php/dav/versions/{user}/", any(versions))
        .route("/remote.php/dav/versions/{user}/{*rest}", any(versions))
}

fn enc(s: &str) -> String {
    crate::paths::encode_segment(s)
}

enum PVal {
    Text(String),
    Empty,
    Raw(&'static str),
}

/// One `<d:response>`: the href and the properties this resource has.
struct Entry {
    href: String,
    props: Vec<(&'static str, &'static str, PVal)>,
}

impl Entry {
    fn collection(href: String) -> Self {
        Self {
            href,
            props: vec![(NS_DAV, "resourcetype", PVal::Raw("<d:collection/>"))],
        }
    }
}

fn tag(ns: &str, local: &str) -> String {
    let p = match ns {
        NS_DAV => "d",
        NS_OC => "oc",
        NS_NC => "nc",
        _ => "x",
    };
    format!("{p}:{local}")
}

fn render(entries: &[Entry], req: &PropRequest) -> Response {
    let mut out = String::new();
    for e in entries {
        out.push_str("<d:response><d:href>");
        escape(&e.href, &mut out);
        out.push_str("</d:href>");
        let (found, missing): (Vec<_>, Vec<_>) = match req {
            PropRequest::Props(p) => {
                let mut f = Vec::new();
                let mut m = Vec::new();
                for (ns, name) in p {
                    match e.props.iter().find(|(n, l, _)| n == ns && l == name) {
                        Some(x) => f.push((x.0, x.1, &x.2)),
                        None => m.push((ns.clone(), name.clone())),
                    }
                }
                (f, m)
            }
            _ => (
                e.props.iter().map(|x| (x.0, x.1, &x.2)).collect(),
                Vec::new(),
            ),
        };
        if !found.is_empty() || missing.is_empty() {
            out.push_str("<d:propstat><d:prop>");
            for (ns, name, v) in found {
                let t = tag(ns, name);
                match (req, v) {
                    (PropRequest::PropName, _) => out.push_str(&format!("<{t}/>")),
                    (_, PVal::Text(s)) => {
                        out.push_str(&format!("<{t}>"));
                        escape(s, &mut out);
                        out.push_str(&format!("</{t}>"));
                    }
                    (_, PVal::Empty) => out.push_str(&format!("<{t}/>")),
                    (_, PVal::Raw(x)) => out.push_str(&format!("<{t}>{x}</{t}>")),
                }
            }
            out.push_str("</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>");
        }
        if !missing.is_empty() {
            out.push_str("<d:propstat><d:prop>");
            for (ns, name) in missing {
                out.push_str(&format!("<{}/>", tag(&ns, &name)));
            }
            out.push_str("</d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>");
        }
        out.push_str("</d:response>");
    }
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        propfind::multistatus(&out),
    )
        .into_response()
}

fn parts_of(path: &str, prefix: &str) -> Vec<String> {
    path.strip_prefix(prefix)
        .unwrap_or("")
        .split('/')
        .filter(|p| !p.is_empty())
        .map(|p| percent_decode_str(p).decode_utf8_lossy().into_owned())
        .collect()
}

fn depth_of(req: &Request) -> u8 {
    match req
        .headers()
        .get("depth")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
    {
        Some("0") => 0,
        _ => 1, // infinity is treated like 1 here: these collections are shallow
    }
}

async fn prop_request(req: Request) -> Result<PropRequest, Response> {
    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
        .await
        .map_err(|_| {
            sabre_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Sabre\\DAV\\Exception\\BadRequest",
                "Request body too large",
            )
        })?;
    propfind::parse_request(&body).map_err(|_| {
        sabre_error(
            StatusCode::BAD_REQUEST,
            "Sabre\\DAV\\Exception\\BadRequest",
            "Invalid XML body",
        )
    })
}

fn options() -> Response {
    let mut r = StatusCode::OK.into_response();
    r.headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static(ALLOW));
    r
}

fn method_not_allowed() -> Response {
    let mut r = sabre_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "Sabre\\DAV\\Exception\\MethodNotAllowed",
        "Method not allowed",
    );
    r.headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static(ALLOW));
    r
}

/// `Destination:` of a restore: must be `<prefix>/{user}/restore/<anything>`.
fn restore_destination(req: &Request, prefix: &str, uid: &str) -> bool {
    let Some(raw) = req
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let path = match raw.find("://") {
        Some(i) => raw[i + 3..].find('/').map_or("", |j| &raw[i + 3 + j..]),
        None => raw,
    };
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let want = format!("{prefix}{}/restore/", enc(uid));
    let want_raw = format!("{prefix}{uid}/restore/");
    (path.starts_with(&want) || path.starts_with(&want_raw))
        && path.len() > want.len().min(want_raw.len())
}

async fn send_file(path: std::path::PathBuf, mime: &str, name: &str, head: bool) -> Response {
    let f = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return not_found(name),
        Err(e) => return internal(e),
    };
    let len = f.metadata().await.map_or(0, |m| m.len());
    let body = if head {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(f))
    };
    let mut r = (StatusCode::OK, body).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(if mime.is_empty() {
            "application/octet-stream"
        } else {
            mime
        })
        .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename*=UTF-8''{}", enc(name))) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    r
}

// ------------------------------------------------------------------ trash bin

fn item_entry(base: &str, it: &TrashItem, user: &AuthUser) -> Entry {
    let entry = it.entry_name();
    let mut props = vec![
        (NS_NC, "trashbin-filename", PVal::Text(it.name.clone())),
        (
            NS_NC,
            "trashbin-original-location",
            PVal::Text(it.original_location()),
        ),
        (
            NS_NC,
            "trashbin-deletion-time",
            PVal::Text(it.deleted_at.to_string()),
        ),
        (
            NS_NC,
            "trashbin-deleted-by-id",
            PVal::Text(user.uid.clone()),
        ),
        (
            NS_NC,
            "trashbin-deleted-by-display-name",
            PVal::Text(user.display_name.clone()),
        ),
        (NS_OC, "fileid", PVal::Text(it.orig_fileid.to_string())),
        (NS_NC, "has-preview", PVal::Text("false".into())),
        (
            NS_DAV,
            "getlastmodified",
            PVal::Text(propfind::http_date(it.deleted_at)),
        ), // stock: when it was deleted
        (NS_OC, "permissions", PVal::Text("GD".into())),
        (NS_OC, "size", PVal::Text(it.size.to_string())),
        (NS_DAV, "displayname", PVal::Text(it.name.clone())),
    ];
    if it.is_dir {
        props.push((NS_DAV, "resourcetype", PVal::Raw("<d:collection/>")));
        props.push((NS_DAV, "getetag", PVal::Text(it.deleted_at.to_string()))); // stock: the deletion time
    } else {
        props.push((NS_DAV, "resourcetype", PVal::Empty));
        props.push((NS_DAV, "getcontentlength", PVal::Text(it.size.to_string())));
        props.push((NS_DAV, "getcontenttype", PVal::Text(it.mimetype.clone())));
        props.push((NS_DAV, "getetag", PVal::Empty));
    }
    Entry {
        href: format!(
            "{base}trash/{}{}",
            enc(&entry),
            if it.is_dir { "/" } else { "" }
        ),
        props,
    }
}

/// An item inside a trashed folder: same properties as the entry itself (stock shows the folder's
/// deletion time and the original location of the item).
fn inner_entry(
    item: &TrashItem,
    user: &AuthUser,
    base: &str,
    rel: &str,
    name: &str,
    is_dir: bool,
    size: i64,
) -> Entry {
    let href = format!(
        "{base}{}{}",
        rel_join(rel, name),
        if is_dir { "/" } else { "" }
    );
    let below = if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    };
    let mut props = vec![
        (NS_NC, "trashbin-filename", PVal::Text(name.to_string())),
        (
            NS_NC,
            "trashbin-original-location",
            PVal::Text(format!("{}/{below}", item.original_location())),
        ),
        (
            NS_NC,
            "trashbin-deletion-time",
            PVal::Text(item.deleted_at.to_string()),
        ),
        (
            NS_NC,
            "trashbin-deleted-by-id",
            PVal::Text(user.uid.clone()),
        ),
        (
            NS_NC,
            "trashbin-deleted-by-display-name",
            PVal::Text(user.display_name.clone()),
        ),
        (NS_NC, "has-preview", PVal::Text("false".into())),
        (NS_OC, "fileid", PVal::Text(item.orig_fileid.to_string())), // not indexed: the entry\'s id stands in
        (NS_DAV, "displayname", PVal::Text(name.to_string())),
        (
            NS_DAV,
            "getlastmodified",
            PVal::Text(propfind::http_date(item.deleted_at)),
        ),
        (NS_OC, "permissions", PVal::Text("GD".into())),
        (NS_OC, "size", PVal::Text(size.to_string())),
    ];
    if is_dir {
        props.push((NS_DAV, "resourcetype", PVal::Raw("<d:collection/>")));
        props.push((NS_DAV, "getetag", PVal::Text(item.deleted_at.to_string())));
    } else {
        props.push((NS_DAV, "resourcetype", PVal::Empty));
        props.push((NS_DAV, "getcontentlength", PVal::Text(size.to_string())));
        props.push((
            NS_DAV,
            "getcontenttype",
            PVal::Text(crate::storage::mime_for(name)),
        ));
        props.push((NS_DAV, "getetag", PVal::Empty));
    }
    Entry { href, props }
}

fn rel_join(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        enc(name)
    } else {
        format!(
            "{}/{}",
            rel.split('/').map(enc).collect::<Vec<_>>().join("/"),
            enc(name)
        )
    }
}

async fn trashbin(State(st): State<AppState>, user: AuthUser, req: Request) -> Response {
    const PREFIX: &str = "/remote.php/dav/trashbin/";
    let parts = parts_of(req.uri().path(), PREFIX);
    if parts.first().map(String::as_str) != Some(user.uid.as_str()) {
        return not_found("trashbin");
    }
    let me = owner(&user);
    let base = format!("{PREFIX}{}/", enc(&user.uid));
    let method = req.method().as_str().to_string();
    match (method.as_str(), parts.get(1).map(String::as_str)) {
        ("OPTIONS", _) => options(),
        ("PROPFIND", None) => {
            let depth = depth_of(&req);
            let preq = match prop_request(req).await {
                Ok(p) => p,
                Err(r) => return r,
            };
            let mut e = vec![Entry::collection(base.clone())];
            if depth > 0 {
                e.push(Entry::collection(format!("{base}restore/")));
                e.push(Entry::collection(format!("{base}trash/")));
            }
            render(&e, &preq)
        }
        ("PROPFIND", Some("restore")) if parts.len() == 2 => {
            let preq = match prop_request(req).await {
                Ok(p) => p,
                Err(r) => return r,
            };
            render(&[Entry::collection(format!("{base}restore/"))], &preq)
        }
        ("PROPFIND", Some("trash")) => {
            let depth = depth_of(&req);
            let preq = match prop_request(req).await {
                Ok(p) => p,
                Err(r) => return r,
            };
            if parts.len() == 2 {
                let mut e = vec![Entry::collection(format!("{base}trash/"))];
                if depth > 0 {
                    match st.store.trash_list(&me).await {
                        Ok(items) => e.extend(items.iter().map(|i| item_entry(&base, i, &user))),
                        Err(err) => return internal(err),
                    }
                }
                return render(&e, &preq);
            }
            let item = match st.store.trash_find(&me, &parts[2]).await {
                Ok(Some(i)) => i,
                Ok(None) => return not_found(&parts[2]),
                Err(e) => return internal(e),
            };
            let rel = safe_rel(&parts[3..]);
            let Some(rel) = rel else {
                return not_found(&parts[2]);
            };
            let mut e = Vec::new();
            if rel.is_empty() {
                e.push(item_entry(&base, &item, &user));
            }
            if (item.is_dir || !rel.is_empty()) && (depth > 0 || !rel.is_empty()) {
                let here = under(st.store.trash_item_path(&me, &item), &rel);
                let md = match tokio::fs::metadata(&here).await {
                    Ok(m) => m,
                    Err(_) => return not_found(&parts[2]),
                };
                let href_base = format!("{base}trash/{}/", enc(&parts[2]));
                if !rel.is_empty() {
                    let name = rel.rsplit('/').next().unwrap_or("");
                    let up = rel.rsplit_once('/').map_or("", |x| x.0);
                    e.push(inner_entry(
                        &item,
                        &user,
                        &href_base,
                        up,
                        name,
                        md.is_dir(),
                        if md.is_dir() {
                            crate::trash::dir_size(&here)
                        } else {
                            md.len() as i64
                        },
                    ));
                }
                if md.is_dir() && depth > 0 {
                    match st.store.trash_children(&me, &item, &rel).await {
                        Ok(kids) => e.extend(kids.iter().map(|(n, d, s, _)| {
                            inner_entry(&item, &user, &href_base, &rel, n, *d, *s)
                        })),
                        Err(err) => return internal(err),
                    }
                }
            }
            render(&e, &preq)
        }
        ("GET" | "HEAD", Some("trash")) if parts.len() >= 3 => {
            let item = match st.store.trash_find(&me, &parts[2]).await {
                Ok(Some(i)) => i,
                Ok(None) => return not_found(&parts[2]),
                Err(e) => return internal(e),
            };
            let Some(rel) = safe_rel(&parts[3..]) else {
                return not_found(&parts[2]);
            };
            let path = under(st.store.trash_item_path(&me, &item), &rel);
            if tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_dir()) {
                return sabre_error(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "Sabre\\DAV\\Exception\\MethodNotAllowed",
                    "Cannot GET a collection",
                );
            }
            let name = if rel.is_empty() {
                item.name.clone()
            } else {
                rel.rsplit('/').next().unwrap_or("").to_string()
            };
            send_file(
                path,
                &crate::storage::mime_for(&name),
                &name,
                method == "HEAD",
            )
            .await
        }
        ("DELETE", Some("trash")) if parts.len() == 2 => match st.store.trash_empty(&me).await {
            Ok(_) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => internal(e),
        },
        ("DELETE", Some("trash")) if parts.len() == 3 => {
            match st.store.trash_find(&me, &parts[2]).await {
                Ok(Some(i)) => match st.store.trash_purge(&me, &i).await {
                    Ok(()) => StatusCode::NO_CONTENT.into_response(),
                    Err(e) => internal(e),
                },
                Ok(None) => not_found(&parts[2]),
                Err(e) => internal(e),
            }
        }
        ("DELETE", Some("trash")) => sabre_error(
            StatusCode::FORBIDDEN,
            "Sabre\\DAV\\Exception\\Forbidden",
            "Items inside a trashed folder cannot be deleted on their own",
        ),
        ("MOVE", Some("trash")) if parts.len() == 3 => {
            // `.../restore/<x>` puts the item back where it was; a path in the files tree restores it there
            let to = if restore_destination(&req, PREFIX, &user.uid) {
                None
            } else {
                match crate::dav::destination(req.headers(), &user) {
                    Ok(p) if !p.is_empty() => Some(p),
                    _ => return not_found(&parts[2]),
                }
            };
            let item = match st.store.trash_find(&me, &parts[2]).await {
                Ok(Some(i)) => i,
                Ok(None) => return not_found(&parts[2]),
                Err(e) => return internal(e),
            };
            match st.store.trash_restore(&me, &item, to.as_deref()).await {
                Ok(_) => StatusCode::CREATED.into_response(),
                Err(crate::storage::FsError::ParentNotFound) => sabre_error(
                    StatusCode::CONFLICT,
                    "Sabre\\DAV\\Exception\\Conflict",
                    "Destination node does not exist",
                ),
                Err(e) => fs_error(e, &parts[2]),
            }
        }
        _ => method_not_allowed(),
    }
}

/// `base` or `base/rel` (an empty `rel` must not add a trailing slash: that breaks opening files).
fn under(base: std::path::PathBuf, rel: &str) -> std::path::PathBuf {
    if rel.is_empty() { base } else { base.join(rel) }
}

/// Relative path below a trashed folder; `None` if it tries to leave it.
fn safe_rel(parts: &[String]) -> Option<String> {
    let mut out = Vec::new();
    for p in parts {
        if p.is_empty() || p == "." || p == ".." || p.contains('/') || p.contains('\0') {
            return None;
        }
        out.push(p.as_str());
    }
    Some(out.join("/"))
}

// ------------------------------------------------------------------ versions

fn version_entry(base: &str, node: &Node, v: &crate::trash::VersionInfo, uid: &str) -> Entry {
    Entry {
        href: format!("{base}versions/{}/{}", node.fileid, v.ts),
        props: vec![
            (NS_DAV, "getcontentlength", PVal::Text(v.size.to_string())),
            (NS_DAV, "getcontenttype", PVal::Text(node.mimetype.clone())),
            (
                NS_DAV,
                "getlastmodified",
                PVal::Text(propfind::http_date(v.ts)),
            ),
            (NS_DAV, "getetag", PVal::Text(v.ts.to_string())),
            (NS_DAV, "resourcetype", PVal::Empty),
            (NS_NC, "version-author", PVal::Text(uid.to_string())),
        ],
    }
}

async fn versions(State(st): State<AppState>, user: AuthUser, req: Request) -> Response {
    const PREFIX: &str = "/remote.php/dav/versions/";
    let parts = parts_of(req.uri().path(), PREFIX);
    if parts.first().map(String::as_str) != Some(user.uid.as_str()) {
        return not_found("versions");
    }
    let me = owner(&user);
    let base = format!("{PREFIX}{}/", enc(&user.uid));
    let method = req.method().as_str().to_string();
    let section = parts.get(1).map(String::as_str);
    // everything below versions/<fileid> needs the file; versions are kept per owner
    let file = if section == Some("versions") && parts.len() >= 3 {
        let Ok(id) = parts[2].parse::<i64>() else {
            return not_found(&parts[2]);
        };
        match st.store.get_by_id(&me, id).await {
            Ok(Some(n)) if !n.is_dir => Some(n),
            Ok(_) => return not_found(&parts[2]),
            Err(e) => return internal(e),
        }
    } else {
        None
    };
    match (method.as_str(), section, &file) {
        ("OPTIONS", _, _) => options(),
        ("PROPFIND", None, _) => {
            let depth = depth_of(&req);
            let preq = match prop_request(req).await {
                Ok(p) => p,
                Err(r) => return r,
            };
            let mut e = vec![Entry::collection(base.clone())];
            if depth > 0 {
                e.push(Entry::collection(format!("{base}versions/")));
                e.push(Entry::collection(format!("{base}restore/")));
            }
            render(&e, &preq)
        }
        ("PROPFIND", Some("versions" | "restore"), None) if parts.len() == 2 => {
            let preq = match prop_request(req).await {
                Ok(p) => p,
                Err(r) => return r,
            };
            render(&[Entry::collection(format!("{base}{}/", parts[1]))], &preq)
        }
        ("PROPFIND", Some("versions"), Some(node)) if parts.len() == 3 => {
            let depth = depth_of(&req);
            let preq = match prop_request(req).await {
                Ok(p) => p,
                Err(r) => return r,
            };
            let mut e = vec![Entry::collection(format!(
                "{base}versions/{}/",
                node.fileid
            ))];
            if depth > 0 {
                match st.store.versions_list(&me, node).await {
                    Ok(list) => e.extend(
                        list.iter()
                            .map(|v| version_entry(&base, node, v, &user.uid)),
                    ),
                    Err(err) => return internal(err),
                }
            }
            render(&e, &preq)
        }
        (m, Some("versions"), Some(node)) if parts.len() == 4 => {
            let Ok(ts) = parts[3].parse::<i64>() else {
                return not_found(&parts[3]);
            };
            let list = match st.store.versions_list(&me, node).await {
                Ok(l) => l,
                Err(e) => return internal(e),
            };
            let Some(v) = list.iter().find(|v| v.ts == ts) else {
                return not_found(&parts[3]);
            };
            match m {
                "PROPFIND" => {
                    let preq = match prop_request(req).await {
                        Ok(p) => p,
                        Err(r) => return r,
                    };
                    render(&[version_entry(&base, node, v, &user.uid)], &preq)
                }
                "GET" | "HEAD" => {
                    send_file(
                        st.store.version_file(&me, node, ts),
                        &node.mimetype,
                        &node.name,
                        m == "HEAD",
                    )
                    .await
                }
                "DELETE" => match st.store.version_delete(&me, node, ts).await {
                    Ok(()) => StatusCode::NO_CONTENT.into_response(),
                    Err(e) => fs_error(e, &parts[3]),
                },
                "MOVE" => {
                    if !restore_destination(&req, PREFIX, &user.uid) {
                        return not_found(&parts[3]);
                    }
                    match st.store.version_restore(&me, node, ts).await {
                        Ok(_) => StatusCode::CREATED.into_response(),
                        Err(e) => fs_error(e, &parts[3]),
                    }
                }
                _ => method_not_allowed(),
            }
        }
        _ => method_not_allowed(),
    }
}
