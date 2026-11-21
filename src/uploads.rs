//! Chunked upload v2 (Nextcloud "chunking 1.0"): `/remote.php/dav/uploads/{user}/{id}[/{chunk}]`.
//!
//! * `MKCOL  uploads/{user}/{id}`          start an upload (header `Destination` is informational)
//! * `PUT    uploads/{user}/{id}/{chunk}`  one chunk; chunk names are numbers, ordered numerically
//! * `PROPFIND uploads/{user}/{id}`        which chunks arrived (resume)
//! * `MOVE   uploads/{user}/{id}/.file`    assemble into `Destination` (`OC-Total-Length` is checked)
//! * `DELETE uploads/{user}/{id}`          abort
//!
//! Chunks live in `<data>/<uid>/uploads/<id>/` (same filesystem as the files, so the assembled file
//! is renamed into place). Assembly appends onto the first chunk and deletes each chunk once it is
//! consumed: peak disk use is the file size plus one chunk, not twice the file size.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use percent_encoding::percent_decode_str;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::auth::AuthUser;
use crate::dav::{
    UploadError, destination, forbidden, fs_error, internal, letters_for, node_headers, not_found,
    owner, put_opts, receive_body, resolve, sabre_error, set, too_large,
};
use crate::propfind::http_date;
use crate::routes::AppState;
use crate::shares;

const ALLOW: &str = "OPTIONS, PROPFIND, MKCOL, PUT, MOVE, DELETE";
const PREFIX: &str = "/remote.php/dav/uploads";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/remote.php/dav/uploads/{user}", any(uploads))
        .route("/remote.php/dav/uploads/{user}/", any(uploads))
        .route("/remote.php/dav/uploads/{user}/{*rest}", any(uploads))
}

/// A single safe path component: no separators, no dots at the start, bounded length.
fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn bad_request(msg: &str) -> Response {
    sabre_error(
        StatusCode::BAD_REQUEST,
        "Sabre\\DAV\\Exception\\BadRequest",
        msg,
    )
}

async fn uploads(State(st): State<AppState>, user: AuthUser, req: Request) -> Response {
    let rest = req
        .uri()
        .path()
        .strip_prefix(PREFIX)
        .unwrap_or("")
        .trim_start_matches('/');
    let parts: Vec<String> = rest
        .split('/')
        .filter(|p| !p.is_empty())
        .map(|p| percent_decode_str(p).decode_utf8_lossy().into_owned())
        .collect();
    if parts.first().map(String::as_str) != Some(user.uid.as_str()) {
        return sabre_error(
            StatusCode::NOT_FOUND,
            "Sabre\\DAV\\Exception\\NotFound",
            "Principal not found",
        );
    }
    let id = parts.get(1).map(String::as_str);
    let chunk = parts.get(2).map(String::as_str);
    if parts.len() > 3
        || id.is_some_and(|i| !safe_component(i))
        || chunk.is_some_and(|c| c != ".file" && !safe_component(c))
    {
        return bad_request("Invalid upload path");
    }
    let uploads_dir = st.store.user_dir(&user.uid).join("uploads");
    let method = req.method().as_str().to_string();
    let mut resp = match (method.as_str(), id, chunk) {
        ("OPTIONS", _, _) => {
            let mut r = StatusCode::OK.into_response();
            r.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static(ALLOW));
            r
        }
        ("PROPFIND", None, _) => listing(&uploads_dir, &user.uid, None, true).await,
        ("PROPFIND", Some(id), None) => {
            let deep = req
                .headers()
                .get("depth")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                != Some("0");
            listing(&uploads_dir.join(id), &user.uid, Some(id), deep).await
        }
        ("MKCOL", Some(id), None) => {
            let declared = req
                .headers()
                .get("oc-total-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            if declared.is_some_and(|n| !st.config.allows_size(n)) {
                too_large(st.config.max_file_size)
            } else {
                mkcol(&uploads_dir, id).await
            }
        }
        ("PUT", Some(id), Some(c)) if c != ".file" => {
            put_chunk(&st, &user, &uploads_dir.join(id), c, req).await
        }
        ("MOVE", Some(id), Some(".file")) => finish(&st, &user, &uploads_dir.join(id), req).await,
        ("DELETE", Some(id), None) => match tokio::fs::remove_dir_all(uploads_dir.join(id)).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => not_found(id),
            Err(e) => internal(e),
        },
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
        HeaderValue::from_static("1, 3, extended-mkcol, nextcloud-checksum-update"),
    );
    resp
}

async fn mkcol(uploads_dir: &Path, id: &str) -> Response {
    if let Err(e) = tokio::fs::create_dir_all(uploads_dir).await {
        return internal(e);
    }
    match tokio::fs::create_dir(uploads_dir.join(id)).await {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            fs_error(crate::storage::FsError::Exists, id)
        }
        Err(e) => internal(e),
    }
}

async fn put_chunk(
    st: &AppState,
    user: &AuthUser,
    dir: &Path,
    chunk: &str,
    req: Request,
) -> Response {
    if !tokio::fs::try_exists(dir).await.unwrap_or(false) {
        return not_found(chunk);
    }
    let (tmp, _size, _sha) = match receive_body(st, &owner(user), req.into_body()).await {
        Ok(x) => x,
        Err(UploadError::Response(r)) => return r,
    };
    // same filesystem (both below <data>/<uid>/): atomic, and a retried chunk replaces the old one
    if let Err(e) = tokio::fs::rename(&tmp, dir.join(chunk)).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return if e.kind() == std::io::ErrorKind::NotFound {
            not_found(chunk) // upload was aborted or cleaned up meanwhile
        } else {
            internal(e)
        };
    }
    // the whole upload must stay below the limit too, not just each chunk
    if let Ok(list) = chunks(dir).await
        && !st.config.allows_size(list.iter().map(|c| c.2).sum())
    {
        let _ = tokio::fs::remove_dir_all(dir).await;
        return too_large(st.config.max_file_size);
    }
    StatusCode::CREATED.into_response()
}

/// Chunks of an upload folder as (number-or-MAX, name, size), in assembly order.
async fn chunks(dir: &Path) -> std::io::Result<Vec<(u64, String, u64)>> {
    let mut out = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(e) = rd.next_entry().await? {
        let md = e.metadata().await?;
        if md.is_file() {
            let name = e.file_name().to_string_lossy().into_owned();
            // numeric order: unpadded names ("2" < "10") must not be sorted as text
            out.push((name.parse::<u64>().unwrap_or(u64::MAX), name, md.len()));
        }
    }
    out.sort();
    Ok(out)
}

async fn listing(dir: &Path, uid: &str, id: Option<&str>, deep: bool) -> Response {
    let href = match id {
        Some(id) => format!("{PREFIX}/{uid}/{id}/"),
        None => format!("{PREFIX}/{uid}/"),
    };
    let mut xml = String::from(
        "<?xml version=\"1.0\"?>\n<d:multistatus xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\">\n",
    );
    let coll = |href: &str| {
        format!(
            "<d:response><d:href>{href}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>\
             <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\n"
        )
    };
    match tokio::fs::metadata(dir).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && id.is_none() => {} // no uploads yet
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return not_found(id.unwrap_or("")),
        Err(e) => return internal(e),
    }
    xml.push_str(&coll(&href));
    if deep && id.is_some() {
        let list = match chunks(dir).await {
            Ok(l) => l,
            Err(e) => return internal(e),
        };
        for (_, name, size) in list {
            let mtime = tokio::fs::metadata(dir.join(&name))
                .await
                .and_then(|m| m.modified())
                .map(unix)
                .unwrap_or(0);
            xml.push_str(&format!(
                "<d:response><d:href>{href}{name}</d:href><d:propstat><d:prop><d:getcontentlength>{size}</d:getcontentlength>\
                 <d:getlastmodified>{}</d:getlastmodified><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\n",
                http_date(mtime)
            ));
        }
    } else if deep && let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if e.file_type().await.is_ok_and(|t| t.is_dir()) {
                xml.push_str(&coll(&format!(
                    "{href}{}/",
                    e.file_name().to_string_lossy()
                )));
            }
        }
    }
    xml.push_str("</d:multistatus>\n");
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

fn unix(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `MOVE .file`: assemble the chunks into the destination file.
async fn finish(st: &AppState, user: &AuthUser, dir: &Path, req: Request) -> Response {
    let dest = match destination(req.headers(), user) {
        Ok(d) if !d.is_empty() => d,
        Ok(_) => return bad_request("Invalid Destination header"),
        Err(r) => return r,
    };
    let headers = req.headers().clone();
    let list = match chunks(dir).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return not_found(&dest),
        Err(e) => return internal(e),
    };
    if list.is_empty() {
        return bad_request("No chunks were uploaded");
    }
    if list.iter().any(|(n, _, _)| *n == u64::MAX) {
        return bad_request("Chunk names must be numbers");
    }
    let total: u64 = list.iter().map(|c| c.2).sum();
    if !st.config.allows_size(total) {
        let _ = tokio::fs::remove_dir_all(dir).await;
        return too_large(st.config.max_file_size);
    }
    if let Some(want) = headers
        .get("oc-total-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        && want != total
    {
        return bad_request(&format!(
            "Chunks on server do not sum up to {want} but {total} bytes"
        ));
    }
    let checksum = headers
        .get("oc-checksum")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let want_sha1 = checksum
        .as_deref()
        .and_then(|c| c.split_once(':'))
        .filter(|(algo, _)| algo.eq_ignore_ascii_case("sha1"))
        .map(|(_, v)| v.to_ascii_lowercase());

    let base = dir.join(&list[0].1);
    let sha1 = match assemble(&base, dir, &list, want_sha1.is_some()).await {
        Ok(h) => h,
        Err(e) => return internal(e),
    };
    if let (Some(want), Some(got)) = (&want_sha1, &sha1)
        && want != got
    {
        let _ = tokio::fs::remove_dir_all(dir).await;
        return bad_request(
            "The computed checksum does not match the one received from the client.",
        );
    }
    let res = match resolve(st, user, &dest).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let exists = match st.store.get(&res.owner, &res.path).await {
        Ok(n) => n.is_some(),
        Err(e) => return internal(e),
    };
    if !res.can(if exists {
        shares::UPDATE
    } else {
        shares::CREATE
    }) {
        let _ = tokio::fs::remove_dir_all(dir).await;
        return forbidden();
    }
    let (opts, mtime_given) = put_opts(&headers, &res.path, checksum);
    let instance = match st.store.instance_id().await {
        Ok(i) => i,
        Err(e) => return internal(e),
    };
    match st
        .store
        .put(&res.owner, &res.path, &base, total as i64, opts)
        .await
    {
        Ok((node, created)) => {
            let _ = tokio::fs::remove_dir_all(dir).await;
            let mut r = (if created {
                StatusCode::CREATED
            } else {
                StatusCode::NO_CONTENT
            })
            .into_response();
            if mtime_given {
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
        Err(e) => {
            // `put` removed the assembled file; the leftovers are useless, the client restarts
            let _ = tokio::fs::remove_dir_all(dir).await;
            fs_error(e, &dest)
        }
    }
}

/// Append chunks 2..n onto the first one, deleting each once consumed. Returns the SHA-1 of the
/// whole file if asked for.
async fn assemble(
    base: &Path,
    dir: &Path,
    list: &[(u64, String, u64)],
    hash: bool,
) -> std::io::Result<Option<String>> {
    let mut sha = hash.then(Sha1::new);
    let mut buf = vec![0u8; 1 << 20];
    if let Some(sha) = sha.as_mut() {
        let mut f = tokio::fs::File::open(base).await?;
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            sha.update(&buf[..n]);
        }
    }
    let mut out = tokio::fs::OpenOptions::new()
        .append(true)
        .open(base)
        .await?;
    for (_, name, _) in &list[1..] {
        let p = dir.join(name);
        let mut f = tokio::fs::File::open(&p).await?;
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).await?;
            if let Some(sha) = sha.as_mut() {
                sha.update(&buf[..n]);
            }
        }
        drop(f);
        tokio::fs::remove_file(&p).await?;
    }
    out.flush().await?;
    out.sync_all().await?;
    Ok(sha.map(|s| s.finalize().iter().map(|b| format!("{b:02x}")).collect()))
}

/// Remove abandoned upload folders and temp files older than `max_age` (by modification time).
/// User folders come from the database, not from listing the data dir. Returns what was removed.
pub async fn cleanup_stale(
    pool: &sqlx::SqlitePool,
    store: &crate::storage::Store,
    max_age: Duration,
) -> anyhow::Result<usize> {
    let uids: Vec<String> = sqlx::query_scalar("SELECT uid FROM users")
        .fetch_all(pool)
        .await?;
    let cutoff = SystemTime::now()
        .checked_sub(max_age)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut removed = 0;
    for uid in uids {
        let base: PathBuf = store.user_dir(&uid);
        for sub in ["uploads", "tmp"] {
            let Ok(mut rd) = tokio::fs::read_dir(base.join(sub)).await else {
                continue;
            };
            while let Ok(Some(e)) = rd.next_entry().await {
                let Ok(md) = e.metadata().await else { continue };
                // an upload folder's mtime moves with every chunk that is added or consumed
                let fresh = md.modified().is_ok_and(|m| m > cutoff);
                if fresh {
                    continue;
                }
                let r = if md.is_dir() {
                    tokio::fs::remove_dir_all(e.path()).await
                } else {
                    tokio::fs::remove_file(e.path()).await
                };
                match r {
                    Ok(()) => removed += 1,
                    Err(err) => {
                        tracing::warn!(path = %e.path().display(), error = %err, "cleanup failed")
                    }
                }
            }
        }
    }
    Ok(removed)
}
