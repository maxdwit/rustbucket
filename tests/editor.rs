//! The live editor: rooms over a real WebSocket (two writers converge, readers cannot write, access and origin
//! checks, autosave through the normal PUT path, outside changes, limits). The client side is `yrs` itself.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use rustbucket::auth::csrf_token_for;
use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::editor::Editors;
use rustbucket::routes::{AppState, router};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message as Ws};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tower::ServiceExt;
use yrs::sync::protocol::MessageReader;
use yrs::sync::{Awareness, DefaultProtocol, Message, Protocol, SyncMessage};
use yrs::updates::decoder::DecoderV1;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, Text, Transact};

struct Env {
    pool: sqlx::SqlitePool,
    _dir: tempfile::TempDir,
    app: Router,
    addr: SocketAddr,
    data: std::path::PathBuf,
}

async fn env_with(cfg: impl FnOnce(&mut Config), idle_ms: u64) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect(&format!("sqlite://{}", dir.path().join("t.db").display()))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    for u in ["alice", "bob", "carol", "dave"] {
        db::create_user(&pool, u, u, &format!("pw-{u}"), false)
            .await
            .unwrap();
    }
    let data = dir.path().join("data");
    let mut c = Config {
        data_dir: data.clone(),
        ..Config::default()
    };
    cfg(&mut c);
    let mut state = AppState::new(pool.clone(), Arc::new(c));
    state.editors = Arc::new(Editors::with_timing(
        Duration::from_millis(idle_ms),
        Duration::from_millis(idle_ms),
    ));
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            served.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Env {
        pool,
        _dir: dir,
        app,
        addr,
        data,
    }
}

async fn env() -> Env {
    env_with(|_| {}, 150).await
}

async fn send(
    e: &Env,
    method: &str,
    uri: &str,
    hdrs: &[(&str, String)],
    body: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut b = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri);
    for (k, v) in hdrs {
        b = b.header(*k, v.as_str());
    }
    let r = e
        .app
        .clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let (st, h) = (r.status(), r.headers().clone());
    let body =
        String::from_utf8_lossy(&r.into_body().collect().await.unwrap().to_bytes()).into_owned();
    (st, h, body)
}

fn basic(u: &str) -> (&'static str, String) {
    (
        "authorization",
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{u}:pw-{u}"))
        ),
    )
}

async fn dav(e: &Env, user: &str, method: &str, path: &str, body: &str) -> StatusCode {
    send(
        e,
        method,
        &format!("/remote.php/dav/files/{user}{path}"),
        &[basic(user)],
        body,
    )
    .await
    .0
}

async fn share(e: &Env, owner: &str, path: &str, with: &str, perms: &str) {
    let body = format!("shareType=0&shareWith={with}&path={path}&permissions={perms}");
    let (st, _, b) = send(
        e,
        "POST",
        "/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json",
        &[
            basic(owner),
            (
                "content-type",
                "application/x-www-form-urlencoded".to_string(),
            ),
        ],
        &body,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
}

/// A logged-in browser session id.
async fn session(e: &Env, user: &str) -> String {
    let (_, h, _) = send(e, "GET", "/login", &[], "").await;
    let csrf = h
        .get_all("set-cookie")
        .iter()
        .find_map(|c| {
            c.to_str()
                .unwrap()
                .strip_prefix("rb_csrf=")
                .map(|v| v.split(';').next().unwrap().to_string())
        })
        .unwrap();
    let (st, h, b) = send(
        e,
        "POST",
        "/login",
        &[
            (
                "content-type",
                "application/x-www-form-urlencoded".to_string(),
            ),
            ("cookie", format!("rb_csrf={csrf}")),
        ],
        &format!("username={user}&password=pw-{user}&csrf={csrf}"),
    )
    .await;
    assert_eq!(st, StatusCode::SEE_OTHER, "{b}");
    h.get_all("set-cookie")
        .iter()
        .find_map(|c| {
            c.to_str()
                .unwrap()
                .strip_prefix("rb_session=")
                .map(|v| v.split(';').next().unwrap().to_string())
        })
        .unwrap()
}

async fn fileid(e: &Env, user: &str, path: &str) -> i64 {
    let (_, _, b) = send(
        e,
        "PROPFIND",
        &format!("/remote.php/dav/files/{user}{path}"),
        &[basic(user), ("depth", "0".to_string())],
        r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:fileid/></d:prop></d:propfind>"#,
    )
    .await;
    let i = b.find("fileid>").unwrap() + 7;
    b[i..i + b[i..].find('<').unwrap()].parse().unwrap()
}

async fn read(e: &Env, user: &str, path: &str) -> String {
    send(
        e,
        "GET",
        &format!("/remote.php/dav/files/{user}{path}"),
        &[basic(user)],
        "",
    )
    .await
    .2
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    aw: Awareness,
    notices: Vec<String>,
    closed: Option<u16>,
}

async fn try_connect(
    e: &Env,
    sid: &str,
    id: i64,
    origin: Option<String>,
    csrf: Option<String>,
) -> Result<Client, StatusCode> {
    let csrf = csrf.unwrap_or_else(|| csrf_token_for(sid));
    try_connect_cookie(e, format!("rb_session={sid}"), id, origin, csrf).await
}

async fn try_connect_cookie(
    e: &Env,
    cookie: String,
    id: i64,
    origin: Option<String>,
    csrf: String,
) -> Result<Client, StatusCode> {
    let mut req = format!("ws://{}/apps/files/edit/ws?id={id}&csrf={csrf}", e.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    req.headers_mut().insert(
        "origin",
        origin
            .unwrap_or_else(|| format!("http://{}", e.addr))
            .parse()
            .unwrap(),
    );
    match connect_async(req).await {
        Ok((ws, _)) => {
            let mut c = Client {
                ws,
                aw: Awareness::new(Doc::new()),
                notices: vec![],
                closed: None,
            };
            c.aw.doc().get_or_insert_text("t");
            let sv = c.aw.doc().transact().state_vector();
            c.send(&Message::Sync(SyncMessage::SyncStep1(sv))).await;
            Ok(c)
        }
        Err(WsError::Http(r)) => Err(r.status()),
        Err(other) => panic!("connect: {other}"),
    }
}

async fn connect(e: &Env, sid: &str, id: i64) -> Client {
    try_connect(e, sid, id, None, None).await.unwrap()
}

impl Client {
    async fn send(&mut self, m: &Message) {
        let _ = self.ws.send(Ws::Binary(m.encode_v1().into())).await;
    }

    /// Process what arrives for `ms` milliseconds.
    async fn pump(&mut self, ms: u64) {
        let end = tokio::time::Instant::now() + Duration::from_millis(ms);
        while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(end, self.ws.next()).await {
            match msg {
                Ws::Binary(b) => self.handle(&b).await,
                Ws::Close(f) => {
                    self.closed = f.map(|f| u16::from(f.code));
                    return;
                }
                _ => {}
            }
        }
    }

    async fn handle(&mut self, data: &[u8]) {
        let mut dec = DecoderV1::new(yrs::encoding::read::Cursor::new(data));
        let mut replies = vec![];
        for m in MessageReader::new(&mut dec) {
            match m.unwrap() {
                Message::Custom(_, d) => self.notices.push(String::from_utf8(d).unwrap()),
                other => {
                    if let Some(r) = DefaultProtocol.handle_message(&mut self.aw, other).unwrap() {
                        replies.push(r);
                    }
                }
            }
        }
        for r in replies {
            self.send(&r).await;
        }
    }

    fn text(&self) -> String {
        let doc = self.aw.doc();
        let txn = doc.transact();
        txn.get_text("t")
            .map(|t| t.get_string(&txn))
            .unwrap_or_default()
    }

    /// Type `s` at `at` and send the change.
    async fn type_at(&mut self, at: u32, s: &str) {
        let doc = self.aw.doc().clone();
        let sv = doc.transact().state_vector();
        {
            let t = doc.get_or_insert_text("t");
            t.insert(&mut doc.transact_mut(), at, s);
        }
        let update = doc.transact().encode_state_as_update_v1(&sv);
        self.send(&Message::Sync(SyncMessage::Update(update))).await;
    }

    fn saw(&self, kind: &str) -> bool {
        self.notices
            .iter()
            .any(|n| n.contains(&format!("\"t\":\"{kind}\"")))
    }
}

async fn setup() -> (Env, i64) {
    let e = env().await;
    assert!(
        dav(&e, "alice", "PUT", "/note.md", "hello")
            .await
            .is_success()
    );
    let id = fileid(&e, "alice", "/note.md").await;
    (e, id)
}

#[tokio::test]
async fn two_writers_converge_and_the_file_is_saved_like_a_put() {
    let (e, id) = setup().await;
    share(&e, "alice", "/note.md", "bob", "3").await;
    let (sa, sb) = (session(&e, "alice").await, session(&e, "bob").await);
    let mut a = connect(&e, &sa, id).await;
    let mut b = connect(&e, &sb, id).await;
    a.pump(300).await;
    b.pump(300).await;
    assert_eq!((a.text(), b.text()), ("hello".into(), "hello".into()));
    assert!(a.saw("hello") && b.saw("hello"));

    let etag_before = send(
        &e,
        "PROPFIND",
        "/remote.php/dav/files/alice/",
        &[basic("alice"), ("depth", "0".into())],
        "",
    )
    .await
    .2;
    a.type_at(5, " world").await;
    b.type_at(0, ">> ").await;
    a.pump(500).await;
    b.pump(500).await;
    assert_eq!(a.text(), b.text());
    assert!(
        a.text().contains("world") && a.text().starts_with(">> "),
        "{}",
        a.text()
    );

    // autosave (150 ms idle in this test): the file on disk and over DAV has the merged text, parent etag moved
    tokio::time::sleep(Duration::from_millis(900)).await;
    a.pump(100).await;
    assert!(a.saw("saved"));
    assert_eq!(read(&e, "alice", "/note.md").await, a.text());
    assert_eq!(
        std::fs::read_to_string(e.data.join("alice/files/note.md")).unwrap(),
        a.text()
    );
    let etag_after = send(
        &e,
        "PROPFIND",
        "/remote.php/dav/files/alice/",
        &[basic("alice"), ("depth", "0".into())],
        "",
    )
    .await
    .2;
    assert_ne!(
        etag_before, etag_after,
        "the root etag must move so sync clients notice"
    );
    // the previous content is a version
    let (st, _, vers) = send(
        &e,
        "PROPFIND",
        &format!("/remote.php/dav/versions/alice/versions/{id}"),
        &[basic("alice"), ("depth", "1".into())],
        "",
    )
    .await;
    assert!(
        st.is_success() && vers.matches("<d:response>").count() >= 2,
        "{st} {vers}"
    );
}

#[tokio::test]
async fn a_reader_sees_changes_but_cannot_make_any() {
    let (e, id) = setup().await;
    share(&e, "alice", "/note.md", "carol", "1").await; // read only
    let (sa, sc) = (session(&e, "alice").await, session(&e, "carol").await);
    let mut a = connect(&e, &sa, id).await;
    let mut c = connect(&e, &sc, id).await;
    a.pump(200).await;
    c.pump(200).await;
    assert!(c.notices.iter().any(|n| n.contains("\"edit\":false")));
    a.type_at(5, "!").await;
    c.pump(300).await;
    assert_eq!(c.text(), "hello!", "the reader receives live changes");
    // the reader forces an update frame anyway
    c.type_at(0, "EVIL ").await;
    a.pump(400).await;
    c.pump(100).await;
    assert_eq!(
        a.text(),
        "hello!",
        "the writer must not see the reader's change"
    );
    drop(c);
    drop(a);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(read(&e, "alice", "/note.md").await, "hello!");
}

#[tokio::test]
async fn access_origin_and_type_checks() {
    let (e, id) = setup().await;
    assert!(dav(&e, "alice", "PUT", "/pic.png", "x").await.is_success());
    let pic = fileid(&e, "alice", "/pic.png").await;
    let sa = session(&e, "alice").await;
    let sd = session(&e, "dave").await;
    // dave has nothing shared with him
    assert_eq!(
        try_connect(&e, &sd, id, None, None).await.err(),
        Some(StatusCode::NOT_FOUND)
    );
    // not a text file type
    assert_eq!(
        try_connect(&e, &sa, pic, None, None).await.err(),
        Some(StatusCode::NOT_FOUND)
    );
    // a page from another site (cross-site WebSocket hijacking), no origin, a wrong token
    assert_eq!(
        try_connect(&e, &sa, id, Some("http://evil.example".into()), None)
            .await
            .err(),
        Some(StatusCode::FORBIDDEN)
    );
    assert_eq!(
        try_connect(&e, &sa, id, None, Some("nope".into()))
            .await
            .err(),
        Some(StatusCode::FORBIDDEN)
    );
    // no login at all
    let r = send(
        &e,
        "GET",
        &format!("/apps/files/edit/ws?id={id}&csrf=x"),
        &[],
        "",
    )
    .await;
    assert_eq!(r.0, StatusCode::SEE_OTHER);
    // the page: 404 for the stranger, 200 for the owner, with the edit flag
    let (st, _, _) = send(
        &e,
        "GET",
        "/apps/files/edit?path=/note.md",
        &[("cookie", format!("rb_session={sd}"))],
        "",
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, h, body) = send(
        &e,
        "GET",
        "/apps/files/edit?path=/note.md",
        &[("cookie", format!("rb_session={sa}"))],
        "",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.contains(&format!("data-id=\"{id}\"")) && body.contains("data-edit=\"1\""));
    let csp = h.get("content-security-policy").unwrap().to_str().unwrap();
    assert!(
        csp.contains("script-src 'self';") && csp.contains("'nonce-"),
        "{csp}"
    );
}

#[tokio::test]
async fn an_outside_change_is_adopted_and_a_dirty_room_keeps_the_edit() {
    let (e, id) = setup().await;
    let sa = session(&e, "alice").await;
    let mut a = connect(&e, &sa, id).await;
    a.pump(200).await;
    // somebody writes the file over WebDAV (a sync client): the open room takes it over
    assert!(
        dav(&e, "alice", "PUT", "/note.md", "from the desktop client")
            .await
            .is_success()
    );
    a.pump(900).await;
    assert_eq!(a.text(), "from the desktop client");
    assert!(a.saw("external"));
    // an edit made now is saved on top of it, and the outside version stays reachable as a version
    a.type_at(0, "A: ").await;
    a.pump(900).await;
    assert_eq!(
        read(&e, "alice", "/note.md").await,
        "A: from the desktop client"
    );
}

#[tokio::test]
async fn limits_and_bad_input() {
    let e = env_with(|c| c.editor_max_bytes = 16, 150).await;
    assert!(
        dav(&e, "alice", "PUT", "/big.md", &"x".repeat(100))
            .await
            .is_success()
    );
    assert!(dav(&e, "alice", "PUT", "/bin.txt", "").await.is_success());
    let binary = send(
        &e,
        "PUT",
        "/remote.php/dav/files/alice/latin.txt",
        &[basic("alice")],
        "",
    )
    .await;
    assert!(binary.0.is_success());
    std::fs::write(e.data.join("alice/files/latin.txt"), [0xff, 0xfe, 0x41]).unwrap();
    let sa = session(&e, "alice").await;
    for name in ["/big.md", "/latin.txt"] {
        let id = fileid(&e, "alice", name).await;
        let mut c = connect(&e, &sa, id).await;
        c.pump(300).await;
        assert!(c.saw("refused"), "{name}: {:?}", c.notices);
        assert_eq!(c.closed, Some(1008), "{name}");
    }
    // hostile bytes close that connection, not the server
    assert!(dav(&e, "alice", "PUT", "/ok.md", "ok").await.is_success());
    let id = fileid(&e, "alice", "/ok.md").await;
    let mut c = connect(&e, &sa, id).await;
    c.pump(200).await;
    c.ws.send(Ws::Binary(vec![0, 2, 3, 255, 255, 255].into()))
        .await
        .unwrap();
    c.pump(300).await;
    assert_eq!(c.closed, Some(1007));
    let mut again = connect(&e, &sa, id).await;
    again.pump(300).await;
    assert_eq!(again.text(), "ok");
    // growing past the limit stops the writer and the file keeps its last good content
    again.type_at(2, &"y".repeat(40)).await;
    again.pump(500).await;
    assert_eq!(again.closed, Some(1009));
    drop(again);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(read(&e, "alice", "/ok.md").await, "ok");
    // switched off: 404
    let off = env_with(|c| c.editor = false, 150).await;
    let s = session(&off, "alice").await;
    let (st, _, _) = send(
        &off,
        "GET",
        "/apps/files/edit?path=/x.md",
        &[("cookie", format!("rb_session={s}"))],
        "",
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_files_list_offers_edit_or_view() {
    let (e, _) = setup().await;
    share(&e, "alice", "/note.md", "carol", "1").await;
    share(&e, "alice", "/note.md", "bob", "3").await;
    for (user, label) in [("alice", "Edit"), ("bob", "Edit"), ("carol", "View")] {
        let s = session(&e, user).await;
        let (_, _, body) = send(
            &e,
            "GET",
            if user == "alice" {
                "/apps/files/"
            } else {
                "/apps/files/?view=shared-with-me"
            },
            &[("cookie", format!("rb_session={s}"))],
            "",
        )
        .await;
        let want = format!(">{label}</a>");
        // the name itself opens the editor (no download); the file stays downloadable from the menu
        let name_link = body
            .find("<span class=\"text\">note.md</span>")
            .map(|i| &body[..i])
            .unwrap();
        let name_link = &name_link[name_link.rfind("<a ").unwrap()..];
        assert!(
            name_link.contains("/apps/files/edit?path=") && !name_link.contains("download"),
            "{user}: {name_link}"
        );
        assert!(body.contains(">Download</a>"), "{user}");
        assert!(
            body.contains("/apps/files/edit?path=") && body.contains(&want),
            "{user}: {body}"
        );
    }
}

#[tokio::test]
async fn save_and_close_writes_the_file_at_once() {
    // autosave far away (1 minute idle): only the explicit flush can have written the file
    let e = env_with(|_| {}, 60_000).await;
    assert!(
        dav(&e, "alice", "PUT", "/note.md", "hello")
            .await
            .is_success()
    );
    share(&e, "alice", "/note.md", "carol", "1").await;
    let id = fileid(&e, "alice", "/note.md").await;
    let (sa, sc) = (session(&e, "alice").await, session(&e, "carol").await);
    let mut a = connect(&e, &sa, id).await;
    a.pump(200).await;
    a.type_at(5, " there").await;
    a.pump(200).await;
    assert_eq!(
        read(&e, "alice", "/note.md").await,
        "hello",
        "not saved yet"
    );
    a.send(&Message::Custom(101, vec![])).await;
    a.pump(500).await;
    assert!(
        a.notices
            .iter()
            .any(|n| n.contains("\"t\":\"flushed\"") && n.contains("\"ok\":true")),
        "{:?}",
        a.notices
    );
    assert_eq!(read(&e, "alice", "/note.md").await, "hello there");
    // a reader may press it too: nothing to save, still answered with ok
    let mut c = connect(&e, &sc, id).await;
    c.pump(200).await;
    c.send(&Message::Custom(101, vec![])).await;
    c.pump(300).await;
    assert!(
        c.notices
            .iter()
            .any(|n| n.contains("\"t\":\"flushed\"") && n.contains("\"ok\":true"))
    );
}

#[tokio::test]
async fn scripts_open_keep_their_mode_and_binary_files_are_refused() {
    use std::os::unix::fs::PermissionsExt;
    let e = env().await;
    for (name, body) in [
        ("/run.sh", "echo hi\n"),
        ("/x.bash", "ls\n"),
        ("/t.PY", "print(1)\n"),
        ("/c.json", "{}"),
        ("/c.yml", "a: 1\n"),
        ("/c.yaml", "b: 2\n"),
        ("/n.conf", "x=1\n"),
    ] {
        assert!(dav(&e, "alice", "PUT", name, body).await.is_success());
    }
    let sa = session(&e, "alice").await;
    for (name, text) in [
        ("/run.sh", "echo hi\n"),
        ("/x.bash", "ls\n"),
        ("/t.PY", "print(1)\n"),
        ("/c.json", "{}"),
        ("/c.yml", "a: 1\n"),
        ("/c.yaml", "b: 2\n"),
        ("/n.conf", "x=1\n"),
    ] {
        let id = fileid(&e, "alice", name).await;
        let mut c = connect(&e, &sa, id).await;
        c.pump(250).await;
        assert_eq!(c.text(), text, "{name}");
    }
    // an executable script stays executable after the editor saved it
    let path = e.data.join("alice/files/run.sh");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let id = fileid(&e, "alice", "/run.sh").await;
    let mut c = connect(&e, &sa, id).await;
    c.pump(250).await;
    c.type_at(0, "#!/bin/sh\n").await;
    c.pump(900).await;
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "#!/bin/sh\necho hi\n"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o755
    );
    // a "script" that is really binary (NUL byte, valid UTF-8 otherwise) is refused
    assert!(dav(&e, "alice", "PUT", "/fake.py", "x").await.is_success());
    std::fs::write(e.data.join("alice/files/fake.py"), b"abc\0def").unwrap();
    let id = fileid(&e, "alice", "/fake.py").await;
    let mut c = connect(&e, &sa, id).await;
    c.pump(300).await;
    assert!(c.saw("refused"), "{:?}", c.notices);
}

// ---------------------------------------------------------------- direct editing (mobile apps)

async fn ocs(
    e: &Env,
    method: &str,
    path: &str,
    user: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let (st, _, b) = send(
        e,
        method,
        &format!("/ocs/v2.php/apps/files/api/v1/directEditing{path}?format=json"),
        &[
            basic(user),
            ("ocs-apirequest", "true".into()),
            ("content-type", "application/x-www-form-urlencoded".into()),
        ],
        body,
    )
    .await;
    (st, serde_json::from_str(&b).unwrap_or_default())
}

/// The URL from a successful open call; the token is the last path segment.
fn open_url(v: &serde_json::Value) -> String {
    v["ocs"]["data"]["url"].as_str().unwrap().to_string()
}

fn path_of(url: &str) -> String {
    url.strip_prefix("http://")
        .map_or(url.to_string(), |r| r[r.find('/').unwrap()..].to_string())
}

#[tokio::test]
async fn the_mobile_app_gets_a_one_time_url_and_a_session_for_that_one_file() {
    let e = env().await;
    assert!(
        dav(&e, "alice", "PUT", "/SHOPPING.txt", "milch")
            .await
            .is_success()
    );
    assert!(
        dav(&e, "alice", "PUT", "/other.txt", "x")
            .await
            .is_success()
    );
    let id = fileid(&e, "alice", "/SHOPPING.txt").await;
    let other = fileid(&e, "alice", "/other.txt").await;

    // capability and editor list, like stock
    let (st, _, caps) = send(
        &e,
        "GET",
        "/ocs/v2.php/cloud/capabilities?format=json",
        &[
            basic("alice"),
            ("ocs-apirequest", "true".into()),
            ("host", "files.example".into()),
        ],
        "",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let caps: serde_json::Value = serde_json::from_str(&caps).unwrap();
    let de = &caps["ocs"]["data"]["capabilities"]["files"]["directEditing"];
    assert_eq!(de["supportsFileId"], true);
    assert!(
        de["url"]
            .as_str()
            .unwrap()
            .ends_with("/ocs/v2.php/apps/files/api/v1/directEditing")
    );
    assert_eq!(de["etag"].as_str().unwrap().len(), 32);
    let (st, list) = ocs(&e, "GET", "", "alice", "").await;
    assert_eq!(st, StatusCode::OK);
    let text = &list["ocs"]["data"]["editors"]["text"];
    assert_eq!(text["id"], "text");
    let mimes: Vec<&str> = text["mimetypes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert!(
        mimes.contains(&"text/plain") && mimes.contains(&"text/markdown"),
        "{mimes:?}"
    );

    // open: answers with a URL under /apps/files/directEditing/<token>
    let (st, v) = ocs(
        &e,
        "POST",
        "/open",
        "alice",
        "path=%2FSHOPPING.txt&editorId=text",
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let url = open_url(&v);
    assert!(url.contains("/apps/files/directEditing/"), "{url}");
    // the URL is opened by the web view without any login: redirect to the editor, plus the direct cookie
    let (st, h, _) = send(&e, "GET", &path_of(&url), &[], "").await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert_eq!(
        h.get("location").unwrap(),
        &format!("/apps/files/edit?id={id}&app=1")
    );
    let cookie = h.get("set-cookie").unwrap().to_str().unwrap().to_string();
    assert!(
        cookie.starts_with("rb_direct=") && cookie.contains("HttpOnly"),
        "{cookie}"
    );
    let sid = cookie
        .strip_prefix("rb_direct=")
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let direct = ("cookie", format!("rb_direct={sid}"));
    // one time only
    assert_eq!(
        send(&e, "GET", &path_of(&url), &[], "").await.0,
        StatusCode::NOT_FOUND
    );

    // with that cookie: the editor page for that file (app mode: no top bar) ...
    let (st, _, page) = send(
        &e,
        "GET",
        &format!("/apps/files/edit?id={id}&app=1"),
        std::slice::from_ref(&direct),
        "",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        page.contains("data-app=\"1\"")
            && page.contains("data-edit=\"1\"")
            && !page.contains("class=\"top\""),
        "{page}"
    );
    // ... and the live socket for that file ...
    let csrf = csrf_token_for(&sid);
    let mut c = try_connect_cookie(&e, format!("rb_direct={sid}"), id, None, csrf.clone())
        .await
        .unwrap();
    c.pump(300).await;
    assert_eq!(c.text(), "milch");
    c.type_at(5, " und brot").await;
    c.send(&Message::Custom(101, vec![])).await;
    c.pump(500).await;
    assert_eq!(read(&e, "alice", "/SHOPPING.txt").await, "milch und brot");
    // ... but nothing else: not another file, not the file list, not DAV, not the API
    let (st, _, _) = send(
        &e,
        "GET",
        &format!("/apps/files/edit?id={other}&app=1"),
        std::slice::from_ref(&direct),
        "",
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(
        try_connect_cookie(&e, format!("rb_direct={sid}"), other, None, csrf)
            .await
            .err(),
        Some(StatusCode::FORBIDDEN)
    );
    assert_eq!(
        send(&e, "GET", "/apps/files/", std::slice::from_ref(&direct), "")
            .await
            .0,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        send(
            &e,
            "GET",
            "/remote.php/dav/files/alice/other.txt",
            std::slice::from_ref(&direct),
            ""
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(
            &e,
            "GET",
            "/ocs/v2.php/cloud/user?format=json",
            &[direct.clone(), ("ocs-apirequest", "true".into())],
            ""
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn direct_editing_refuses_what_it_should_and_tokens_expire() {
    let e = env().await;
    assert!(dav(&e, "alice", "PUT", "/a.txt", "x").await.is_success());
    assert!(dav(&e, "alice", "PUT", "/pic.png", "x").await.is_success());
    assert!(dav(&e, "alice", "MKCOL", "/dir", "").await.is_success());
    share(&e, "alice", "/a.txt", "carol", "1").await; // carol may only read
    for (user, body, why) in [
        ("alice", "path=%2Fa.txt&editorId=nope", "unknown editor"),
        ("alice", "path=%2Fmissing.txt&editorId=text", "missing file"),
        (
            "alice",
            "path=%2Fpic.png&editorId=text",
            "not an editable type",
        ),
        ("alice", "path=%2Fdir&editorId=text", "a folder"),
        ("alice", "editorId=text", "no file"),
        (
            "dave",
            "path=%2Fa.txt&editorId=text",
            "no access for a stranger",
        ),
    ] {
        let (st, v) = ocs(&e, "POST", "/open", user, body).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{why}: {v}");
        assert!(
            v["ocs"]["data"]["message"]
                .as_str()
                .unwrap()
                .starts_with("Failed to open file:"),
            "{why}"
        );
    }
    // by file id works too; a read-only share opens in view mode
    let id = fileid(&e, "alice", "/a.txt").await;
    let (st, v) = ocs(
        &e,
        "POST",
        "/open",
        "alice",
        &format!("fileId={id}&editorId=text"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = ocs(&e, "POST", "/open", "carol", "path=%2Fa.txt&editorId=text").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, h, _) = send(&e, "GET", &path_of(&open_url(&v)), &[], "").await;
    let sid = h
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .strip_prefix("rb_direct=")
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let (st, _, page) = send(
        &e,
        "GET",
        &format!("/apps/files/edit?id={id}&app=1"),
        &[("cookie", format!("rb_direct={sid}"))],
        "",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        page.contains("data-edit=\"0\""),
        "read-only share is view mode"
    );

    // an expired token is dead; so is an expired session
    let (_, v) = ocs(&e, "POST", "/open", "alice", "path=%2Fa.txt&editorId=text").await;
    let url = open_url(&v);
    sqlx::query("UPDATE direct_tokens SET expires_at = 1")
        .execute(&e.pool)
        .await
        .unwrap();
    assert_eq!(
        send(&e, "GET", &path_of(&url), &[], "").await.0,
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE direct_sessions SET expires_at = 1")
        .execute(&e.pool)
        .await
        .unwrap();
    let (st, _, _) = send(
        &e,
        "GET",
        &format!("/apps/files/edit?id={id}&app=1"),
        &[("cookie", format!("rb_direct={sid}"))],
        "",
    )
    .await;
    assert_eq!(
        st,
        StatusCode::SEE_OTHER,
        "an expired direct session is sent to the login"
    );
    // garbage token
    assert_eq!(
        send(&e, "GET", "/apps/files/directEditing/nonsense", &[], "")
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // switched off: no capability, open refused
    let off = env_with(|c| c.editor = false, 150).await;
    let (_, _, caps) = send(
        &off,
        "GET",
        "/ocs/v2.php/cloud/capabilities?format=json",
        &[basic("alice"), ("ocs-apirequest", "true".into())],
        "",
    )
    .await;
    assert!(!caps.contains("directEditing"), "{caps}");
    assert!(dav(&off, "alice", "PUT", "/a.txt", "x").await.is_success());
    let (st, _) = ocs(
        &off,
        "POST",
        "/open",
        "alice",
        "path=%2Fa.txt&editorId=text",
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}
