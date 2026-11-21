//! Previews / thumbnails for the mobile and web clients.
//!
//! * `GET /index.php/core/preview[.png]?file=<path>|fileId=<id>&x=&y=&a=&mode=`
//! * `GET /index.php/apps/files/api/v1/thumbnail/{x}/{y}/{path}`
//!
//! Images (png, jpeg, gif, webp, bmp) are decoded, resized and cached as PNG below
//! `<data>/.appdata/preview/` (the cache key contains the ETag, so an edited file never serves a stale
//! preview). Decoding runs on the blocking pool behind a small semaphore so a gallery scroll cannot
//! take all CPUs. Any other file type gets a generic icon, like stock Nextcloud does.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use image::imageops::FilterType;
use image::{ImageFormat, Rgba, RgbaImage};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::auth::AuthUser;
use crate::dav::{internal, owner};
use crate::paths;
use crate::routes::AppState;
use crate::storage::Node;

const MAX_SOURCE_BYTES: i64 = 64 << 20;
const MAX_DIM: u32 = 4096;
const CACHE_PUBLIC: &str = "private, max-age=86400, immutable";
const CACHE_NONE: &str = "no-cache, no-store, must-revalidate";

/// Types we can decode, i.e. where `nc:has-preview` is true.
pub fn previewable(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
    )
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/index.php/core/preview", get(core_preview))
        .route("/index.php/core/preview.png", get(core_preview))
        .route("/core/preview", get(core_preview))
        .route("/core/preview.png", get(core_preview))
        .route(
            "/index.php/apps/files/api/v1/thumbnail/{x}/{y}/{*path}",
            get(thumbnail),
        )
}

/// Stock rounds requested sizes up to the next power of four (64, 256, 1024, 4096), so a gallery
/// scrolling through many sizes shares a few cached files.
fn bucket(n: u32) -> u32 {
    let mut b = 64;
    while b < n && b < MAX_DIM {
        b *= 4;
    }
    b
}

#[derive(Clone, Copy)]
struct Spec {
    x: u32,
    y: u32,
    /// keep the aspect ratio (`a=1`); otherwise crop to exactly x*y
    keep_aspect: bool,
    /// with `a=1`: scale so both sides reach x/y (no crop) instead of fitting inside
    cover: bool,
}

impl Spec {
    fn key(self) -> String {
        format!(
            "{}x{}-{}{}",
            self.x,
            self.y,
            u8::from(self.keep_aspect),
            u8::from(self.cover)
        )
    }
}

fn json_error(status: StatusCode) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (header::CACHE_CONTROL, CACHE_NONE),
        ],
        "[]",
    )
        .into_response()
}

async fn core_preview(
    State(st): State<AppState>,
    user: AuthUser,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let num = |k: &str, default: u32| match q.get(k) {
        None => Some(default),
        Some(v) => v.parse::<u32>().ok().filter(|n| *n >= 1),
    };
    let (Some(x), Some(y)) = (num("x", 32), num("y", 32)) else {
        return json_error(StatusCode::BAD_REQUEST);
    };
    let spec = Spec {
        x: bucket(x),
        y: bucket(y),
        keep_aspect: matches!(q.get("a").map(String::as_str), Some("1" | "true")),
        cover: q.get("mode").map(String::as_str) == Some("cover"),
    };
    let node = if let Some(id) = q.get("fileId") {
        let Ok(id) = id.parse::<i64>() else {
            return json_error(StatusCode::BAD_REQUEST);
        };
        st.store.visible_by_id(&owner(&user), id).await
    } else if let Some(file) = q.get("file") {
        let Ok(p) = paths::normalize(file) else {
            return json_error(StatusCode::NOT_FOUND);
        };
        lookup(&st, &user, &p).await
    } else {
        return json_error(StatusCode::BAD_REQUEST);
    };
    respond(&st, node, spec, &headers, CACHE_PUBLIC).await
}

async fn thumbnail(
    State(st): State<AppState>,
    user: AuthUser,
    UrlPath((x, y, path)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (Ok(x), Ok(y)) = (x.parse::<u32>(), y.parse::<u32>()) else {
        return json_error(StatusCode::BAD_REQUEST);
    };
    if x == 0 || y == 0 {
        return json_error(StatusCode::BAD_REQUEST);
    }
    let Ok(p) = paths::normalize(&path) else {
        return json_error(StatusCode::NOT_FOUND);
    };
    let spec = Spec {
        x: bucket(x),
        y: bucket(y),
        keep_aspect: false,
        cover: false,
    };
    let node = lookup(&st, &user, &p).await;
    respond(&st, node, spec, &headers, CACHE_NONE).await
}

/// A path of the user's tree (own or inside a share) -> the node and the owner of its bytes.
async fn lookup(
    st: &AppState,
    user: &AuthUser,
    path: &str,
) -> Result<Option<(Node, crate::storage::Owner)>, crate::storage::FsError> {
    let r = st.store.resolve(&owner(user), path).await?;
    Ok(st.store.get(&r.owner, &r.path).await?.map(|n| (n, r.owner)))
}

async fn respond(
    st: &AppState,
    node: Result<Option<(Node, crate::storage::Owner)>, crate::storage::FsError>,
    spec: Spec,
    req_headers: &HeaderMap,
    cache_control: &'static str,
) -> Response {
    let (node, real_owner) = match node {
        Ok(Some((n, o))) if !n.is_dir => (n, o),
        Ok(_) => return json_error(StatusCode::NOT_FOUND),
        Err(e) => return internal(e),
    };
    let key = spec.key();
    let etag = {
        let h = Sha256::digest(format!("{}:{}:{key}", node.fileid, node.etag));
        h[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let quoted = format!("\"{etag}\"");
    if req_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == quoted))
    {
        return png_headers(
            StatusCode::NOT_MODIFIED.into_response(),
            &quoted,
            cache_control,
        );
    }
    let bytes = if previewable(&node.mimetype) && node.size <= MAX_SOURCE_BYTES {
        let src = st.store.fs_path(&real_owner, &node.path);
        let cache = cache_file(&st.store.appdata_dir(), real_owner.id, &node, &key);
        match cached_or_render(src, cache, spec).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, file = %node.path, "preview failed");
                return json_error(StatusCode::NOT_FOUND);
            }
        }
    } else {
        icon(spec.x.min(spec.y).clamp(16, 256))
    };
    let r = (StatusCode::OK, bytes).into_response();
    png_headers(r, &quoted, cache_control)
}

fn png_headers(mut r: Response, etag: &str, cache_control: &'static str) -> Response {
    let ok = r.status() == StatusCode::OK;
    let h = r.headers_mut();
    if ok {
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    }
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    if let Ok(v) = HeaderValue::from_str(etag) {
        h.insert(header::ETAG, v);
    }
    r
}

fn cache_file(appdata: &Path, owner_id: i64, n: &Node, key: &str) -> PathBuf {
    appdata
        .join("preview")
        .join(owner_id.to_string())
        .join(n.fileid.to_string())
        .join(format!("{}-{key}.png", n.etag))
}

fn render_slots() -> &'static Semaphore {
    static SEM: OnceLock<Semaphore> = OnceLock::new();
    SEM.get_or_init(|| Semaphore::new(2))
}

async fn cached_or_render(src: PathBuf, cache: PathBuf, spec: Spec) -> anyhow::Result<Vec<u8>> {
    if let Ok(b) = tokio::fs::read(&cache).await {
        return Ok(b);
    }
    let _permit = render_slots().acquire().await?;
    let bytes = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let bytes = render(&src, spec)?;
        if let Some(dir) = cache.parent() {
            std::fs::create_dir_all(dir)?;
            let tmp = dir.join(format!(".tmp-{}", crate::auth::random_token(12)));
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &cache)?;
        }
        Ok(bytes)
    })
    .await??;
    Ok(bytes)
}

fn target_dims(w: u32, h: u32, s: Spec) -> (u32, u32) {
    let (fx, fy) = (f64::from(s.x) / f64::from(w), f64::from(s.y) / f64::from(h));
    // never upscale: a preview is a thumbnail, not an enlargement
    let f = if s.cover { fx.max(fy) } else { fx.min(fy) }.min(1.0);
    (
        (f64::from(w) * f).round().max(1.0) as u32,
        (f64::from(h) * f).round().max(1.0) as u32,
    )
}

fn render(src: &Path, s: Spec) -> anyhow::Result<Vec<u8>> {
    let mut reader = image::ImageReader::open(src)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(512 << 20);
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    reader.limits(limits);
    let img = reader.decode()?;
    let out = if s.keep_aspect {
        let (w, h) = target_dims(img.width(), img.height(), s);
        img.resize_exact(w, h, FilterType::Triangle)
    } else {
        img.resize_to_fill(
            s.x.min(img.width()),
            s.y.min(img.height()),
            FilterType::Triangle,
        )
    };
    encode_png(&out.to_rgba8())
}

fn encode_png(img: &RgbaImage) -> anyhow::Result<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png)?;
    Ok(out.into_inner())
}

/// Generic "file" icon for types without a preview: grey tile with a darker frame.
fn icon(size: u32) -> Vec<u8> {
    let frame = (size / 8).max(1);
    let img = RgbaImage::from_fn(size, size, |x, y| {
        if x < frame || y < frame || x >= size - frame || y >= size - frame {
            Rgba([0x9a, 0xa5, 0xb1, 0xff])
        } else {
            Rgba([0xd8, 0xde, 0xe6, 0xff])
        }
    });
    encode_png(&img).unwrap_or_default()
}

/// Delete cached previews not written within `max_age`. Returns how many files were removed.
pub async fn prune_cache(appdata: PathBuf, max_age: Duration) -> usize {
    tokio::task::spawn_blocking(move || {
        let cutoff = SystemTime::now()
            .checked_sub(max_age)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut removed = 0;
        fn walk(dir: &Path, cutoff: SystemTime, removed: &mut usize) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                let Ok(md) = e.metadata() else { continue };
                if md.is_dir() {
                    walk(&p, cutoff, removed);
                    let _ = std::fs::remove_dir(&p); // only succeeds when empty
                } else if md.modified().is_ok_and(|m| m < cutoff)
                    && std::fs::remove_file(&p).is_ok()
                {
                    *removed += 1;
                }
            }
        }
        walk(&appdata.join("preview"), cutoff, &mut removed);
        removed
    })
    .await
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(x: u32, y: u32, keep_aspect: bool, cover: bool) -> Spec {
        Spec {
            x,
            y,
            keep_aspect,
            cover,
        }
    }

    #[test]
    fn dims_fit_cover_and_no_upscale() {
        // 800x600 into 64x64: fit keeps everything inside, cover reaches 64 on the short side
        assert_eq!(target_dims(800, 600, spec(64, 64, true, false)), (64, 48));
        assert_eq!(target_dims(800, 600, spec(64, 64, true, true)), (85, 64));
        // small source is never enlarged
        assert_eq!(target_dims(20, 10, spec(64, 64, true, false)), (20, 10));
        assert_eq!(target_dims(20, 10, spec(64, 64, true, true)), (20, 10));
    }

    #[test]
    fn sizes_round_up_to_powers_of_four() {
        let b: Vec<u32> = [1, 32, 64, 65, 100, 128, 256, 257, 500, 1024, 1025, 99999]
            .map(bucket)
            .to_vec();
        assert_eq!(
            b,
            [64, 64, 64, 256, 256, 256, 256, 1024, 1024, 1024, 4096, 4096]
        );
    }

    #[test]
    fn only_decodable_types_have_previews() {
        assert!(previewable("image/png") && previewable("image/jpeg"));
        assert!(
            !previewable("video/mp4")
                && !previewable("text/plain")
                && !previewable("image/svg+xml")
        );
    }
}
