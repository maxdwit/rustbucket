//! Live editor for `.md` / `.txt` files: one room per file, a WebSocket that speaks the y-protocols sync and
//! awareness messages (Yjs on the browser side, `yrs` here), autosave through the normal `Store::put` so a save
//! behaves exactly like a WebDAV PUT (version, etag, sync visibility).
//!
//! Who may do what is decided here and not in the browser: everybody who can read the file may join and
//! receives every change; only users with the update bit have their changes applied (the others' updates are
//! dropped silently). Permissions are re-checked while a connection lives.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{CloseFrame, Message as WsMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use yrs::sync::protocol::MessageReader;
use yrs::sync::{Awareness, DefaultProtocol, Error as SyncError, Message, Protocol, SyncMessage};
use yrs::updates::decoder::DecoderV1;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, Text, TextRef, Transact};

use crate::auth::Peer;
use crate::login::cookie_value;
use crate::routes::AppState;
use crate::shares::{READ, UPDATE};
use crate::storage::{FsError, Node, Owner, PutOpts};
use crate::web::{self, Web};

/// Name of the shared text inside the Yjs document (the browser uses the same name).
const TEXT: &str = "t";
/// Custom message tag for server notices (JSON payload): saved, external change, permission change.
const NOTICE: u8 = 100;
/// Custom message tag from the browser: "write the file now" (Save & close).
const FLUSH: u8 = 101;

const MAX_FRAME: usize = 1 << 20;
const MAX_CLIENTS_PER_ROOM: usize = 20;
const MAX_ROOMS: usize = 100;
/// Awareness ids one connection may announce (a browser tab uses one) and the size of one state.
const MAX_AWARENESS_IDS: usize = 4;
const MAX_AWARENESS_JSON: usize = 2048;
/// Autosave: this long after the last edit, but never later than `SAVE_MAX` after the first unsaved edit.
const SAVE_IDLE: Duration = Duration::from_secs(5);
const SAVE_MAX: Duration = Duration::from_secs(60);
const EXTERNAL_POLL: Duration = Duration::from_secs(5);
const RECHECK: Duration = Duration::from_secs(30);
const PING: Duration = Duration::from_secs(25);
const DEAD_AFTER: Duration = Duration::from_secs(90);

/// All open rooms (in `AppState`) and the autosave timing.
pub struct Editors {
    rooms: Mutex<HashMap<i64, Arc<Room>>>,
    save_idle: Duration,
    poll: Duration,
}

impl Default for Editors {
    fn default() -> Self {
        Self::with_timing(SAVE_IDLE, EXTERNAL_POLL)
    }
}

impl Editors {
    /// Different timing, for tests: save this long after the last edit, look for outside changes this often.
    pub fn with_timing(save_idle: Duration, poll: Duration) -> Self {
        Self {
            rooms: Mutex::default(),
            save_idle,
            poll,
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct RoomState {
    clients: usize,
    /// etag of the file as we last read or wrote it
    etag: String,
    /// bumped by every edit; a save only clears `dirty_since` when nothing changed while it ran
    edit_seq: u64,
    dirty_since: Option<Instant>,
    last_edit: Instant,
    /// the text grew past the limit: nothing is saved any more until the room is closed
    overflow: bool,
    /// the file was deleted
    gone: bool,
    last_poll: Instant,
}

pub struct Room {
    fileid: i64,
    owner: Owner,
    awareness: Mutex<Awareness>,
    tx: broadcast::Sender<Arc<Vec<u8>>>,
    state: Arc<Mutex<RoomState>>,
    max_bytes: u64,
}

/// What the page needs to know about a file the viewer opens.
pub struct Opened {
    pub node: Node,
    pub can_edit: bool,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/apps/files/edit", get(page))
        .route("/apps/files/edit/ws", get(socket))
}

/// File types the editor opens, by extension (any case): text and Markdown, shell and Python scripts, JSON, YAML and .conf files.
/// The extension only decides whether to offer the editor; the content is checked as well when a room opens
/// (valid UTF-8, no NUL bytes), so a binary file with a script name is refused instead of mangled.
pub const EDITABLE: [&str; 10] = [
    ".md",
    ".markdown",
    ".txt",
    ".sh",
    ".bash",
    ".py",
    ".json",
    ".yml",
    ".yaml",
    ".conf",
];

pub fn editable_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    EDITABLE.iter().any(|e| n.ends_with(e)) && !n.starts_with('.')
}

fn notice(kind: &str, extra: &str) -> Arc<Vec<u8>> {
    let json = format!("{{\"t\":\"{kind}\"{extra}}}");
    Arc::new(Message::Custom(NOTICE, json.into_bytes()).encode_v1())
}

// ------------------------------------------------------------------ rooms

impl Room {
    fn text(&self) -> String {
        let aw = lock(&self.awareness);
        let txn = aw.doc().transact();
        match txn.get_text(TEXT) {
            Some(t) => t.get_string(&txn),
            None => String::new(),
        }
    }

    fn text_len(aw: &Awareness) -> u64 {
        let txn = aw.doc().transact();
        txn.get_text(TEXT).map_or(0, |t| u64::from(t.len(&txn)))
    }
}

/// The room of a file, loading the file into a new one if nobody has it open.
async fn join(st: &AppState, node: &Node, owner: &Owner) -> Result<Arc<Room>, &'static str> {
    {
        let map = lock(&st.editors.rooms);
        if let Some(r) = map.get(&node.fileid) {
            let mut s = lock(&r.state);
            if s.clients >= MAX_CLIENTS_PER_ROOM {
                return Err("too many people are editing this file");
            }
            s.clients += 1;
            return Ok(r.clone());
        }
        if map.len() >= MAX_ROOMS {
            return Err("too many files are open for editing");
        }
    }
    let max = st.config.editor_max_bytes;
    let path = st.store.fs_path(owner, &node.path);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| "the file cannot be read")?;
    if bytes.len() as u64 > max {
        return Err("the file is too large for the editor");
    }
    if bytes.contains(&0) {
        return Err("the file looks binary");
    }
    let text = String::from_utf8(bytes).map_err(|_| "the file is not UTF-8 text")?;

    let doc = Doc::new();
    {
        let t = doc.get_or_insert_text(TEXT);
        let mut txn = doc.transact_mut();
        t.insert(&mut txn, 0, &text);
    }
    let (tx, _) = broadcast::channel::<Arc<Vec<u8>>>(256);
    let state = Arc::new(Mutex::new(RoomState {
        clients: 0,
        etag: node.etag.clone(),
        edit_seq: 0,
        dirty_since: None,
        last_edit: Instant::now(),
        overflow: false,
        gone: false,
        last_poll: Instant::now(),
    }));
    {
        let tx = tx.clone();
        let state = state.clone();
        doc.observe_update_v1("rb-room", move |_txn, e: &yrs::UpdateEvent| {
            let _ = tx.send(Arc::new(
                Message::Sync(SyncMessage::Update(e.update.clone())).encode_v1(),
            ));
            let mut s = lock(&state);
            s.edit_seq += 1;
            s.last_edit = Instant::now();
            s.dirty_since.get_or_insert_with(Instant::now);
        })
        .map_err(|_| "internal error")?;
    }
    let room = Arc::new(Room {
        fileid: node.fileid,
        owner: owner.clone(),
        awareness: Mutex::new(Awareness::new(doc)),
        tx,
        state,
        max_bytes: max,
    });
    let mut map = lock(&st.editors.rooms);
    if let Some(r) = map.get(&node.fileid) {
        // somebody else created it while we were reading the file
        let mut s = lock(&r.state);
        if s.clients >= MAX_CLIENTS_PER_ROOM {
            return Err("too many people are editing this file");
        }
        s.clients += 1;
        return Ok(r.clone());
    }
    lock(&room.state).clients = 1;
    map.insert(node.fileid, room.clone());
    drop(map);
    tokio::spawn(housekeeping(st.clone(), Arc::downgrade(&room)));
    Ok(room)
}

/// A client left: the last one out saves what is left and closes the room.
async fn leave(st: &AppState, room: &Arc<Room>) {
    let last = {
        let mut s = lock(&room.state);
        s.clients = s.clients.saturating_sub(1);
        s.clients == 0
    };
    if !last {
        return;
    }
    if let Err(e) = save(st, room, true).await {
        tracing::warn!(error = %e, fileid = room.fileid, "editor: final save failed");
    }
    let mut map = lock(&st.editors.rooms);
    if lock(&room.state).clients == 0 {
        map.remove(&room.fileid);
    }
}

/// Autosave and the check for changes made behind our back; ends when the room is gone.
async fn housekeeping(st: AppState, room: Weak<Room>) {
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tick.tick().await;
        let Some(room) = room.upgrade() else { return };
        let (due, poll) = {
            let mut s = lock(&room.state);
            if s.gone {
                return;
            }
            let now = Instant::now();
            let due = s.dirty_since.is_some_and(|since| {
                now.duration_since(s.last_edit) >= st.editors.save_idle
                    || now.duration_since(since) >= SAVE_MAX
            });
            let poll =
                s.dirty_since.is_none() && now.duration_since(s.last_poll) >= st.editors.poll;
            if poll {
                s.last_poll = now;
            }
            (due, poll)
        };
        if due {
            if let Err(e) = save(&st, &room, false).await {
                tracing::warn!(error = %e, fileid = room.fileid, "editor: autosave failed");
            }
        } else if poll {
            adopt_external(&st, &room).await;
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Write the room's text to the file like a PUT. With `closing` the room is going away, so a failure is only logged.
async fn save(st: &AppState, room: &Arc<Room>, closing: bool) -> Result<(), FsError> {
    let seq = {
        let s = lock(&room.state);
        if s.dirty_since.is_none() || s.overflow || s.gone {
            return Ok(());
        }
        s.edit_seq
    };
    let text = room.text();
    if text.len() as u64 > room.max_bytes {
        lock(&room.state).overflow = true;
        let _ = room.tx.send(notice("overflow", ""));
        return Ok(());
    }
    if !st.config.allows_size(text.len() as u64) {
        return Ok(());
    }
    let Some(node) = st.store.get_by_id(&room.owner, room.fileid).await? else {
        lock(&room.state).gone = true;
        let _ = room.tx.send(notice("gone", ""));
        return Ok(());
    };
    // Whatever somebody else wrote since we looked is not lost: `put` keeps it as a version.
    let (tmp, mut f) = st.store.new_tmp(&room.owner).await?;
    f.write_all(text.as_bytes()).await?;
    f.sync_all().await?;
    drop(f);
    // the new file replaces the old one: keep its mode, so an executable script stays executable
    if let Ok(m) = tokio::fs::metadata(st.store.fs_path(&room.owner, &node.path)).await {
        let _ = tokio::fs::set_permissions(&tmp, m.permissions()).await;
    }
    let opts = PutOpts {
        mtime: unix_now(),
        mimetype: crate::storage::mime_for(&node.name),
        checksum: None,
        if_match: Some(node.etag.clone()),
        if_none_match_star: false,
    };
    let (new, _) = st
        .store
        .put(&room.owner, &node.path, &tmp, text.len() as i64, opts)
        .await?;
    {
        let mut s = lock(&room.state);
        s.etag = new.etag;
        if s.edit_seq == seq {
            s.dirty_since = None;
        } else {
            s.dirty_since = Some(Instant::now());
        }
    }
    if !closing {
        let _ = room
            .tx
            .send(notice("saved", &format!(",\"at\":{}", unix_now())));
    }
    Ok(())
}

/// A clean room whose file changed on disk (sync client, WebDAV): take the new content over as a text diff.
async fn adopt_external(st: &AppState, room: &Arc<Room>) {
    let Ok(Some(node)) = st.store.get_by_id(&room.owner, room.fileid).await else {
        if matches!(st.store.get_by_id(&room.owner, room.fileid).await, Ok(None)) {
            lock(&room.state).gone = true;
            let _ = room.tx.send(notice("gone", ""));
        }
        return;
    };
    if lock(&room.state).etag == node.etag || node.size as u64 > room.max_bytes {
        return;
    }
    let Ok(bytes) = tokio::fs::read(st.store.fs_path(&room.owner, &node.path)).await else {
        return;
    };
    let Ok(new_text) = String::from_utf8(bytes) else {
        return;
    };
    {
        let aw = lock(&room.awareness);
        let old = {
            let txn = aw.doc().transact();
            txn.get_text(TEXT)
                .map(|t| t.get_string(&txn))
                .unwrap_or_default()
        };
        if old != new_text {
            replace_diff(aw.doc(), &old, &new_text);
        }
    }
    let mut s = lock(&room.state);
    s.etag = node.etag;
    s.dirty_since = None; // adopting is not an edit of ours
    drop(s);
    let _ = room.tx.send(notice("external", ""));
}

/// Turn `old` into `new` with one removal and one insertion in the middle (common prefix and suffix stay).
fn replace_diff(doc: &Doc, old: &str, new: &str) {
    let prefix = old
        .char_indices()
        .zip(new.chars())
        .take_while(|((_, a), b)| a == b)
        .last()
        .map_or(0, |((i, a), _)| i + a.len_utf8());
    let (o, n) = (&old[prefix..], &new[prefix..]);
    let suffix = o
        .chars()
        .rev()
        .zip(n.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum::<usize>();
    let (cut, put) = (&o[..o.len() - suffix], &n[..n.len() - suffix]);
    let t: TextRef = doc.get_or_insert_text(TEXT);
    let mut txn = doc.transact_mut();
    if !cut.is_empty() {
        t.remove_range(&mut txn, prefix as u32, cut.len() as u32);
    }
    if !put.is_empty() {
        t.insert(&mut txn, prefix as u32, put);
    }
}

// ------------------------------------------------------------------ protocol with permissions

/// The default sync protocol, except that a client without write permission cannot change the text.
struct Guarded {
    can_edit: bool,
}

impl Protocol for Guarded {
    fn handle_sync_step2(
        &self,
        awareness: &mut Awareness,
        update: yrs::Update,
    ) -> Result<Option<Message>, SyncError> {
        if self.can_edit {
            DefaultProtocol.handle_sync_step2(awareness, update)
        } else {
            Ok(None)
        }
    }
    fn handle_update(
        &self,
        awareness: &mut Awareness,
        update: yrs::Update,
    ) -> Result<Option<Message>, SyncError> {
        self.handle_sync_step2(awareness, update)
    }
}

// ------------------------------------------------------------------ routes

fn off(st: &AppState) -> bool {
    !st.config.editor
}

/// Origin of a WebSocket handshake must be this site (browsers always send it).
fn same_origin(st: &AppState, peer: std::net::IpAddr, headers: &HeaderMap) -> bool {
    let base = crate::proxy::external_base_url(&st.config, peer, headers);
    headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|o| o.trim_end_matches('/') == base.trim_end_matches('/'))
}

/// May this user open the file with this id, and may they change it?
pub(crate) async fn access(st: &AppState, user: &Owner, fileid: i64) -> Option<(Opened, Owner)> {
    let (node, owner, perms) = st.store.visible_perms_by_id(user, fileid).await.ok()??;
    if node.is_dir || perms & READ == 0 || !editable_name(&node.name) {
        return None;
    }
    Some((
        Opened {
            can_edit: perms & UPDATE != 0,
            node,
        },
        owner,
    ))
}

/// Who is asking: a normal browser session, or a direct session of a mobile app that may open one file only.
pub struct EditorWeb {
    web: Web,
    only: Option<i64>,
}

impl FromRequestParts<AppState> for EditorWeb {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        match Web::from_request_parts(parts, state).await {
            Ok(web) => Ok(EditorWeb { web, only: None }),
            Err(login_redirect) => {
                if let Some(sid) = cookie_value(&parts.headers, crate::direct::DIRECT_COOKIE)
                    && let Some((user, file)) = crate::direct::session(&state.pool, &sid).await
                {
                    return Ok(EditorWeb {
                        web: Web::from_direct(user, sid),
                        only: Some(file),
                    });
                }
                Err(login_redirect)
            }
        }
    }
}

async fn page(
    State(st): State<AppState>,
    EditorWeb { web, only }: EditorWeb,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if off(&st) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let app = only.is_some() || q.get("app").is_some_and(|v| v == "1");
    let id = q.get("id").and_then(|v| v.parse::<i64>().ok());
    web::editor_page(
        &st,
        &web,
        q.get("path").map_or("", String::as_str),
        id,
        only,
        app,
    )
    .await
}

async fn socket(
    State(st): State<AppState>,
    EditorWeb { web: w, only }: EditorWeb,
    Peer(peer): Peer,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    if off(&st) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !same_origin(&st, peer, &headers)
        || !q
            .get("csrf")
            .is_some_and(|t| crate::login::ct_eq(t, &w.csrf))
    {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    let Some(id) = q.get("id").and_then(|v| v.parse::<i64>().ok()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if only.is_some_and(|f| f != id) {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    let me = Owner {
        id: w.user.id,
        uid: w.user.uid.clone(),
    };
    let Some((opened, owner)) = access(&st, &me, id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ws.max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |sock| async move {
            let room = match join(&st, &opened.node, &owner).await {
                Ok(r) => r,
                Err(msg) => return refuse(sock, msg).await,
            };
            run_client(sock, &st, &room, &me, opened.can_edit).await;
            leave(&st, &room).await;
        })
}

async fn refuse(mut sock: WebSocket, why: &'static str) {
    let _ = sock
        .send(WsMsg::Binary(
            notice("refused", &format!(",\"why\":\"{why}\""))
                .to_vec()
                .into(),
        ))
        .await;
    finish(sock, 1008, why).await;
}

/// Send the close frame and wait briefly for the browser's answer: dropping the socket with unread data
/// in it would reset the connection and the browser would never see why it was closed.
async fn finish(mut sock: WebSocket, code: u16, reason: &'static str) {
    let _ = sock
        .send(WsMsg::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while sock.recv().await.is_some() {}
    })
    .await;
}

fn frame(m: &Message) -> WsMsg {
    WsMsg::Binary(m.encode_v1().into())
}

async fn run_client(
    mut sock: WebSocket,
    st: &AppState,
    room: &Arc<Room>,
    me: &Owner,
    mut can_edit: bool,
) {
    let mut rx = room.tx.subscribe();
    let mut my_ids: HashSet<yrs::block::ClientID> = HashSet::new();
    // greeting: our state vector (the client answers with what we lack) and who is here
    let hello = {
        let aw = lock(&room.awareness);
        let sv = aw.doc().transact().state_vector();
        let people = aw.update().ok();
        (sv, people)
    };
    let mut first = vec![frame(&Message::Sync(SyncMessage::SyncStep1(hello.0)))];
    if let Some(p) = hello.1 {
        first.push(frame(&Message::Awareness(p)));
    }
    first.push(WsMsg::Binary(
        notice("hello", &format!(",\"edit\":{can_edit}"))
            .to_vec()
            .into(),
    ));
    for m in first {
        if sock.send(m).await.is_err() {
            return;
        }
    }
    let mut ping = tokio::time::interval(PING);
    let mut recheck = tokio::time::interval(RECHECK);
    let mut last_rx = Instant::now();
    let close_code: u16 = loop {
        tokio::select! {
            incoming = sock.recv() => {
                let Some(Ok(msg)) = incoming else { break 1001 };
                last_rx = Instant::now();
                let data = match msg {
                    WsMsg::Binary(b) => b,
                    WsMsg::Close(_) => break 1000,
                    WsMsg::Text(_) => break 1003,
                    _ => continue,
                };
                match handle_frame(room, can_edit, &data, &mut my_ids) {
                    Ok((replies, flush)) => {
                        for r in replies {
                            if sock.send(frame(&r)).await.is_err() { break; }
                        }
                        if flush {
                            // Save & close: write now (only people who may change the text have anything to save)
                            let ok = !can_edit
                                || (save(st, room, false).await.is_ok() && !lock(&room.state).overflow);
                            let n = notice("flushed", &format!(",\"ok\":{ok}"));
                            if sock.send(WsMsg::Binary(n.to_vec().into())).await.is_err() { break 1001 }
                        }
                    }
                    Err(code) => break code,
                }
            }
            out = rx.recv() => match out {
                Ok(bytes) => {
                    if sock.send(WsMsg::Binary(bytes.to_vec().into())).await.is_err() { break 1001 }
                }
                // we fell behind: reconnecting is cheaper than guessing what was missed
                Err(_) => break 1013,
            },
            _ = ping.tick() => {
                if last_rx.elapsed() > DEAD_AFTER { break 1001 }
                if sock.send(WsMsg::Ping(Vec::new().into())).await.is_err() { break 1001 }
            }
            _ = recheck.tick() => {
                match access(st, me, room.fileid).await {
                    None => break 1008,
                    Some((o, _)) if o.can_edit != can_edit => {
                        can_edit = o.can_edit;
                        let n = notice("perm", &format!(",\"edit\":{can_edit}"));
                        if sock.send(WsMsg::Binary(n.to_vec().into())).await.is_err() { break 1001 }
                    }
                    Some(_) => {}
                }
            }
        }
    };
    // tell the others this person has gone
    if !my_ids.is_empty() {
        let removal = {
            let mut aw = lock(&room.awareness);
            for id in &my_ids {
                aw.remove_state(*id);
            }
            aw.update_with_clients(my_ids.iter().copied()).ok()
        };
        if let Some(u) = removal {
            let _ = room.tx.send(Arc::new(Message::Awareness(u).encode_v1()));
        }
    }
    finish(sock, close_code, "").await;
}

/// Apply one client frame; `Ok` = messages to send back to this client, `Err` = close code.
fn handle_frame(
    room: &Arc<Room>,
    can_edit: bool,
    data: &[u8],
    my_ids: &mut HashSet<yrs::block::ClientID>,
) -> Result<(Vec<Message>, bool), u16> {
    let mut replies = Vec::new();
    let mut flush = false;
    let mut decoder = DecoderV1::new(yrs::encoding::read::Cursor::new(data));
    let reader = MessageReader::new(&mut decoder);
    let guard = Guarded { can_edit };
    for m in reader {
        let m = m.map_err(|_| 1007u16)?;
        match m {
            Message::Awareness(u) => {
                if u.clients
                    .values()
                    .any(|e| e.json.len() > MAX_AWARENESS_JSON)
                {
                    return Err(1009);
                }
                for id in u.clients.keys() {
                    my_ids.insert(*id);
                }
                if my_ids.len() > MAX_AWARENESS_IDS {
                    return Err(1008);
                }
                let mut aw = lock(&room.awareness);
                aw.apply_update(u.clone()).map_err(|_| 1007u16)?;
                drop(aw);
                let _ = room.tx.send(Arc::new(Message::Awareness(u).encode_v1()));
            }
            Message::Custom(FLUSH, _) => flush = true,
            Message::Custom(..) | Message::Auth(_) => {}
            other => {
                let mut aw = lock(&room.awareness);
                if let Some(r) = guard.handle_message(&mut aw, other).map_err(|_| 1007u16)? {
                    replies.push(r);
                }
                if can_edit && Room::text_len(&aw) > room.max_bytes {
                    drop(aw);
                    lock(&room.state).overflow = true;
                    let _ = room.tx.send(notice("overflow", ""));
                    return Err(1009);
                }
            }
        }
    }
    Ok((replies, flush))
}
