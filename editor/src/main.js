// Browser side of the live editor: CodeMirror 6 on a shared Yjs text, synced over one WebSocket to the server
// (src/editor.rs). The server decides who may change the text; "readOnly" here is only for the user's benefit.
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView, drawSelection, highlightActiveLine, keymap, lineNumbers } from "@codemirror/view";
import { defaultKeymap } from "@codemirror/commands";
import * as Y from "yjs";
import { yCollab } from "y-codemirror.next";
import * as awarenessProtocol from "y-protocols/awareness";
import * as syncProtocol from "y-protocols/sync";
import * as encoding from "lib0/encoding";
import * as decoding from "lib0/decoding";

const MSG_SYNC = 0, MSG_AWARENESS = 1, MSG_NOTICE = 100, MSG_FLUSH = 101;
// origin of everything that came from the server: tracked by the undo manager, so Undo rolls back other people's changes too
const REMOTE = "remote";
const HOLD_MS = 1000;
const COLORS = ["#d32f2f", "#1976d2", "#388e3c", "#f57c00", "#7b1fa2", "#00838f", "#c2185b", "#5d4037"];

const host = document.getElementById("editor");
if (host) start(host);

function start(host) {
  const id = host.dataset.id;
  const name = host.dataset.user || "anonymous";
  let canEdit = host.dataset.edit === "1";
  const csrf = document.querySelector('meta[name="csrf-token"]').content;
  const nonce = document.querySelector('meta[name="csp-nonce"]')?.content;
  const status = document.getElementById("ed-status");
  const people = document.getElementById("ed-people");

  const ydoc = new Y.Doc();
  const ytext = ydoc.getText("t");
  const awareness = new awarenessProtocol.Awareness(ydoc);
  const color = COLORS[Math.floor(Math.random() * COLORS.length)];
  awareness.setLocalStateField("user", { name, color, colorLight: color + "33" });

  // one shared history: changes of every person since this page was opened (the text that was there at the start is the floor)
  const undo = new Y.UndoManager(ytext, { trackedOrigins: new Set([REMOTE]), captureTimeout: 500 });
  const editable = new Compartment();
  const view = new EditorView({
    parent: host,
    state: EditorState.create({
      doc: ytext.toString(),
      extensions: [
        nonce ? EditorView.cspNonce.of(nonce) : [],
        lineNumbers(),
        drawSelection(),
        highlightActiveLine(),
        EditorView.lineWrapping,
        keymap.of(defaultKeymap), // no Ctrl+Z: a rollback of everybody's work needs the held buttons
        editable.of(EditorState.readOnly.of(!canEdit)),
        yCollab(ytext, awareness, { undoManager: undo }),
      ],
    }),
  });
  let setEditable = (v) => {
    canEdit = v;
    view.dispatch({ effects: editable.reconfigure(EditorState.readOnly.of(!v)) });
  };

  let ws = null, retry = 0, timer = null, initialSynced = false, flushWaiter = null;
  const say = (t) => { status.textContent = t; };
  const clock = (s) => new Date(s * 1000).toLocaleTimeString();

  const send = (enc) => { if (ws && ws.readyState === 1) ws.send(encoding.toUint8Array(enc)); };

  ydoc.on("update", (update, origin) => {
    if (origin === REMOTE) return;
    const enc = encoding.createEncoder();
    encoding.writeVarUint(enc, MSG_SYNC);
    syncProtocol.writeUpdate(enc, update);
    send(enc);
  });
  awareness.on("update", ({ added, updated, removed }, origin) => {
    if (origin === REMOTE) return;
    const changed = added.concat(updated, removed);
    const enc = encoding.createEncoder();
    encoding.writeVarUint(enc, MSG_AWARENESS);
    encoding.writeVarUint8Array(enc, awarenessProtocol.encodeAwarenessUpdate(awareness, changed));
    send(enc);
  });
  awareness.on("change", () => {
    people.replaceChildren();
    awareness.getStates().forEach((st, cid) => {
      if (!st.user) return;
      const chip = document.createElement("span");
      chip.className = "chip";
      chip.style.backgroundColor = st.user.color; // CSSOM, allowed by the page's CSP
      chip.textContent = cid === ydoc.clientID ? st.user.name + " (you)" : st.user.name;
      people.append(chip);
    });
  });

  function onNotice(n) {
    if (n.t === "hello") { try { bridge()?.loaded?.(); } catch (_) { /* no app around */ } setEditable(!!n.edit); say(n.edit ? "connected" : "connected, read only"); }
    else if (n.t === "perm") { setEditable(!!n.edit); say(n.edit ? "you can edit now" : "your edit permission was withdrawn"); }
    else if (n.t === "saved") say("saved " + clock(n.at));
    else if (n.t === "flushed") { if (flushWaiter) flushWaiter(!!n.ok); }
    else if (n.t === "external") say("the file was changed elsewhere; updated");
    else if (n.t === "gone") say("the file was deleted");
    else if (n.t === "overflow") say("the text is too large; changes are no longer saved");
    else if (n.t === "refused") say("cannot open: " + n.why);
  }

  function connect() {
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    ws = new WebSocket(`${proto}//${location.host}/apps/files/edit/ws?id=${encodeURIComponent(id)}&csrf=${encodeURIComponent(csrf)}`);
    ws.binaryType = "arraybuffer";
    const mine = ws;
    ws.onopen = () => {
      retry = 0;
      const enc = encoding.createEncoder();
      encoding.writeVarUint(enc, MSG_SYNC);
      syncProtocol.writeSyncStep1(enc, ydoc);
      send(enc);
      awareness.setLocalState(awareness.getLocalState()); // announce ourselves again
    };
    ws.onmessage = (ev) => {
      const dec = decoding.createDecoder(new Uint8Array(ev.data));
      const type = decoding.readVarUint(dec);
      if (type === MSG_SYNC) {
        const enc = encoding.createEncoder();
        encoding.writeVarUint(enc, MSG_SYNC);
        const kind = syncProtocol.readSyncMessage(dec, enc, ydoc, REMOTE);
        if (encoding.length(enc) > 1) send(enc);
        if (kind === syncProtocol.messageYjsSyncStep2 && !initialSynced) { initialSynced = true; undo.clear(); }
      } else if (type === MSG_AWARENESS) {
        awarenessProtocol.applyAwarenessUpdate(awareness, decoding.readVarUint8Array(dec), REMOTE);
      } else if (type === MSG_NOTICE) {
        try { onNotice(JSON.parse(new TextDecoder().decode(decoding.readVarUint8Array(dec)))); } catch (_) { /* ignore */ }
      }
    };
    ws.onclose = (ev) => {
      // forget the other people until we are back
      awarenessProtocol.removeAwarenessStates(
        awareness,
        Array.from(awareness.getStates().keys()).filter((c) => c !== ydoc.clientID),
        mine,
      );
      if (ev.code === 1008) { say(ev.reason ? "closed: " + ev.reason : "no access"); return; }
      say("offline, reconnecting...");
      retry = Math.min(retry + 1, 6);
      timer = setTimeout(connect, 500 * 2 ** retry);
    };
  }
  connect();
  window.addEventListener("beforeunload", () => {
    awarenessProtocol.removeAwarenessStates(awareness, [ydoc.clientID], "unload");
    clearTimeout(timer);
  });

  // ---- Undo / Redo: hold the button for one second (a rollback affects everybody's text)
  const btnUndo = document.getElementById("ed-undo");
  const btnRedo = document.getElementById("ed-redo");
  const refresh = () => {
    btnUndo.disabled = !canEdit || undo.undoStack.length === 0;
    btnRedo.disabled = !canEdit || undo.redoStack.length === 0;
  };
  ["stack-item-added", "stack-item-popped", "stack-cleared"].forEach((e) => undo.on(e, refresh));
  const baseSetEditable = setEditable;
  setEditable = (v) => { baseSetEditable(v); refresh(); }; // eslint-disable-line no-func-assign
  refresh();

  function holdButton(btn, action, label) {
    let t = null, done = false;
    const stop = () => { clearTimeout(t); t = null; btn.classList.remove("holding"); if (!done) say(""); };
    const start = () => {
      if (btn.disabled || t !== null || done) return;
      btn.classList.add("holding");
      say(label + ": keep holding for one second");
      t = setTimeout(() => {
        t = null; done = true; btn.classList.remove("holding");
        action(); refresh(); say(label + " done");
        view.focus();
      }, HOLD_MS);
    };
    const release = () => { done = false; stop(); };
    btn.addEventListener("pointerdown", (e) => { if (e.button === 0) start(); });
    ["pointerup", "pointercancel", "pointerleave"].forEach((e) => btn.addEventListener(e, release));
    btn.addEventListener("keydown", (e) => { if ((e.key === " " || e.key === "Enter") && !e.repeat) { e.preventDefault(); start(); } });
    btn.addEventListener("keyup", (e) => { if (e.key === " " || e.key === "Enter") release(); });
    btn.addEventListener("blur", release);
    btn.addEventListener("click", (e) => e.preventDefault());
  }
  holdButton(btnUndo, () => undo.undo(), "Undo");
  holdButton(btnRedo, () => undo.redo(), "Redo");

  // ---- Save & close: ask the server to write the file now, leave when it says so
  const close = document.getElementById("ed-close");
  // inside the Nextcloud mobile app's web view the app closes the view itself (it offers this bridge object)
  const bridge = () => window.DirectEditingMobileInterface;
  const leave = () => {
    const b = bridge();
    if (b && typeof b.close === "function") { b.close(); return true; }
    return false;
  };
  close.addEventListener("click", (e) => {
    if (!canEdit || !ws || ws.readyState !== 1) { // nothing of ours to save: leave
      if (leave()) e.preventDefault();
      return;
    }
    e.preventDefault();
    say("saving...");
    let finished = false;
    const go = (ok) => {
      if (finished) return;
      finished = true; flushWaiter = null;
      if (!ok) say("could not save; the page stays open");
      else if (!leave()) location.href = close.href;
    };
    flushWaiter = go;
    const enc = encoding.createEncoder();
    encoding.writeUint8(enc, MSG_FLUSH);
    encoding.writeVarUint8Array(enc, new Uint8Array(0));
    send(enc);
    setTimeout(() => go(false), 8000);
  });
}
