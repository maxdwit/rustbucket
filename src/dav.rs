//! WebDAV file endpoints: `/remote.php/dav/files/{user}/…` and the `/remote.php/webdav/…` alias.
//! Behaviour (status codes, headers) follows recordings of stock Nextcloud 35, see
//! `tests/fixtures/nc35/`.

use std::io::SeekFrom;
use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use http_body_util::BodyExt;
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

use crate::auth::{AuthUser, unix_now};
use crate::ocs::escape;
use crate::paths::{self, InvalidPath};
use crate::propfind::{self, Ctx, PropRequest};
use crate::routes::AppState;
use crate::shares::{self, Resolved, dav_letters};
use crate::storage::{FsError, Node, Owner, PutOpts};
use crate::view::ViewCtx;

const ALLOW: &str =
    "OPTIONS, GET, HEAD, DELETE, PROPFIND, PUT, PROPPATCH, COPY, MOVE, MKCOL, REPORT, LOCK, UNLOCK";
const DAV_CAPS: &str = "1, 3, extended-mkcol, nextcloud-checksum-update, 2";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/remote.php/dav/files/{user}", any(dav))
        .route("/remote.php/dav/files/{user}/", any(dav))
        .route("/remote.php/dav/files/{user}/{*rest}", any(dav))
        .route("/remote.php/dav", any(crate::search::search))
        .route("/remote.php/dav/", any(crate::search::search))
        .route("/remote.php/webdav", any(dav))
        .route("/remote.php/webdav/", any(dav))
        .route("/remote.php/webdav/{*rest}", any(dav))
}

pub(crate) fn sabre_error(status: StatusCode, exception: &str, message: &str) -> Response {
    let mut m = String::new();
    escape(message, &mut m);
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:error xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\">\n  \
         <s:exception>{exception}</s:exception>\n  <s:message>{m}</s:message>\n</d:error>"
    );
    (
        status,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        body,
    )
        .into_response()
}

pub(crate) fn too_large(limit: u64) -> Response {
    sabre_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Sabre\\DAV\\Exception\\PayloadTooLarge",
        &format!("File is larger than the server limit of {limit} bytes"),
    )
}

pub(crate) fn not_found(path: &str) -> Response {
    sabre_error(
        StatusCode::NOT_FOUND,
        "Sabre\\DAV\\Exception\\NotFound",
        &format!("File with name /{path} could not be located"),
    )
}

pub(crate) fn internal(e: impl std::fmt::Debug) -> Response {
    tracing::error!(error = ?e, "dav request failed");
    sabre_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Sabre\\DAV\\Exception",
        "Internal server error",
    )
}

pub(crate) fn fs_error(e: FsError, path: &str) -> Response {
    match e {
        FsError::NotFound => not_found(path),
        FsError::ParentNotFound => not_found(paths::split(path).0),
        FsError::NotCollection => sabre_error(
            StatusCode::CONFLICT,
            "Sabre\\DAV\\Exception\\Conflict",
            "Parent node is not a collection",
        ),
        FsError::Exists => {
            let mut r = sabre_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "Sabre\\DAV\\Exception\\MethodNotAllowed",
                "The resource you tried to create already exists",
            );
            r.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static(ALLOW));
            r
        }
        FsError::IsCollection => sabre_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Sabre\\DAV\\Exception\\MethodNotAllowed",
            "Target is a collection",
        ),
        FsError::PreconditionFailed => sabre_error(
            StatusCode::PRECONDITION_FAILED,
            "Sabre\\DAV\\Exception\\PreconditionFailed",
            "Precondition failed",
        ),
        FsError::Forbidden(m) => {
            sabre_error(StatusCode::FORBIDDEN, "Sabre\\DAV\\Exception\\Forbidden", m)
        }
        FsError::Db(e) => internal(e),
        FsError::Io(e) if e.kind() == std::io::ErrorKind::InvalidFilename => sabre_error(
            StatusCode::BAD_REQUEST,
            "Sabre\\DAV\\Exception\\BadRequest",
            "File name too long or invalid for the filesystem",
        ),
        FsError::Io(e) => internal(e),
        FsError::DataFolder(m) => internal(m),
    }
}

pub(crate) fn set(r: &mut Response, name: &'static str, v: &str) {
    if let Ok(v) = HeaderValue::from_str(v) {
        r.headers_mut().insert(HeaderName::from_static(name), v);
    }
}

pub(crate) fn quoted(etag: &str) -> String {
    format!("\"{etag}\"")
}

pub(crate) fn node_headers(r: &mut Response, n: &Node, instance: &str, owner: &str, perms: &str) {
    set(r, "oc-fileid", &propfind::file_id(instance, n.fileid));
    set(r, "etag", &quoted(&n.etag));
    set(r, "oc-etag", &quoted(&n.etag));
    set(r, "x-nc-ownerid", owner);
    set(r, "x-nc-permissions", perms);
}

pub(crate) fn forbidden() -> Response {
    sabre_error(
        StatusCode::FORBIDDEN,
        "Sabre\\DAV\\Exception\\Forbidden",
        "",
    )
}

/// Permission letters for a node reached through a resolution (own tree or a share).
pub(crate) fn letters_for(r: &Resolved, n: &Node) -> String {
    match &r.mount {
        None => propfind::permissions(n).to_string(),
        Some(m) => dav_letters(n.is_dir, r.perms, true, n.fileid == m.source.fileid),
    }
}

/// Resolve a path of the user's tree (see `shares`).
#[allow(clippy::result_large_err)]
pub(crate) async fn resolve(
    st: &AppState,
    user: &AuthUser,
    path: &str,
) -> Result<Resolved, Response> {
    st.store.resolve(&owner(user), path).await.map_err(internal)
}

/// Where a request path lives: href prefix and the path below the user root.
struct Target {
    prefix: String,
    path: String,
}

#[derive(Debug)]
enum TargetError {
    WrongUser,
    Invalid,
}

fn target(raw_path: &str, user: &AuthUser) -> Result<Target, TargetError> {
    if let Some(rest) = raw_path.strip_prefix("/remote.php/webdav") {
        let path = paths::normalize(rest).map_err(|_: InvalidPath| TargetError::Invalid)?;
        return Ok(Target {
            prefix: "/remote.php/webdav".into(),
            path,
        });
    }
    let rest = raw_path
        .strip_prefix("/remote.php/dav/files/")
        .ok_or(TargetError::Invalid)?;
    let (u, rel) = rest.split_once('/').unwrap_or((rest, ""));
    if percent_decode_str(u).decode_utf8().ok().as_deref() != Some(user.uid.as_str()) {
        return Err(TargetError::WrongUser);
    }
    let path = paths::normalize(rel).map_err(|_| TargetError::Invalid)?;
    Ok(Target {
        prefix: format!("/remote.php/dav/files/{u}"),
        path,
    })
}

/// `Destination:` header (absolute URL or path) -> normalised path inside the user's tree.
#[allow(clippy::result_large_err)]
pub(crate) fn destination(headers: &HeaderMap, user: &AuthUser) -> Result<String, Response> {
    let bad = || {
        sabre_error(
            StatusCode::BAD_REQUEST,
            "Sabre\\DAV\\Exception\\BadRequest",
            "Invalid Destination header",
        )
    };
    let raw = headers
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(bad)?;
    let path = match raw.find("://") {
        Some(i) => {
            let after = &raw[i + 3..];
            &after[after.find('/').ok_or_else(bad)?..]
        }
        None => raw,
    };
    let path = path.split(['?', '#']).next().unwrap_or(path);
    match target(path, user) {
        Ok(t) => Ok(t.path),
        Err(_) => Err(sabre_error(
            StatusCode::BAD_GATEWAY,
            "Sabre\\DAV\\Exception\\BadGateway",
            "Destination is not on this server/user",
        )),
    }
}

async fn dav(State(st): State<AppState>, user: AuthUser, req: Request) -> Response {
    let tgt = match target(req.uri().path(), &user) {
        Ok(t) => t,
        Err(TargetError::WrongUser) => {
            return sabre_error(
                StatusCode::NOT_FOUND,
                "Sabre\\DAV\\Exception\\NotFound",
                "Principal not found",
            );
        }
        Err(TargetError::Invalid) => {
            return sabre_error(
                StatusCode::BAD_REQUEST,
                "Sabre\\DAV\\Exception\\BadRequest",
                "Invalid path",
            );
        }
    };
    let mut resp = match req.method().as_str() {
        "OPTIONS" => options(),
        "PROPFIND" => propfind(&st, &user, &tgt, req).await,
        "GET" => get(&st, &user, &tgt, req, false).await,
        "HEAD" => get(&st, &user, &tgt, req, true).await,
        "PUT" => put(&st, &user, &tgt, req).await,
        "MKCOL" => mkcol(&st, &user, &tgt).await,
        "LOCK" => lock(&st, &user, &tgt, req).await,
        "UNLOCK" => StatusCode::NO_CONTENT.into_response(),
        "PROPPATCH" => proppatch(&user, &tgt, req).await,
        "REPORT" => match axum::body::to_bytes(req.into_body(), 1 << 20).await {
            Ok(b) => crate::search::report(&b).await,
            Err(_) => sabre_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Sabre\\DAV\\Exception\\BadRequest",
                "Request body too large",
            ),
        },
        "DELETE" => delete(&st, &user, &tgt).await,
        "MOVE" | "COPY" => move_copy(&st, &user, &tgt, req).await,
        _ => {
            let mut r = sabre_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "Sabre\\DAV\\Exception\\MethodNotAllowed",
                "Method not allowed",
            );
            r.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static(ALLOW));
            r
        }
    };
    resp.headers_mut().insert(
        HeaderName::from_static("dav"),
        HeaderValue::from_static(DAV_CAPS),
    );
    resp
}

pub(crate) fn owner(u: &AuthUser) -> Owner {
    Owner {
        id: u.id,
        uid: u.uid.clone(),
    }
}

fn options() -> Response {
    let mut r = StatusCode::OK.into_response();
    let h = r.headers_mut();
    h.insert(header::ALLOW, HeaderValue::from_static(ALLOW));
    h.insert(
        HeaderName::from_static("ms-author-via"),
        HeaderValue::from_static("DAV"),
    );
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    r
}

async fn propfind(st: &AppState, user: &AuthUser, t: &Target, req: Request) -> Response {
    let depth = req
        .headers()
        .get("depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("infinity")
        .trim()
        .to_ascii_lowercase();
    let body = match axum::body::to_bytes(req.into_body(), 1 << 20).await {
        Ok(b) => b,
        Err(_) => {
            return sabre_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Sabre\\DAV\\Exception\\BadRequest",
                "Request body too large",
            );
        }
    };
    let preq: PropRequest = match propfind::parse_request(&body) {
        Ok(p) => p,
        Err(_) => {
            return sabre_error(
                StatusCode::BAD_REQUEST,
                "Sabre\\DAV\\Exception\\BadRequest",
                "Invalid XML body",
            );
        }
    };
    let me = owner(user);
    let vc = match ViewCtx::new(&st.store, &me, &user.display_name).await {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let r = vc.resolve(&t.path);
    let node = match st.store.get(&r.owner, &r.path).await {
        Ok(Some(n)) => n,
        Ok(None) => return not_found(&t.path),
        Err(e) => return internal(e),
    };
    let mut first = vc.item(&r, node.clone());
    if r.is_mount_root
        && let Some(m) = &r.mount
    {
        first.extra.shares = vec![vc.viewer_share(m)];
    }
    let mut items = vec![first];
    if node.is_dir && depth != "0" {
        let more = if depth == "1" {
            vc.children(&r, &node).await
        } else {
            vc.descendants(&r, &node).await
        };
        match more {
            Ok(mut k) => items.append(&mut k),
            Err(e) => return internal(e),
        }
    }
    let instance = match st.store.instance_id().await {
        Ok(i) => i,
        Err(e) => return internal(e),
    };
    let ctx = Ctx {
        prefix: &t.prefix,
        instance_id: &instance,
        owner_uid: &user.uid,
        owner_display: &user.display_name,
    };
    let mut out = String::new();
    for it in &items {
        propfind::render_response(&mut out, &it.node, &ctx, &preq, &it.extra);
    }
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        propfind::multistatus(&out),
    )
        .into_response()
}

/// `Range: bytes=a-b` for a file of `size` bytes -> inclusive (start, end). None = no/invalid range.
fn parse_range(h: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = h.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multiple ranges: serve the whole file
    }
    let (a, b) = spec.split_once('-')?;
    let r = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 || size == 0 {
            Err(())
        } else {
            Ok((size.saturating_sub(n), size - 1))
        }
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() {
            size.saturating_sub(1)
        } else {
            b.parse::<u64>().ok()?.min(size.saturating_sub(1))
        };
        if start >= size || start > end {
            Err(())
        } else {
            Ok((start, end))
        }
    };
    Some(r)
}

async fn get(st: &AppState, user: &AuthUser, t: &Target, req: Request, head: bool) -> Response {
    let me = owner(user);
    let mounts = match st.store.mounts(&me).await {
        Ok(m) => m,
        Err(e) => return internal(e),
    };
    let res = shares::resolve_in(&mounts, &me, &t.path);
    let node = match st.store.get(&res.owner, &res.path).await {
        Ok(Some(n)) => n,
        Ok(None) => return not_found(&t.path),
        Err(e) => return internal(e),
    };
    if node.is_dir && head {
        // Sabre answers HEAD on a collection with 200 (the Windows client probes the DAV root this way)
        let etag = shares::dir_etag(&node, &t.path, &mounts).unwrap_or_else(|| node.etag.clone());
        let mut r = StatusCode::OK.into_response();
        set(&mut r, "etag", &quoted(&etag));
        set(&mut r, "oc-etag", &quoted(&etag));
        set(&mut r, "last-modified", &propfind::http_date(node.mtime));
        return r;
    }
    if node.is_dir {
        return sabre_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Sabre\\DAV\\Exception\\MethodNotAllowed",
            "Cannot GET a collection",
        );
    }
    let size = node.size as u64;
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| parse_range(h, size));
    let (status, start, len) = match range {
        Some(Err(())) => {
            let mut r = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            set(&mut r, "content-range", &format!("bytes */{size}"));
            return r;
        }
        Some(Ok((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b - a + 1),
        None => (StatusCode::OK, 0, size),
    };
    let body = if head || len == 0 {
        Body::empty()
    } else {
        let path: PathBuf = st.store.fs_path(&res.owner, &node.path);
        let mut f = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %path.display(), "indexed file is missing on disk (run `rustbucket scan`)");
                return not_found(&t.path);
            }
            Err(e) => return internal(e),
        };
        if start > 0 && f.seek(SeekFrom::Start(start)).await.is_err() {
            return internal("seek failed");
        }
        Body::from_stream(ReaderStream::new(f.take(len)))
    };
    let mut r = (status, body).into_response();
    let h = r.headers_mut();
    let ct = if node.mimetype.is_empty() {
        "application/octet-stream"
    } else {
        &node.mimetype
    };
    if let Ok(v) = HeaderValue::from_str(ct) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if status == StatusCode::PARTIAL_CONTENT {
        set(
            &mut r,
            "content-range",
            &format!("bytes {start}-{}/{size}", start + len - 1),
        );
    }
    set(&mut r, "etag", &quoted(&node.etag));
    set(&mut r, "oc-etag", &quoted(&node.etag));
    set(&mut r, "last-modified", &propfind::http_date(node.mtime));
    let enc = utf8_percent_encode(&node.name, NON_ALPHANUMERIC).to_string();
    let ascii: String = node
        .name
        .chars()
        .map(|c| {
            if c.is_ascii() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    set(
        &mut r,
        "content-disposition",
        &format!("attachment; filename*=UTF-8''{enc}; filename=\"{ascii}\""),
    );
    r
}

#[allow(clippy::result_large_err)]
pub(crate) enum UploadError {
    Response(Response),
}

/// Stream the body into a temp file; returns (tmp path, size, sha1 hex).
#[allow(clippy::result_large_err)]
pub(crate) async fn receive_body(
    st: &AppState,
    o: &Owner,
    body: Body,
) -> Result<(PathBuf, i64, String), UploadError> {
    let (tmp, mut file) = st
        .store
        .new_tmp(o)
        .await
        .map_err(|e| UploadError::Response(internal(e)))?;
    let cleanup = |tmp: PathBuf| async move {
        let _ = tokio::fs::remove_file(&tmp).await;
    };
    let mut body = body;
    let limit = st.config.max_file_size;
    let mut size = 0i64;
    let mut sha = Sha1::new();
    loop {
        match body.frame().await {
            None => break,
            Some(Ok(frame)) => {
                if let Some(d) = frame.data_ref() {
                    if let Err(e) = file.write_all(d).await {
                        cleanup(tmp).await;
                        return Err(UploadError::Response(internal(e)));
                    }
                    sha.update(d);
                    size += d.len() as i64;
                    if !st.config.allows_size(size as u64) {
                        cleanup(tmp).await;
                        return Err(UploadError::Response(too_large(limit)));
                    }
                }
            }
            Some(Err(e)) => {
                cleanup(tmp).await;
                tracing::warn!(error = %e, "upload body aborted");
                return Err(UploadError::Response(sabre_error(
                    StatusCode::BAD_REQUEST,
                    "Sabre\\DAV\\Exception\\BadRequest",
                    "Upload aborted",
                )));
            }
        }
    }
    if let Err(e) = file.flush().await.and(file.sync_all().await) {
        cleanup(tmp).await;
        return Err(UploadError::Response(internal(e)));
    }
    let hex: String = sha.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((tmp, size, hex))
}

/// Options of a PUT (or of the MOVE that finishes a chunked upload) from the request headers.
/// Returns the options and whether the client sent a usable `X-OC-Mtime`.
pub(crate) fn put_opts(
    headers: &HeaderMap,
    path: &str,
    checksum: Option<String>,
) -> (PutOpts, bool) {
    let mtime_hdr = headers
        .get("x-oc-mtime")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|m| *m > 0);
    let name = paths::split(path).1;
    let opts = PutOpts {
        mtime: mtime_hdr.unwrap_or_else(unix_now),
        mimetype: mime_guess::from_path(name)
            .first_or_octet_stream()
            .essence_str()
            .to_string(),
        checksum,
        if_match: headers
            .get(header::IF_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        if_none_match_star: headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            == Some("*"),
    };
    (opts, mtime_hdr.is_some())
}

async fn put(st: &AppState, user: &AuthUser, t: &Target, req: Request) -> Response {
    if t.path.is_empty() {
        return fs_error(FsError::IsCollection, &t.path);
    }
    if req.headers().contains_key("oc-chunked") {
        return sabre_error(
            StatusCode::NOT_IMPLEMENTED,
            "Sabre\\DAV\\Exception\\NotImplemented",
            "Legacy (v1) chunked upload is not supported, use /remote.php/dav/uploads",
        );
    }
    let res = match resolve(st, user, &t.path).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    // overwriting needs "update", creating needs "create" (checked before the body is read)
    let exists = match st.store.get(&res.owner, &res.path).await {
        Ok(n) => n.is_some(),
        Err(e) => return internal(e),
    };
    if !res.can(if exists {
        shares::UPDATE
    } else {
        shares::CREATE
    }) {
        return forbidden();
    }
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| !st.config.allows_size(n)) {
        return too_large(st.config.max_file_size);
    }
    let headers = req.headers().clone();
    let (tmp, size, sha1) = match receive_body(st, &owner(user), req.into_body()).await {
        Ok(x) => x,
        Err(UploadError::Response(r)) => return r,
    };
    let checksum = headers
        .get("oc-checksum")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(c) = &checksum
        && let Some(("SHA1" | "sha1", want)) = c.split_once(':')
        && !want.eq_ignore_ascii_case(&sha1)
    {
        let _ = tokio::fs::remove_file(&tmp).await;
        return sabre_error(
            StatusCode::BAD_REQUEST,
            "Sabre\\DAV\\Exception\\BadRequest",
            "The computed checksum does not match the one received from the client.",
        );
    }
    let (opts, mtime_hdr) = put_opts(&headers, &res.path, checksum);
    let instance = match st.store.instance_id().await {
        Ok(i) => i,
        Err(e) => return internal(e),
    };
    match st.store.put(&res.owner, &res.path, &tmp, size, opts).await {
        Ok((node, created)) => {
            let mut r = (if created {
                StatusCode::CREATED
            } else {
                StatusCode::NO_CONTENT
            })
            .into_response();
            if mtime_hdr {
                set(&mut r, "x-oc-mtime", "accepted");
            }
            node_headers(
                &mut r,
                &node,
                &instance,
                &res.owner.uid,
                &letters_for(&res, &node),
            );
            r
        }
        Err(FsError::ParentNotFound) => not_found(paths::split(&t.path).0),
        Err(e) => fs_error(e, &t.path),
    }
}

/// LOCK is a stub: generic WebDAV clients (Windows/macOS mounts, davfs2) insist on class 2 before
/// they write, but nothing here enforces locks - uploads are atomic and last-writer-wins with
/// If-Match for the real sync clients. Locking an unmapped URL creates an empty file (RFC 4918 7.3).
async fn lock(st: &AppState, user: &AuthUser, t: &Target, req: Request) -> Response {
    if t.path.is_empty() {
        return fs_error(FsError::IsCollection, &t.path);
    }
    let res = match resolve(st, user, &t.path).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let mut status = StatusCode::OK;
    match st.store.get(&res.owner, &res.path).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            if !res.can(shares::CREATE) {
                return forbidden();
            }
            let (tmp, mut f) = match st.store.new_tmp(&owner(user)).await {
                Ok(x) => x,
                Err(e) => return internal(e),
            };
            let _ = f.flush().await;
            let (opts, _) = put_opts(req.headers(), &res.path, None);
            match st.store.put(&res.owner, &res.path, &tmp, 0, opts).await {
                Ok(_) => status = StatusCode::CREATED,
                Err(FsError::ParentNotFound) => return not_found(paths::split(&t.path).0),
                Err(e) => return fs_error(e, &t.path),
            }
        }
        Err(e) => return internal(e),
    }
    let token = format!("opaquelocktoken:{}", crate::auth::random_token(16));
    let mut root = String::new();
    escape(&paths::href(&t.prefix, &t.path, false), &mut root);
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:prop xmlns:d=\"DAV:\"><d:lockdiscovery><d:activelock>\
         <d:lockscope><d:exclusive/></d:lockscope><d:locktype><d:write/></d:locktype><d:depth>infinity</d:depth>\
         <d:timeout>Second-3600</d:timeout><d:locktoken><d:href>{token}</d:href></d:locktoken>\
         <d:lockroot><d:href>{root}</d:href></d:lockroot></d:activelock></d:lockdiscovery></d:prop>"
    );
    let mut r = (
        status,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        body,
    )
        .into_response();
    set(&mut r, "lock-token", &format!("<{token}>"));
    r
}

/// PROPPATCH is a stub: nothing is writable (no favourites/tags yet), every property is refused
/// with 403 in a multistatus, which is what a read-only property store answers.
async fn proppatch(user: &AuthUser, t: &Target, req: Request) -> Response {
    let Ok(body) = axum::body::to_bytes(req.into_body(), 1 << 20).await else {
        return sabre_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Sabre\\DAV\\Exception\\BadRequest",
            "Request body too large",
        );
    };
    let props = match propfind::parse_request(&body) {
        Ok(PropRequest::Props(p)) => p,
        _ => {
            return sabre_error(
                StatusCode::BAD_REQUEST,
                "Sabre\\DAV\\Exception\\BadRequest",
                "Invalid XML body",
            );
        }
    };
    let _ = user;
    let mut out = String::from("<d:response><d:href>");
    escape(&paths::href(&t.prefix, &t.path, false), &mut out);
    out.push_str("</d:href><d:propstat><d:prop>");
    for (ns, name) in &props {
        let prefix = match ns.as_str() {
            propfind::NS_DAV => "d",
            propfind::NS_OC => "oc",
            propfind::NS_NC => "nc",
            _ => "x",
        };
        out.push_str(&format!("<{prefix}:{name}/>"));
    }
    out.push_str("</d:prop><d:status>HTTP/1.1 403 Forbidden</d:status></d:propstat></d:response>");
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        propfind::multistatus(&out),
    )
        .into_response()
}

async fn mkcol(st: &AppState, user: &AuthUser, t: &Target) -> Response {
    let instance = match st.store.instance_id().await {
        Ok(i) => i,
        Err(e) => return internal(e),
    };
    let res = match resolve(st, user, &t.path).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    if !res.can(shares::CREATE) {
        return forbidden();
    }
    match st.store.mkcol(&res.owner, &res.path).await {
        Ok(node) => {
            let mut r = StatusCode::CREATED.into_response();
            set(
                &mut r,
                "oc-fileid",
                &propfind::file_id(&instance, node.fileid),
            );
            r
        }
        Err(FsError::ParentNotFound) => sabre_error(
            StatusCode::CONFLICT,
            "Sabre\\DAV\\Exception\\Conflict",
            "Parent node does not exist",
        ),
        Err(e) => fs_error(e, &t.path),
    }
}

async fn delete(st: &AppState, user: &AuthUser, t: &Target) -> Response {
    let res = match resolve(st, user, &t.path).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    // deleting the mount point of a share only removes it from the recipient's view
    if res.is_mount_root
        && let Some(m) = &res.mount
    {
        return match st.store.unshare_for(&owner(user), m.source.fileid).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => internal(e),
        };
    }
    if !res.can(shares::DELETE) {
        return forbidden();
    }
    match st.store.delete(&res.owner, &res.path).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fs_error(e, &t.path),
    }
}

async fn move_copy(st: &AppState, user: &AuthUser, t: &Target, req: Request) -> Response {
    let is_move = req.method() == Method::from_bytes(b"MOVE").unwrap();
    let dst = match destination(req.headers(), user) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let overwrite = !req
        .headers()
        .get("overwrite")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("F"));
    let instance = match st.store.instance_id().await {
        Ok(i) => i,
        Err(e) => return internal(e),
    };
    let me = owner(user);
    let mounts = match st.store.mounts(&me).await {
        Ok(m) => m,
        Err(e) => return internal(e),
    };
    let rs = shares::resolve_in(&mounts, &me, &t.path);
    let rd = shares::resolve_in(&mounts, &me, &dst);
    let reply = |node: &Node, created: bool| {
        let mut r = (if created {
            StatusCode::CREATED
        } else {
            StatusCode::NO_CONTENT
        })
        .into_response();
        set(
            &mut r,
            "oc-fileid",
            &propfind::file_id(&instance, node.fileid),
        );
        if is_move {
            set(&mut r, "etag", &quoted(&node.etag));
            set(&mut r, "oc-etag", &quoted(&node.etag));
        }
        r
    };

    // Renaming/moving a mount point only changes where the share appears in the user's own tree.
    if rs.is_mount_root
        && is_move
        && let Some(m) = &rs.mount
    {
        if rd.is_shared() {
            // stock: a mount point cannot be put inside another share (409)
            return sabre_error(
                StatusCode::CONFLICT,
                "Sabre\\DAV\\Exception\\Conflict",
                "Destination node does not exist",
            );
        }
        let parent_ok = matches!(
            st.store.get(&me, paths::split(&dst).0).await,
            Ok(Some(p)) if p.is_dir
        );
        if !parent_ok {
            return sabre_error(
                StatusCode::CONFLICT,
                "Sabre\\DAV\\Exception\\Conflict",
                "Destination node does not exist",
            );
        }
        let occupied = match st.store.get(&me, &dst).await {
            Ok(n) => n.is_some(),
            Err(e) => return internal(e),
        } || mounts
            .iter()
            .any(|x| x.target == dst && x.share_id != m.share_id);
        if occupied {
            return fs_error(FsError::PreconditionFailed, &t.path);
        }
        return match st.store.set_target(m.share_id, me.id, &dst).await {
            Ok(()) => reply(&m.source, true),
            Err(e) => internal(e),
        };
    }
    if rd.is_mount_root {
        return forbidden(); // a mount point cannot be replaced
    }
    let dest_exists = match st.store.get(&rd.owner, &rd.path).await {
        Ok(n) => n.is_some(),
        Err(e) => return internal(e),
    };
    let dest_need = if dest_exists {
        shares::UPDATE
    } else {
        shares::CREATE
    };
    let same_tree = rs.mount.as_ref().map(|m| m.share_id) == rd.mount.as_ref().map(|m| m.share_id);

    let res = if same_tree {
        let allowed = if is_move {
            let can_rename =
                rs.can(shares::UPDATE) || (rs.can(shares::CREATE) && rs.can(shares::DELETE));
            if paths::split(&rs.path).0 == paths::split(&rd.path).0 {
                can_rename // plain rename
            } else {
                rs.can(shares::UPDATE) // move to another folder of the same share
            }
        } else {
            rs.can(shares::READ) && rd.can(dest_need)
        };
        if !allowed {
            return forbidden();
        }
        if is_move {
            st.store
                .rename(&rs.owner, &rs.path, &rd.path, overwrite)
                .await
        } else {
            st.store
                .copy(&rs.owner, &rs.path, &rd.path, overwrite)
                .await
        }
    } else {
        // between two trees (own <-> share, share <-> share): copy, then delete the source for MOVE
        if !(rs.can(shares::READ) && rd.can(dest_need) && (!is_move || rs.can(shares::DELETE))) {
            return forbidden();
        }
        match st
            .store
            .copy_across(&rs.owner, &rs.path, &rd.owner, &rd.path, overwrite)
            .await
        {
            Ok(ok) => {
                if is_move && let Err(e) = st.store.delete_permanent(&rs.owner, &rs.path).await {
                    return fs_error(e, &t.path);
                }
                Ok(ok)
            }
            Err(e) => Err(e),
        }
    };
    match res {
        Ok((node, created)) => reply(&node, created),
        Err(FsError::ParentNotFound) => sabre_error(
            StatusCode::CONFLICT,
            "Sabre\\DAV\\Exception\\Conflict",
            "Destination node does not exist",
        ),
        Err(e) => fs_error(e, &t.path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("bytes=0-4", 12), Some(Ok((0, 4))));
        assert_eq!(parse_range("bytes=6-", 12), Some(Ok((6, 11))));
        assert_eq!(parse_range("bytes=-4", 12), Some(Ok((8, 11))));
        assert_eq!(parse_range("bytes=0-999", 12), Some(Ok((0, 11))));
        assert_eq!(parse_range("bytes=20-", 12), Some(Err(())));
        assert_eq!(parse_range("bytes=0-1,5-6", 12), None);
        assert_eq!(parse_range("items=0-1", 12), None);
    }
}
