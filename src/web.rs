//! The browser UI (server-rendered with askama, enhanced with HTMX).
//!
//! * Pages and fragments live here; **every file operation goes through the same DAV/OCS endpoints the
//!   sync clients use** (`static/app.js` calls them with the session cookie and `X-CSRF-Token`), so the
//!   permission rules of sharing apply to the web UI without a second implementation.
//! * Sessions are the `rb_session` cookie of `login`; the CSRF token is derived from the session id
//!   (`auth::csrf_token_for`), rendered into the page and sent back as a header or a hidden form field.
//! * No inline scripts or styles on app pages (CSP `script-src 'self'`); user text only ever reaches the
//!   HTML through askama's escaping, and URLs are built with percent-encoding.

#![allow(clippy::result_large_err)] // extractor rejections and helpers return ready-made responses

use std::collections::HashMap;

use askama::Template;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;

use crate::auth::{
    self, AuthUser, ClientIp, Peer, csrf_token_for, session_auth_user, sha256_hex, unix_now,
};
use crate::dav::owner;
use crate::db::{self, UserEdit};
use crate::login::{
    SESSION_COOKIE, SESSION_TTL_SECS, cookie_value, csrf_ok, is_secure, set_cookie,
};
use crate::paths;
use crate::preview::previewable;
use crate::propfind;
use crate::routes::AppState;
use crate::shares::{self, Resolved, outgoing_list};
use crate::storage::{FsError, Owner};
use crate::view::{Item, ViewCtx};

const CSRF_COOKIE: &str = "rb_csrf";

/// App pages: scripts and styles only from this origin, nothing inline.
const CSP_APP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; \
     connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";
/// The login page keeps the small inline style of the other pre-login pages.
const CSP_LOGIN: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn enc(s: &str) -> String {
    utf8_percent_encode(s, COMPONENT).to_string()
}
fn enc_path(p: &str) -> String {
    p.split('/')
        .filter(|x| !x.is_empty())
        .map(enc)
        .collect::<Vec<_>>()
        .join("/")
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(root))
        .route("/login", get(login_page).post(login_submit))
        .route("/index.php/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
        .route("/apps/files", get(files))
        .route("/apps/files/", get(files))
        .route("/index.php/apps/files", get(files))
        .route("/index.php/apps/files/", get(files))
        .route("/apps/files/trash", get(trash_page))
        .route("/index.php/apps/files/trash", get(trash_page))
        .route("/ui/share", get(share_dialog))
        .route("/ui/versions", get(versions_dialog))
        .route("/settings/user", get(settings))
        .route("/settings/user/profile", post(settings_profile))
        .route("/settings/user/password", post(settings_password))
        .route("/settings/user/app-passwords", post(app_password_create))
        .route(
            "/settings/user/app-passwords/{id}/delete",
            post(app_password_delete),
        )
        .route("/settings/about", get(about_page))
        .route("/settings/users", get(users_page).post(user_create))
        .route("/settings/users/{uid}", post(user_edit))
        .route("/settings/groups", post(group_create))
        .route("/settings/groups/{gid}/members", post(group_add_member))
        .route(
            "/settings/groups/{gid}/members/{uid}/delete",
            post(group_remove_member),
        )
        .route("/static/{name}", get(static_asset))
}

// ---------------------------------------------------------------- responses

fn secure_headers(r: &mut Response, csp: &'static str) {
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(csp),
    );
}

fn render(status: StatusCode, t: &impl Template, csp: &'static str) -> Response {
    match t.render() {
        Ok(body) => {
            let mut r = (status, body).into_response();
            secure_headers(&mut r, csp);
            r
        }
        Err(e) => {
            tracing::error!(error = %e, "template rendering failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

fn redirect(to: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, to.to_string())]).into_response()
}

fn found(to: &str) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, to.to_string())]).into_response()
}

/// Only same-site absolute paths (no open redirects).
fn safe_redirect(to: &str) -> String {
    // browsers drop tabs and newlines inside a URL, so "/<TAB>/evil.example" would become "//evil.example":
    // no control characters at all
    if to.starts_with('/')
        && !to.starts_with("//")
        && !to.chars().any(|c| c.is_control() || c == '\\')
    {
        to.to_string()
    } else {
        "/apps/files/".to_string()
    }
}

// ---------------------------------------------------------------- session extractor

pub struct Web {
    pub user: AuthUser,
    pub csrf: String,
    sid: String,
}

impl Web {
    /// Form posts carry the token as a hidden field.
    fn check(&self, token: &str) -> Result<(), Response> {
        if crate::login::ct_eq(token, &self.csrf) {
            Ok(())
        } else {
            Err((StatusCode::FORBIDDEN, "invalid CSRF token").into_response())
        }
    }
}

impl FromRequestParts<AppState> for Web {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        if let Some(sid) = cookie_value(&parts.headers, SESSION_COOKIE)
            && let Ok(Some(user)) = session_auth_user(&state.pool, &sid).await
        {
            return Ok(Web {
                user,
                csrf: csrf_token_for(&sid),
                sid,
            });
        }
        // HTMX asks for a fragment: tell it to leave the page; otherwise send the browser to the login
        if parts.headers.contains_key("hx-request") {
            let mut r = StatusCode::UNAUTHORIZED.into_response();
            r.headers_mut()
                .insert("hx-redirect", HeaderValue::from_static("/login"));
            return Err(r);
        }
        let back = if parts.method == axum::http::Method::GET {
            format!(
                "/login?redirect_url={}",
                enc(parts.uri.path_and_query().map_or("/", |p| p.as_str()))
            )
        } else {
            "/login".to_string()
        };
        Err(redirect(&back))
    }
}

/// An administrator; everyone else gets a plain 403 (the nav link is hidden for them as well).
pub struct Admin(pub Web);

impl FromRequestParts<AppState> for Admin {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let w = Web::from_request_parts(parts, state).await?;
        if w.user.is_admin {
            Ok(Admin(w))
        } else {
            Err((StatusCode::FORBIDDEN, "administrators only").into_response())
        }
    }
}

#[derive(Clone)]
struct Chrome {
    csrf: String,
    /// the running build; its id keys the asset links, so a new build is never served stale scripts
    build: &'static crate::buildinfo::BuildInfo,
    uid: String,
    uid_enc: String,
    display: String,
    is_admin: bool,
}

impl Chrome {
    fn new(w: &Web) -> Self {
        Self {
            csrf: w.csrf.clone(),
            build: crate::buildinfo::get(),
            uid: w.user.uid.clone(),
            uid_enc: enc(&w.user.uid),
            display: w.user.display_name.clone(),
            is_admin: w.user.is_admin,
        }
    }
}

// ---------------------------------------------------------------- login / logout

#[derive(Template)]
#[template(path = "weblogin.html")]
struct LoginPage {
    csrf: String,
    redirect: String,
    error: Option<String>,
}

#[derive(Deserialize)]
struct LoginQuery {
    redirect_url: Option<String>,
}

async fn root(State(st): State<AppState>, headers: HeaderMap) -> Response {
    // stock Nextcloud: 302 to the login page; a logged-in browser goes straight to its files
    if let Some(sid) = cookie_value(&headers, SESSION_COOKIE)
        && matches!(session_auth_user(&st.pool, &sid).await, Ok(Some(_)))
    {
        return found("/apps/files/");
    }
    found("/login")
}

async fn login_page(
    State(st): State<AppState>,
    Peer(peer): Peer,
    headers: HeaderMap,
    Query(q): Query<LoginQuery>,
) -> Response {
    let redirect = safe_redirect(q.redirect_url.as_deref().unwrap_or("/apps/files/"));
    if let Some(sid) = cookie_value(&headers, SESSION_COOKIE)
        && matches!(session_auth_user(&st.pool, &sid).await, Ok(Some(_)))
    {
        return redirect_to(&redirect);
    }
    let (csrf, cookie) = csrf_cookie(&st, &headers, peer);
    let mut r = render(
        StatusCode::OK,
        &LoginPage {
            csrf,
            redirect,
            error: None,
        },
        CSP_LOGIN,
    );
    if let Some(c) = cookie {
        r.headers_mut().append(header::SET_COOKIE, c);
    }
    r
}

fn redirect_to(to: &str) -> Response {
    redirect(to)
}

/// Double-submit cookie for the pre-login form.
fn csrf_cookie(
    st: &AppState,
    headers: &HeaderMap,
    peer: std::net::IpAddr,
) -> (String, Option<HeaderValue>) {
    match cookie_value(headers, CSRF_COOKIE).filter(|c| !c.is_empty()) {
        Some(c) => (c, None),
        None => {
            let c = auth::random_token(32);
            let h = set_cookie(
                CSRF_COOKIE,
                &c,
                SESSION_TTL_SECS,
                is_secure(st, headers, peer),
            );
            (c, Some(h))
        }
    }
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    csrf: String,
    redirect_url: Option<String>,
}

async fn login_submit(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    Peer(peer): Peer,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    let redirect = safe_redirect(f.redirect_url.as_deref().unwrap_or("/apps/files/"));
    if !csrf_ok(&headers, &f.csrf) {
        return (StatusCode::FORBIDDEN, "invalid CSRF token").into_response();
    }
    if let Err(secs) = st.auth_gate(ip) {
        let mut r = (
            StatusCode::TOO_MANY_REQUESTS,
            format!("Too many failed attempts, retry in {secs} s"),
        )
            .into_response();
        if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
            r.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return r;
    }
    let user = match auth::verify_user_password(&st.pool, f.username.trim(), &f.password).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            st.auth_failed(ip);
            return render(
                StatusCode::UNAUTHORIZED,
                &LoginPage {
                    csrf: f.csrf,
                    redirect,
                    error: Some("Wrong account name or password.".into()),
                },
                CSP_LOGIN,
            );
        }
        Err(e) => {
            tracing::error!(error = ?e, "login failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    st.auth_ok(ip);
    let sid = auth::random_token(48);
    let now = unix_now();
    if let Err(e) = sqlx::query("INSERT INTO web_sessions (id_hash, user_id, created_at, expires_at) VALUES ($1, $2, $3, $4)")
        .bind(sha256_hex(&sid))
        .bind(user.id)
        .bind(now)
        .bind(now + SESSION_TTL_SECS)
        .execute(&st.pool)
        .await
    {
        tracing::error!(error = %e, "cannot create session");
        return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
    }
    let mut r = redirect_to(&redirect);
    r.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(
            SESSION_COOKIE,
            &sid,
            SESSION_TTL_SECS,
            is_secure(&st, &headers, peer),
        ),
    );
    r
}

#[derive(Deserialize)]
struct CsrfOnly {
    csrf: String,
}

async fn logout(
    State(st): State<AppState>,
    w: Web,
    Peer(peer): Peer,
    headers: HeaderMap,
    Form(f): Form<CsrfOnly>,
) -> Response {
    if let Err(r) = w.check(&f.csrf) {
        return r;
    }
    let _ = sqlx::query("DELETE FROM web_sessions WHERE id_hash = $1")
        .bind(sha256_hex(&w.sid))
        .execute(&st.pool)
        .await;
    let mut r = redirect("/login");
    r.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(SESSION_COOKIE, "", 0, is_secure(&st, &headers, peer)),
    );
    r
}

// ---------------------------------------------------------------- file browser

struct Crumb {
    name: String,
    link: String,
}

struct Row {
    /// path in the user's tree with a leading slash (what app.js sends to DAV)
    path: String,
    name: String,
    is_dir: bool,
    link: String,
    download: String,
    thumb: Option<String>,
    icon: &'static str,
    size: String,
    mtime: String,
    from_owner: Option<String>,
    shared_out: bool,
    read_only: bool,
    can_rename: bool,
    can_move: bool,
    can_share: bool,
    can_versions: bool,
    can_delete: bool,
    delete_label: &'static str,
    /// second line under the name (who it is shared with / permission / date), shared views only
    sub: Option<String>,
    /// a folder link that stays inside the current view may be loaded in place (HTMX); one that leaves it must not
    boost: bool,
}

#[derive(Template)]
#[template(path = "files_inner.html")]
struct FilesInner {
    dir: String,
    can_create: bool,
    can_mkdir: bool,
    root_label: &'static str,
    root_link: String,
    empty_text: &'static str,
    crumbs: Vec<Crumb>,
    message: Option<&'static str>,
    rows: Vec<Row>,
}

#[derive(Template)]
#[template(path = "files.html")]
struct FilesPage {
    chrome: Chrome,
    nav: Vec<NavItem>,
    inner: String,
}

#[derive(Deserialize)]
struct FilesQuery {
    dir: Option<String>,
    view: Option<String>,
}

// ---------------------------------------------------------------- left menu and the three file views

/// The views of the file browser: `?view=` in the URL, "mine" when absent or unknown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NavView {
    /// my own files (incoming shares are not part of this tree in the web view)
    Mine,
    /// what other people shared with me
    SharedWithMe,
    /// what I share with others
    Shared,
}

impl NavView {
    fn parse(s: Option<&str>) -> Self {
        match s {
            Some("shared-with-me") => Self::SharedWithMe,
            Some("shared") => Self::Shared,
            _ => Self::Mine,
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::Mine => "mine",
            Self::SharedWithMe => "shared-with-me",
            Self::Shared => "shared",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Mine => "My own files",
            Self::SharedWithMe => "Shared with me",
            Self::Shared => "Shared",
        }
    }
    fn root_link(self) -> String {
        match self {
            Self::Mine => "/apps/files/".to_string(),
            v => format!("/apps/files/?view={}", v.key()),
        }
    }
    fn empty_text(self) -> &'static str {
        match self {
            Self::Mine => "This folder is empty.",
            Self::SharedWithMe => "Nobody has shared anything with you yet.",
            Self::Shared => "You have not shared anything yet.",
        }
    }
}

/// Link to folder `dir` (a path in the viewer's tree, leading slash) inside `view`. The "mine" form is the
/// one that existed before the views did, so old bookmarks keep working.
fn files_link(view: NavView, dir: &str) -> String {
    match view {
        NavView::Mine => format!("/apps/files/?dir={}", enc(dir)),
        v => format!("/apps/files/?view={}&dir={}", v.key(), enc(dir)),
    }
}

struct NavItem {
    href: &'static str,
    label: &'static str,
    icon: &'static str,
    active: bool,
}

/// Entries of the left menu; `active` is one of `mine`, `shared-with-me`, `shared`, `trash`.
fn nav_items(active: &str) -> Vec<NavItem> {
    [
        ("mine", "/apps/files/", "My own files", "\u{1F4C1}"),
        (
            "shared-with-me",
            "/apps/files/?view=shared-with-me",
            "Shared with me",
            "\u{1F465}",
        ),
        ("shared", "/apps/files/?view=shared", "Shared", "\u{1F517}"),
        ("trash", "/apps/files/trash", "Trash", "\u{1F5D1}"),
    ]
    .into_iter()
    .map(|(key, href, label, icon)| NavItem {
        href,
        label,
        icon,
        active: key == active,
    })
    .collect()
}

/// "Can view" / "Can edit" like the share dialog; anything else is shown as custom. The share bit is
/// ignored (resharing does not exist).
fn perm_label(is_dir: bool, perms: u32) -> String {
    let base = perms & 15;
    perm_presets(is_dir)
        .into_iter()
        .find(|(v, _)| *v == base)
        .map_or_else(|| format!("Custom ({perms})"), |(_, l)| l.to_string())
}

pub fn human_size(n: i64) -> String {
    let n = n.max(0) as f64;
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", n as i64)
    } else {
        format!("{v:.1} {}", units[i])
    }
}

/// `YYYY-MM-DD HH:MM` (UTC) without a date library.
pub fn fmt_time(ts: i64) -> String {
    if ts <= 0 {
        return "-".into();
    }
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

fn icon_for(mime: &str, is_dir: bool) -> &'static str {
    if is_dir {
        "\u{1F4C1}"
    } else if mime.starts_with("image/") {
        "\u{1F5BC}"
    } else if mime.starts_with("video/") {
        "\u{1F39E}"
    } else if mime.starts_with("audio/") {
        "\u{1F3B5}"
    } else if mime.contains("zip") || mime.contains("compressed") || mime.contains("tar") {
        "\u{1F5DC}"
    } else {
        "\u{1F4C4}"
    }
}

fn sort_rows(rows: &mut [Row]) {
    rows.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// One list row for an item the viewer can see. `link` is where a folder row leads.
fn make_row(w: &Web, it: &Item, link: String, boost: bool) -> Row {
    let n = &it.node;
    let l = it
        .extra
        .perms
        .clone()
        .unwrap_or_else(|| propfind::permissions(n).to_string());
    let path = format!("/{}", n.path);
    let root = it.extra.mount_root;
    Row {
        link,
        download: format!(
            "/remote.php/dav/files/{}/{}",
            enc(&w.user.uid),
            enc_path(&n.path)
        ),
        thumb: (!n.is_dir && previewable(&n.mimetype))
            .then(|| format!("/index.php/core/preview.png?fileId={}&x=64&y=64", n.fileid)),
        icon: icon_for(&n.mimetype, n.is_dir),
        size: human_size(n.size),
        mtime: fmt_time(n.mtime),
        from_owner: root.then(|| it.extra.owner_display.clone()).flatten(),
        shared_out: !it.extra.shares.is_empty(),
        read_only: root
            && if n.is_dir {
                !l.contains('C')
            } else {
                !l.contains('W')
            },
        can_rename: l.contains('N'),
        can_move: l.contains('V'),
        can_share: it.extra.perms.is_none(),
        can_versions: !n.is_dir && it.extra.perms.is_none(),
        can_delete: l.contains('D'),
        delete_label: if root { "Remove share" } else { "Delete" },
        is_dir: n.is_dir,
        name: n.name.clone(),
        path,
        sub: None,
        boost,
    }
}

/// "Shared with me": one row per incoming share, wherever the recipient has put the mount.
fn incoming_rows(w: &Web, vc: &ViewCtx<'_>) -> Vec<Row> {
    let mut rows: Vec<Row> = vc
        .mounts
        .iter()
        .map(|m| {
            let it = vc.mount_item(m);
            let path = format!("/{}", it.node.path);
            let mut r = make_row(w, &it, files_link(NavView::SharedWithMe, &path), true);
            r.sub = Some(format!(
                "{} \u{b7} shared {}",
                perm_label(m.source.is_dir, m.perms),
                fmt_time(m.stime)
            ));
            r
        })
        .collect();
    sort_rows(&mut rows);
    rows
}

/// "Shared": what the viewer shares with others. Folders lead to their real place in "My own files".
async fn outgoing_rows(
    st: &AppState,
    w: &Web,
    vc: &ViewCtx<'_>,
    me: &Owner,
) -> Result<Vec<Row>, FsError> {
    let mut rows = Vec::new();
    for o in outgoing_list(&st.store, me.id).await? {
        let it = vc.item(&Resolved::own(me, &o.node.path), o.node.clone());
        let path = format!("/{}", it.node.path);
        let mut r = make_row(w, &it, files_link(NavView::Mine, &path), false);
        let with = o
            .recipients
            .iter()
            .map(|x| {
                let who = if x.share_type == shares::TYPE_GROUP {
                    format!("{} (group)", x.with)
                } else {
                    x.display.clone()
                };
                format!("{who}: {}", perm_label(o.node.is_dir, x.perms))
            })
            .collect::<Vec<_>>()
            .join(", ");
        let since = o.recipients.iter().map(|x| x.stime).min().unwrap_or(0);
        r.sub = Some(format!("with {with} \u{b7} shared {}", fmt_time(since)));
        rows.push(r);
    }
    sort_rows(&mut rows);
    Ok(rows)
}

/// Full page, or only the listing when HTMX asks for it.
fn respond(
    w: &Web,
    headers: &HeaderMap,
    status: StatusCode,
    inner: FilesInner,
    active: &str,
) -> Response {
    let htmx =
        headers.contains_key("hx-request") && !headers.contains_key("hx-history-restore-request");
    if htmx {
        return render(status, &inner, CSP_APP);
    }
    match inner.render() {
        Ok(html) => render(
            status,
            &FilesPage {
                chrome: Chrome::new(w),
                nav: nav_items(active),
                inner: html,
            },
            CSP_APP,
        ),
        Err(e) => server_error(e),
    }
}

/// The top level of "Shared with me" / "Shared": a list, not a folder (nothing can be uploaded into it).
async fn shared_top(
    st: &AppState,
    w: &Web,
    vc: &ViewCtx<'_>,
    headers: &HeaderMap,
    view: NavView,
    status: StatusCode,
    message: Option<&'static str>,
) -> Response {
    let rows = if view == NavView::Shared {
        match outgoing_rows(st, w, vc, &owner(&w.user)).await {
            Ok(r) => r,
            Err(e) => return server_error(e),
        }
    } else {
        incoming_rows(w, vc)
    };
    let inner = FilesInner {
        dir: "/".to_string(),
        can_create: false,
        can_mkdir: false,
        root_label: view.label(),
        root_link: view.root_link(),
        empty_text: view.empty_text(),
        crumbs: Vec::new(),
        message,
        rows,
    };
    respond(w, headers, status, inner, view.key())
}

async fn files(
    State(st): State<AppState>,
    w: Web,
    Query(q): Query<FilesQuery>,
    headers: HeaderMap,
) -> Response {
    let view = NavView::parse(q.view.as_deref());
    let dir_path = paths::normalize(q.dir.as_deref().unwrap_or("/")).unwrap_or_default();
    let me = owner(&w.user);
    let vc = match ViewCtx::new(&st.store, &me, &w.user.display_name).await {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    // the two "Shared" views start with a list; only "Shared with me" has folders of its own below it
    if dir_path.is_empty() && view != NavView::Mine {
        return shared_top(&st, &w, &vc, &headers, view, StatusCode::OK, None).await;
    }
    if view == NavView::Shared {
        // the folders of that list are my own files
        return redirect(&files_link(NavView::Mine, &format!("/{dir_path}")));
    }
    let mut message = None;
    let mut status = StatusCode::OK;
    let mut shown = dir_path.clone();
    let mut r = vc.resolve(&shown);
    if view == NavView::Mine && r.mount.is_some() {
        // an incoming share is not part of "My own files": show it where it belongs
        return redirect(&files_link(NavView::SharedWithMe, &format!("/{dir_path}")));
    }
    let mut node = match st.store.get(&r.owner, &r.path).await {
        Ok(n) => n.filter(|n| n.is_dir && (view == NavView::Mine || r.mount.is_some())),
        Err(e) => return server_error(e),
    };
    if node.is_none() {
        // unknown folder (or a file): say so and show the top level instead
        let msg = "That folder does not exist (any more).";
        if view == NavView::SharedWithMe {
            return shared_top(
                &st,
                &w,
                &vc,
                &headers,
                view,
                StatusCode::NOT_FOUND,
                Some(msg),
            )
            .await;
        }
        message = Some(msg);
        status = StatusCode::NOT_FOUND;
        shown = String::new();
        r = vc.resolve("");
        node = match st.store.get(&r.owner, &r.path).await {
            Ok(n) => n,
            Err(e) => return server_error(e),
        };
    }
    let Some(node) = node else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let here = vc.item(&r, node.clone());
    let letters = here
        .extra
        .perms
        .clone()
        .unwrap_or_else(|| propfind::permissions(&node).to_string());
    let kids = match vc.children_opts(&r, &node, false).await {
        Ok(k) => k,
        Err(e) => return server_error(e),
    };
    let mut rows: Vec<Row> = kids
        .iter()
        .map(|it| {
            make_row(
                &w,
                it,
                files_link(view, &format!("/{}", it.node.path)),
                true,
            )
        })
        .collect();
    sort_rows(&mut rows);
    // below "Shared with me" the path starts at the share itself, not at the folders above where it is mounted
    let skip = match (&r.mount, view) {
        (Some(m), NavView::SharedWithMe) => m.target.split('/').count() - 1,
        _ => 0,
    };
    let mut crumbs = Vec::new();
    let mut acc = String::new();
    for (i, part) in shown.split('/').filter(|p| !p.is_empty()).enumerate() {
        acc.push('/');
        acc.push_str(part);
        if i >= skip {
            crumbs.push(Crumb {
                name: part.to_string(),
                link: files_link(view, &acc),
            });
        }
    }
    let inner = FilesInner {
        dir: format!("/{shown}"),
        can_create: letters.contains('C'),
        can_mkdir: letters.contains('K'),
        root_label: view.label(),
        root_link: view.root_link(),
        empty_text: view.empty_text(),
        crumbs,
        message,
        rows,
    };
    respond(&w, &headers, status, inner, view.key())
}

fn server_error(e: impl std::fmt::Debug) -> Response {
    tracing::error!(error = ?e, "web request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

// ---------------------------------------------------------------- share dialog fragment

struct PermOption {
    value: u32,
    label: String,
    selected: bool,
}

struct ShareEntry {
    id: i64,
    label: String,
    is_group: bool,
    options: Vec<PermOption>,
}

#[derive(Template)]
#[template(path = "share_dialog.html")]
struct ShareDialog {
    path: String,
    name: String,
    can_share: bool,
    shares: Vec<ShareEntry>,
    add_options: Vec<PermOption>,
}

fn perm_presets(is_dir: bool) -> [(u32, &'static str); 2] {
    if is_dir {
        [(1, "Can view"), (15, "Can edit")]
    } else {
        [(1, "Can view"), (3, "Can edit")]
    }
}

fn perm_options(is_dir: bool, current: Option<u32>) -> Vec<PermOption> {
    let mut v: Vec<PermOption> = perm_presets(is_dir)
        .into_iter()
        .map(|(value, l)| PermOption {
            value,
            label: l.to_string(),
            selected: current == Some(value),
        })
        .collect();
    if let Some(c) = current
        && !v.iter().any(|o| o.value == c)
    {
        v.push(PermOption {
            value: c,
            label: format!("Custom ({c})"),
            selected: true,
        });
    }
    v
}

#[derive(Deserialize)]
struct PathQuery {
    path: String,
}

async fn share_dialog(State(st): State<AppState>, w: Web, Query(q): Query<PathQuery>) -> Response {
    let Ok(path) = paths::normalize(&q.path) else {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    };
    let me = owner(&w.user);
    let res = match st.store.resolve(&me, &path).await {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let node = match st.store.get(&res.owner, &res.path).await {
        Ok(Some(n)) => n,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => return server_error(e),
    };
    let can_share = !res.is_shared() && !path.is_empty();
    let rows: Vec<(i64, i64, String, String, i64)> = if can_share {
        match sqlx::query_as(
            "SELECT s.id, s.share_type, s.share_with, COALESCE(u.display_name, s.share_with), s.permissions \
             FROM shares s LEFT JOIN users u ON s.share_type = 0 AND u.uid = s.share_with \
             WHERE s.owner_id = $1 AND s.file_source = $2 ORDER BY s.id",
        )
        .bind(me.id)
        .bind(node.fileid)
        .fetch_all(st.store.pool())
        .await
        {
            Ok(r) => r,
            Err(e) => return server_error(e),
        }
    } else {
        Vec::new()
    };
    let shares = rows
        .into_iter()
        .map(|(id, ty, with, display, perms)| ShareEntry {
            id,
            label: if ty == 0 && display != with {
                format!("{display} ({with})")
            } else {
                display
            },
            is_group: ty == 1,
            options: perm_options(node.is_dir, Some(perms as u32)),
        })
        .collect();
    render(
        StatusCode::OK,
        &ShareDialog {
            path: format!("/{path}"),
            name: node.name.clone(),
            can_share,
            shares,
            add_options: perm_options(node.is_dir, None),
        },
        CSP_APP,
    )
}

// ---------------------------------------------------------------- trash and versions views

struct TrashRow {
    entry: String,
    name: String,
    location: String,
    icon: &'static str,
    size: String,
    deleted: String,
}

#[derive(Template)]
#[template(path = "trash.html")]
struct TrashPage {
    chrome: Chrome,
    nav: Vec<NavItem>,
    rows: Vec<TrashRow>,
    retention: u32,
}

async fn trash_page(State(st): State<AppState>, w: Web) -> Response {
    let items = match st.store.trash_list(&owner(&w.user)).await {
        Ok(i) => i,
        Err(e) => return server_error(e),
    };
    let rows = items
        .into_iter()
        .map(|i| TrashRow {
            entry: i.entry_name(),
            location: format!("/{}", i.orig_dir),
            icon: icon_for(&i.mimetype, i.is_dir),
            size: human_size(i.size),
            deleted: fmt_time(i.deleted_at),
            name: i.name,
        })
        .collect();
    render(
        StatusCode::OK,
        &TrashPage {
            chrome: Chrome::new(&w),
            nav: nav_items("trash"),
            rows,
            retention: st.config.trash_retention_days,
        },
        CSP_APP,
    )
}

struct VersionRow {
    ts: i64,
    when: String,
    size: String,
    current: bool,
    download: String,
}

#[derive(Template)]
#[template(path = "versions_dialog.html")]
struct VersionsDialog {
    id: i64,
    path: String,
    name: String,
    versions: Vec<VersionRow>,
}

async fn versions_dialog(
    State(st): State<AppState>,
    w: Web,
    Query(q): Query<PathQuery>,
) -> Response {
    let Ok(path) = paths::normalize(&q.path) else {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    };
    let me = owner(&w.user);
    // versions are kept per owner: only files of the user's own tree
    let node = match st.store.get(&me, &path).await {
        Ok(Some(n)) if !n.is_dir => n,
        Ok(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => return server_error(e),
    };
    let list = match st.store.versions_list(&me, &node).await {
        Ok(l) => l,
        Err(e) => return server_error(e),
    };
    let versions = list
        .into_iter()
        .rev()
        .map(|v| VersionRow {
            ts: v.ts,
            when: fmt_time(v.ts),
            size: human_size(v.size),
            current: v.current,
            download: format!(
                "/remote.php/dav/versions/{}/versions/{}/{}",
                enc(&w.user.uid),
                node.fileid,
                v.ts
            ),
        })
        .collect();
    render(
        StatusCode::OK,
        &VersionsDialog {
            id: node.fileid,
            path: format!("/{path}"),
            name: node.name.clone(),
            versions,
        },
        CSP_APP,
    )
}

// ---------------------------------------------------------------- flash messages (fixed texts only)

struct Flash {
    kind: &'static str,
    text: &'static str,
}

fn flash(q: &HashMap<String, String>) -> Option<Flash> {
    let (kind, key) = match (q.get("ok"), q.get("err")) {
        (Some(k), _) => ("ok", k.as_str()),
        (None, Some(k)) => ("err", k.as_str()),
        _ => return None,
    };
    let text = match (kind, key) {
        ("ok", "profile") => "Profile saved.",
        ("ok", "password") => "Password changed.",
        ("ok", "revoked") => "App password removed: that device is signed out.",
        ("ok", "created") => "Account created.",
        ("ok", "saved") => "Account saved.",
        ("ok", "group") => "Group saved.",
        ("err", "email") => "That does not look like an email address.",
        ("err", "name") => "The display name must be 1 to 64 characters.",
        ("err", "badpw") => "The current password is wrong.",
        ("err", "mismatch") => "The two new passwords differ (or are empty).",
        ("err", "baduid") => {
            "Account names use letters, digits, space and _ . @ - (1-64 characters, no leading dot)."
        }
        ("err", "exists") => "That name is already taken.",
        ("err", "lastadmin") => "There must always be at least one enabled administrator.",
        ("err", "nouser") => "No such user.",
        ("err", _) => "That did not work.",
        _ => return None,
    };
    Some(Flash { kind, text })
}

fn err_key(e: &anyhow::Error) -> &'static str {
    use crate::db::UserError as U;
    match crate::db::user_error(e) {
        Some(U::InvalidEmail) => "email",
        Some(U::InvalidName) => "name",
        Some(U::LastAdmin) => "lastadmin",
        Some(U::NoSuchUser(_)) => "nouser",
        Some(U::InvalidUid) => "baduid",
        Some(U::Exists) => "exists",
        Some(U::EmptyPassword) | None => "failed",
    }
}

// ---------------------------------------------------------------- personal settings

struct AppPw {
    id: i64,
    name: String,
    created: String,
    last_used: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    chrome: Chrome,
    flash: Option<Flash>,
    display_name: String,
    email: String,
    app_passwords: Vec<AppPw>,
    new_token: Option<String>,
    new_token_name: String,
}

async fn settings_view(
    st: &AppState,
    w: &Web,
    flash: Option<Flash>,
    new: Option<(String, String)>,
) -> Response {
    let u = match db::get_user(&st.pool, &w.user.uid).await {
        Ok(Some(u)) => u,
        Ok(None) => return redirect("/login"),
        Err(e) => return server_error(e),
    };
    let rows: Vec<(i64, String, i64, Option<i64>)> = match sqlx::query_as(
        "SELECT id, name, created_at, last_used_at FROM app_passwords WHERE user_id = $1 ORDER BY id DESC",
    )
    .bind(w.user.id)
    .fetch_all(&st.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let (new_token, new_token_name) = match new {
        Some((t, n)) => (Some(t), n),
        None => (None, String::new()),
    };
    render(
        StatusCode::OK,
        &SettingsPage {
            chrome: Chrome::new(w),
            flash,
            display_name: u.display_name,
            email: u.email,
            app_passwords: rows
                .into_iter()
                .map(|(id, name, c, l)| AppPw {
                    id,
                    name,
                    created: fmt_time(c),
                    last_used: l.map_or_else(|| "never".into(), fmt_time),
                })
                .collect(),
            new_token,
            new_token_name,
        },
        CSP_APP,
    )
}

async fn settings(
    State(st): State<AppState>,
    w: Web,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    settings_view(&st, &w, flash(&q), None).await
}

#[derive(Deserialize)]
struct ProfileForm {
    csrf: String,
    display_name: String,
    email: String,
}

async fn settings_profile(
    State(st): State<AppState>,
    w: Web,
    Form(f): Form<ProfileForm>,
) -> Response {
    if let Err(r) = w.check(&f.csrf) {
        return r;
    }
    let edit = UserEdit {
        display_name: Some(f.display_name),
        email: Some(f.email.trim().to_string()),
        ..Default::default()
    };
    match db::update_user(&st.pool, &w.user.uid, &edit).await {
        Ok(()) => redirect("/settings/user?ok=profile"),
        Err(e) => redirect(&format!("/settings/user?err={}", err_key(&e))),
    }
}

#[derive(Deserialize)]
struct PasswordForm {
    csrf: String,
    current: String,
    new: String,
    again: String,
}

async fn settings_password(
    State(st): State<AppState>,
    w: Web,
    ClientIp(ip): ClientIp,
    Form(f): Form<PasswordForm>,
) -> Response {
    if let Err(r) = w.check(&f.csrf) {
        return r;
    }
    if f.new.is_empty() || f.new != f.again {
        return redirect("/settings/user?err=mismatch");
    }
    // the current password is checked like a login: same throttle, same timing
    if st.auth_gate(ip).is_err() {
        return (StatusCode::TOO_MANY_REQUESTS, "Too many failed attempts").into_response();
    }
    match auth::verify_user_password(&st.pool, &w.user.uid, &f.current).await {
        Ok(Some(_)) => st.auth_ok(ip),
        Ok(None) => {
            st.auth_failed(ip);
            return redirect("/settings/user?err=badpw");
        }
        Err(e) => return server_error(e),
    }
    match db::update_user(
        &st.pool,
        &w.user.uid,
        &UserEdit {
            password: Some(f.new),
            keep_session: Some(sha256_hex(&w.sid)),
            ..Default::default()
        },
    )
    .await
    {
        Ok(()) => redirect("/settings/user?ok=password"),
        Err(e) => redirect(&format!("/settings/user?err={}", err_key(&e))),
    }
}

#[derive(Deserialize)]
struct NameForm {
    csrf: String,
    name: String,
}

async fn app_password_create(
    State(st): State<AppState>,
    w: Web,
    Form(f): Form<NameForm>,
) -> Response {
    if let Err(r) = w.check(&f.csrf) {
        return r;
    }
    let name: String = f.name.trim().chars().take(100).collect();
    if name.is_empty() {
        return redirect("/settings/user?err=failed");
    }
    match auth::create_app_password(&st.pool, w.user.id, &name).await {
        Ok(token) => settings_view(&st, &w, None, Some((token, name))).await,
        Err(e) => server_error(e),
    }
}

async fn app_password_delete(
    State(st): State<AppState>,
    w: Web,
    Path(id): Path<i64>,
    Form(f): Form<CsrfOnly>,
) -> Response {
    if let Err(r) = w.check(&f.csrf) {
        return r;
    }
    match sqlx::query("DELETE FROM app_passwords WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(w.user.id)
        .execute(&st.pool)
        .await
    {
        Ok(_) => redirect("/settings/user?ok=revoked"),
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------- administration

struct UserLine {
    uid: String,
    uid_enc: String,
    display_name: String,
    email: String,
    is_admin: bool,
    enabled: bool,
}

struct MemberLine {
    uid_enc: String,
    label: String,
}

struct Candidate {
    uid: String,
    label: String,
}

struct GroupLine {
    gid: String,
    gid_enc: String,
    members: Vec<MemberLine>,
    candidates: Vec<Candidate>,
}

#[derive(Template)]
#[template(path = "about.html")]
struct AboutPage {
    chrome: Chrome,
    nextcloud: String,
    uptime: String,
}

/// "3 days 4 h 12 min" / "12 min" / "45 s"
fn fmt_uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{secs} s"),
        (0, 0, m) => format!("{m} min"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, h, m) => format!("{d} days {h} h {m} min"),
    }
}

/// Version and build number of the running server (admins only, like the other pages under /settings/users).
async fn about_page(State(st): State<AppState>, Admin(w): Admin) -> Response {
    use crate::routes::{NC_MAJOR, NC_MICRO, NC_MINOR};
    render(
        StatusCode::OK,
        &AboutPage {
            chrome: Chrome::new(&w),
            nextcloud: format!("{NC_MAJOR}.{NC_MINOR}.{NC_MICRO}"),
            uptime: fmt_uptime(st.metrics.uptime_secs()),
        },
        CSP_APP,
    )
}

#[derive(Template)]
#[template(path = "users.html")]
struct UsersPage {
    chrome: Chrome,
    flash: Option<Flash>,
    users: Vec<UserLine>,
    groups: Vec<GroupLine>,
}

async fn users_page(
    State(st): State<AppState>,
    Admin(w): Admin,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let users = match db::list_users(&st.pool).await {
        Ok(u) => u,
        Err(e) => return server_error(e),
    };
    let members: Vec<(String, Option<String>)> = match sqlx::query_as(
        "SELECT g.gid, u.uid FROM groups g LEFT JOIN group_members m ON m.group_id = g.id \
         LEFT JOIN users u ON u.id = m.user_id ORDER BY g.gid, u.uid",
    )
    .fetch_all(&st.pool)
    .await
    {
        Ok(m) => m,
        Err(e) => return server_error(e),
    };
    let label = |uid: &str| {
        users.iter().find(|u| u.uid == uid).map_or_else(
            || uid.to_string(),
            |u| format!("{} ({})", u.display_name, u.uid),
        )
    };
    let mut groups: Vec<GroupLine> = Vec::new();
    for (gid, uid) in &members {
        if groups.last().is_none_or(|g| g.gid != *gid) {
            groups.push(GroupLine {
                gid: gid.clone(),
                gid_enc: enc(gid),
                members: vec![],
                candidates: vec![],
            });
        }
        if let Some(uid) = uid
            && let Some(g) = groups.last_mut()
        {
            g.members.push(MemberLine {
                uid_enc: enc(uid),
                label: label(uid),
            });
        }
    }
    for g in &mut groups {
        let inside: Vec<String> = members
            .iter()
            .filter(|(gid, _)| *gid == g.gid)
            .filter_map(|(_, u)| u.clone())
            .collect();
        g.candidates = users
            .iter()
            .filter(|u| u.enabled && !inside.contains(&u.uid))
            .map(|u| Candidate {
                uid: u.uid.clone(),
                label: format!("{} ({})", u.display_name, u.uid),
            })
            .collect();
    }
    let lines = users
        .into_iter()
        .map(|u| UserLine {
            uid_enc: enc(&u.uid),
            uid: u.uid,
            display_name: u.display_name,
            email: u.email,
            is_admin: u.is_admin,
            enabled: u.enabled,
        })
        .collect();
    render(
        StatusCode::OK,
        &UsersPage {
            chrome: Chrome::new(&w),
            flash: flash(&q),
            users: lines,
            groups,
        },
        CSP_APP,
    )
}

#[derive(Deserialize)]
struct NewUserForm {
    csrf: String,
    uid: String,
    display_name: String,
    email: String,
    password: String,
    is_admin: Option<String>,
}

async fn user_create(
    State(st): State<AppState>,
    Admin(w): Admin,
    Form(f): Form<NewUserForm>,
) -> Response {
    if let Err(r) = w.check(&f.csrf) {
        return r;
    }
    let uid = f.uid.trim().to_string();
    let display = if f.display_name.trim().is_empty() {
        uid.clone()
    } else {
        f.display_name.trim().to_string()
    };
    if let Err(e) =
        db::create_user(&st.pool, &uid, &display, &f.password, f.is_admin.is_some()).await
    {
        return redirect(&format!("/settings/users?err={}", err_key(&e)));
    }
    if !f.email.trim().is_empty()
        && let Err(e) = db::update_user(
            &st.pool,
            &uid,
            &UserEdit {
                email: Some(f.email.trim().to_string()),
                ..Default::default()
            },
        )
        .await
    {
        return redirect(&format!("/settings/users?err={}", err_key(&e)));
    }
    redirect("/settings/users?ok=created")
}

#[derive(Deserialize)]
struct EditUserForm {
    csrf: String,
    display_name: String,
    email: String,
    password: String,
    is_admin: Option<String>,
    enabled: Option<String>,
}

async fn user_edit(
    State(st): State<AppState>,
    Admin(w): Admin,
    Path(uid): Path<String>,
    Form(f): Form<EditUserForm>,
) -> Response {
    if let Err(r) = w.check(&f.csrf) {
        return r;
    }
    let edit = UserEdit {
        display_name: Some(f.display_name),
        email: Some(f.email.trim().to_string()),
        password: (!f.password.is_empty()).then_some(f.password),
        is_admin: Some(f.is_admin.is_some()),
        enabled: Some(f.enabled.is_some()),
        // an administrator changing their own password here keeps their own session
        keep_session: (uid == w.user.uid).then(|| sha256_hex(&w.sid)),
    };
    match db::update_user(&st.pool, &uid, &edit).await {
        Ok(()) => redirect("/settings/users?ok=saved"),
        Err(e) => redirect(&format!("/settings/users?err={}", err_key(&e))),
    }
}

#[derive(Deserialize)]
struct GroupForm {
    csrf: String,
    gid: String,
}

async fn group_create(
    State(st): State<AppState>,
    Admin(w): Admin,
    Form(f): Form<GroupForm>,
) -> Response {
    if let Err(r) = w.check(&f.csrf) {
        return r;
    }
    let gid = f.gid.trim();
    if gid.is_empty() || gid.chars().count() > 64 || gid.contains('/') {
        return redirect("/settings/users?err=failed");
    }
    match db::create_group(&st.pool, gid).await {
        Ok(()) => redirect("/settings/users?ok=group"),
        Err(e) => redirect(&format!("/settings/users?err={}", err_key(&e))),
    }
}

#[derive(Deserialize)]
struct MemberForm {
    csrf: String,
    uid: String,
}

async fn group_add_member(
    State(st): State<AppState>,
    Admin(w): Admin,
    Path(gid): Path<String>,
    Form(f): Form<MemberForm>,
) -> Response {
    if let Err(r) = w.check(&f.csrf) {
        return r;
    }
    match db::add_member(&st.pool, &gid, &f.uid).await {
        Ok(()) => redirect("/settings/users?ok=group"),
        Err(e) => redirect(&format!("/settings/users?err={}", err_key(&e))),
    }
}

async fn group_remove_member(
    State(st): State<AppState>,
    Admin(w): Admin,
    Path((gid, uid)): Path<(String, String)>,
    Form(f): Form<CsrfOnly>,
) -> Response {
    if let Err(r) = w.check(&f.csrf) {
        return r;
    }
    match sqlx::query(
        "DELETE FROM group_members WHERE group_id = (SELECT id FROM groups WHERE gid = $1) \
         AND user_id = (SELECT id FROM users WHERE uid = $2)",
    )
    .bind(&gid)
    .bind(&uid)
    .execute(&st.pool)
    .await
    {
        Ok(_) => redirect("/settings/users?ok=group"),
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------- static assets

async fn static_asset(Path(name): Path<String>) -> Response {
    let (body, ct): (&'static [u8], &'static str) = match name.as_str() {
        "app.css" => (
            include_bytes!("../static/app.css"),
            "text/css; charset=utf-8",
        ),
        "app.js" => (
            include_bytes!("../static/app.js"),
            "text/javascript; charset=utf-8",
        ),
        "htmx.min.js" => (
            include_bytes!("../static/htmx.min.js"),
            "text/javascript; charset=utf-8",
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "public, max-age=3600"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_times() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(fmt_time(0), "-");
        assert_eq!(fmt_time(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(fmt_time(951_782_400), "2000-02-29 00:00"); // leap day
    }

    #[test]
    fn redirects_stay_on_this_site() {
        assert_eq!(
            safe_redirect("/apps/files/?dir=%2Fa"),
            "/apps/files/?dir=%2Fa"
        );
        for bad in [
            "/\t/evil.example",
            "/\u{b}/evil.example",
            "/\r/evil.example",
            "/a\u{7f}b",
            "//evil.example",
            "https://evil.example",
            "/\\evil",
            "javascript:alert(1)",
            "",
            "/x\nSet-Cookie: a=b",
        ] {
            assert_eq!(safe_redirect(bad), "/apps/files/", "{bad:?}");
        }
    }

    #[test]
    fn urls_are_percent_encoded() {
        assert_eq!(enc("a b&c/d\"'<>"), "a%20b%26c%2Fd%22%27%3C%3E");
        assert_eq!(
            enc_path("/Ünï cöde/x y.txt"),
            "%C3%9Cn%C3%AF%20c%C3%B6de/x%20y.txt"
        );
    }
}
