// rustbucket web UI. No inline script (CSP script-src 'self'); all file operations go through the
// same DAV/OCS endpoints the sync clients use, with the session cookie plus the X-CSRF-Token header.
(() => {
  'use strict';
  const meta = (n) => document.querySelector(`meta[name="${n}"]`)?.content || '';
  const csrf = meta('csrf-token');
  const main = document.getElementById('main');
  if (!main) return;
  const user = main.dataset.user;
  const davBase = main.dataset.dav; // /remote.php/dav/files/<user>
  const uploadBase = main.dataset.uploads; // /remote.php/dav/uploads/<user>
  const trashBase = main.dataset.trash; // /remote.php/dav/trashbin/<user>
  const versionsBase = main.dataset.versions; // /remote.php/dav/versions/<user>

  const enc = (p) => p.split('/').filter(Boolean).map(encodeURIComponent).join('/');
  const davUrl = (p) => davBase + '/' + enc(p);
  const join = (dir, name) => (dir === '/' || dir === '' ? '' : dir.replace(/\/$/, '')) + '/' + name;
  const state = () => document.getElementById('files-state')?.dataset || {};
  const dirNow = () => state().dir || '/';

  // HTMX: send the CSRF token with every request it makes
  document.body.addEventListener('htmx:configRequest', (e) => { e.detail.headers['X-CSRF-Token'] = csrf; });
  // an expired session answers 401: go to the login page instead of showing an error fragment
  document.body.addEventListener('htmx:responseError', (e) => {
    if (e.detail.xhr.status === 401) location.href = '/login';
  });

  function toast(msg, kind = 'err') {
    const box = document.getElementById('flash') || (() => {
      const d = document.createElement('div'); d.id = 'flash'; main.prepend(d); return d; })();
    box.className = 'flash ' + kind;
    box.textContent = msg;
    box.setAttribute('role', 'status');
    if (kind === 'ok') setTimeout(() => { if (box.textContent === msg) box.remove(); }, 4000);
  }
  const explain = (status) => ({
    401: 'You were signed out. Please sign in again.',
    403: 'You are not allowed to do that here.',
    404: 'Not found (it may have been removed meanwhile).',
    405: 'That name is already taken.',
    409: 'The target folder does not exist.',
    412: 'That name is already taken.',
    413: 'The file is larger than the limit of this server.',
    507: 'The server is out of space.',
  }[status] || `The server answered ${status}.`);

  // run an async job started by an event handler: an unexpected failure shows a message instead of leaving a rejected promise behind
  const background = (job) => job.catch(() => toast('Something went wrong. Please reload the page and try again.'));

  async function dav(method, path, headers = {}, body) {
    const r = await fetch(davUrl(path), { method, headers: { 'X-CSRF-Token': csrf, ...headers }, body, credentials: 'same-origin' });
    if (r.status === 401) { location.href = '/login'; }
    return r;
  }
  const refresh = () => window.htmx.ajax('GET', location.pathname + location.search, { target: '#files-root', swap: 'innerHTML' });
  const destHeader = (path) => ({ Destination: location.origin + davUrl(path) });

  // ---------- inline forms ----------
  function inlineForm(host, { value = '', placeholder = '', button = 'Save', onSubmit }) {
    const f = document.createElement('form');
    f.className = 'inline-edit';
    const i = document.createElement('input'); i.type = 'text'; i.value = value; i.placeholder = placeholder; i.required = true;
    i.setAttribute('aria-label', placeholder || button);
    const ok = document.createElement('button'); ok.type = 'submit'; ok.className = 'primary'; ok.textContent = button;
    const no = document.createElement('button'); no.type = 'button'; no.textContent = 'Cancel';
    f.append(i, ok, no);
    host.replaceChildren(f);
    i.focus(); i.select();
    f.addEventListener('submit', async (ev) => { ev.preventDefault(); ok.disabled = true; await onSubmit(i.value.trim()); ok.disabled = false; });
    no.addEventListener('click', () => refresh());
    return f;
  }

  const validName = (n) => n && n !== '.' && n !== '..' && !n.includes('/');

  async function act(row, action) {
    const path = row.dataset.path, name = row.dataset.name;
    if (action === 'rename') {
      inlineForm(row.querySelector('.name'), { value: name, placeholder: 'New name', button: 'Rename', onSubmit: async (v) => {
        if (!validName(v)) return toast('That is not a valid name.');
        if (v === name) return refresh();
        const target = join(dirNow(), v);
        const r = await dav('MOVE', path, { ...destHeader(target), Overwrite: 'F' });
        r.ok ? refresh() : toast(explain(r.status));
      } });
    } else if (action === 'move') {
      inlineForm(row.querySelector('.name'), { value: dirNow(), placeholder: 'Target folder, e.g. /Photos', button: 'Move', onSubmit: async (v) => {
        const folder = '/' + enc(v).split('/').map(decodeURIComponent).join('/');
        const r = await dav('MOVE', path, { ...destHeader(join(folder, name)), Overwrite: 'F' });
        r.ok ? refresh() : toast(explain(r.status));
      } });
    } else if (action === 'delete') {
      const btn = row.querySelector('[data-action="delete"]');
      if (btn.dataset.confirm !== '1') {
        btn.dataset.confirm = '1'; btn.dataset.label = btn.textContent; btn.textContent = 'Sure?'; btn.classList.add('danger');
        setTimeout(() => { if (btn.isConnected && btn.dataset.confirm === '1') { btn.dataset.confirm = ''; btn.textContent = btn.dataset.label; } }, 4000);
        return;
      }
      const r = await dav('DELETE', path);
      r.ok ? refresh() : toast(explain(r.status));
    } else if (action === 'share') {
      openShare(path, name);
    } else if (action === 'versions') {
      openVersions(path);
    }
  }

  // ---------- upload ----------
  const chunkSize = () => Number(localStorage.getItem('rbChunkSize')) || 10 * 1024 * 1024;

  function xhr(method, url, { headers = {}, body, onProgress } = {}) {
    return new Promise((resolve) => {
      const x = new XMLHttpRequest();
      x.open(method, url);
      x.setRequestHeader('X-CSRF-Token', csrf);
      for (const [k, v] of Object.entries(headers)) x.setRequestHeader(k, v);
      if (onProgress) x.upload.onprogress = (e) => e.lengthComputable && onProgress(e.loaded);
      x.onload = () => resolve(x.status);
      x.onerror = () => resolve(0);
      x.send(body);
    });
  }

  async function uploadFile(file, dir, ui) {
    const target = join(dir, file.name);
    const mtime = String(Math.floor(file.lastModified / 1000));
    const size = file.size, chunk = chunkSize();
    if (size <= chunk) {
      const st = await xhr('PUT', davUrl(target), { headers: { 'X-OC-Mtime': mtime, 'If-None-Match': '*' }, body: file, onProgress: (n) => ui.set(n, size) });
      return st === 201 || st === 204 ? 0 : st || -1;
    }
    const id = 'web-' + crypto.randomUUID().replace(/-/g, '');
    const base = `${uploadBase}/${id}`;
    const fail = async (st) => { await fetch(base, { method: 'DELETE', headers: { 'X-CSRF-Token': csrf }, credentials: 'same-origin' }); return st || -1; };
    let st = await xhr('MKCOL', base, { headers: { 'OC-Total-Length': String(size), Destination: location.origin + davUrl(target) } });
    if (st !== 201) return fail(st);
    let done = 0;
    for (let off = 0; off < size; off += chunk) {
      const part = file.slice(off, Math.min(off + chunk, size));
      st = await xhr('PUT', `${base}/${String(off).padStart(15, '0')}`, { body: part, onProgress: (n) => ui.set(done + n, size) });
      if (st !== 201) return fail(st);
      done += part.size;
    }
    st = await xhr('MOVE', `${base}/.file`, { headers: { Destination: location.origin + davUrl(target), 'OC-Total-Length': String(size), 'X-OC-Mtime': mtime, 'If-None-Match': '*' } });
    return st === 201 || st === 204 ? 0 : fail(st);
  }

  async function uploadAll(files) {
    const dir = dirNow();
    if (state().canCreate !== '1') return toast('You may not upload into this folder.');
    const box = document.getElementById('uploads');
    let bad = 0;
    for (const file of files) {
      const el = document.createElement('div'); el.className = 'upload'; el.dataset.name = file.name;
      const label = document.createElement('span'); label.textContent = file.name;
      const stat = document.createElement('span'); stat.className = 'state'; stat.textContent = 'waiting';
      const bar = document.createElement('progress'); bar.max = 100; bar.value = 0;
      el.append(label, stat, bar); box.append(el);
      const ui = { set: (n, t) => { bar.value = t ? (100 * n) / t : 100; stat.textContent = Math.round(bar.value) + ' %'; } };
      stat.textContent = '0 %';
      const err = await uploadFile(file, dir, ui);
      if (err) { bad++; el.classList.add('failed'); stat.textContent = err === -1 ? 'failed' : explain(err); }
      else { bar.value = 100; stat.textContent = 'done'; setTimeout(() => el.remove(), 3000); }
    }
    if (!bad) toast('Upload finished.', 'ok');
    refresh();
  }

  // ---------- share dialog ----------
  const dialog = () => document.getElementById('share-dialog');
  function openShare(path, name) {
    const d = dialog();
    document.getElementById('share-body').textContent = 'Loading…';
    d.showModal();
    window.htmx.ajax('GET', '/ui/share?path=' + encodeURIComponent(path), { target: '#share-body', swap: 'innerHTML' });
  }
  function openVersions(path) {
    document.getElementById('share-body').textContent = 'Loading…';
    dialog().showModal();
    window.htmx.ajax('GET', '/ui/versions?path=' + encodeURIComponent(path), { target: '#share-body', swap: 'innerHTML' });
  }
  const reloadVersions = () => { const p = document.getElementById('versions-state')?.dataset.path; if (p) openVersions(p); };
  const raw = (method, url, headers = {}) => fetch(url, { method, headers: { 'X-CSRF-Token': csrf, ...headers }, credentials: 'same-origin' });

  const ocs = async (method, path, form) => {
    const r = await fetch('/ocs/v2.php/apps/files_sharing/api/v1' + path + (path.includes('?') ? '&' : '?') + 'format=json', {
      method, credentials: 'same-origin',
      headers: { 'X-CSRF-Token': csrf, 'OCS-APIRequest': 'true', ...(form ? { 'Content-Type': 'application/x-www-form-urlencoded' } : {}) },
      body: form ? new URLSearchParams(form) : undefined });
    let j = {}; try { j = await r.json(); } catch (_) { /* not json */ }
    return { ok: r.ok, status: r.status, message: j?.ocs?.meta?.message || explain(r.status), data: j?.ocs?.data };
  };
  const reloadShare = () => { const p = document.getElementById('share-state')?.dataset.path; if (p) window.htmx.ajax('GET', '/ui/share?path=' + encodeURIComponent(p), { target: '#share-body', swap: 'innerHTML' }); };
  const shareMsg = (m, kind = 'err') => { const e = document.getElementById('share-msg'); if (e) { e.textContent = m; e.className = 'flash ' + kind; } };

  let shareeKinds = {};
  async function searchSharees(q) {
    const r = await ocs('GET', '/sharees?itemType=file&lookup=false&perPage=20&search=' + encodeURIComponent(q));
    const dl = document.getElementById('sharee-list'); if (!dl || !r.ok) return;
    shareeKinds = {}; dl.replaceChildren();
    for (const u of [...(r.data.exact?.users || []), ...(r.data.users || [])]) { shareeKinds[u.value.shareWith] = 0; const o = document.createElement('option'); o.value = u.value.shareWith; o.label = u.label; dl.append(o); }
    for (const g of [...(r.data.exact?.groups || []), ...(r.data.groups || [])]) { shareeKinds[g.value.shareWith] = 1; const o = document.createElement('option'); o.value = g.value.shareWith; o.label = g.label + ' (group)'; dl.append(o); }
  }
  let searchTimer;

  // ---------- "..." menus ----------
  const closeMenus = (except) => document.querySelectorAll('details.menu[open]').forEach((m) => { if (m !== except) m.open = false; });

  // ---------- multi-select: checkboxes, the bar above the list, bulk move / delete ----------
  const rowChecks = () => [...document.querySelectorAll('#listing .row:not(.head) .sel')];
  const selectedRows = () => rowChecks().filter((c) => c.checked).map((c) => c.closest('.row'));
  const plural = (n) => `${n} item${n === 1 ? '' : 's'}`;

  function updateBulk() {
    const checks = rowChecks();
    const rows = checks.filter((c) => c.checked).map((c) => c.closest('.row'));
    checks.forEach((c) => c.closest('.row').classList.toggle('selected', c.checked));
    const bar = document.getElementById('bulkbar');
    if (!bar) return;
    bar.hidden = rows.length === 0;
    document.getElementById('bulk-count').textContent = `${rows.length} selected`;
    // an action is offered only when every selected item allows it (no half-done surprises)
    const mv = document.getElementById('bulk-move'), del = document.getElementById('bulk-delete');
    mv.disabled = !rows.every((r) => r.dataset.canMove === '1');
    del.disabled = !rows.every((r) => r.dataset.canDelete === '1');
    mv.title = mv.disabled ? 'Some of the selected items cannot be moved.' : '';
    del.title = del.disabled ? 'Some of the selected items cannot be deleted.' : '';
    // a changed selection cancels a pending confirmation or an open "move" form
    del.dataset.confirm = ''; del.textContent = 'Delete';
    const host = document.getElementById('bulk-host'); host.hidden = true; host.replaceChildren();
    const all = document.getElementById('sel-all');
    if (all) { all.checked = checks.length > 0 && rows.length === checks.length; all.indeterminate = rows.length > 0 && rows.length < checks.length; }
  }

  // children before parents, so a folder and something inside it (possible in the "Shared" list) both work
  const deepestFirst = (rows) => rows.sort((a, b) => b.dataset.path.split('/').length - a.dataset.path.split('/').length);

  async function bulk(kind, folder) {
    const rows = deepestFirst(selectedRows());
    const failed = [];
    for (const row of rows) {
      const { path, name } = row.dataset;
      const r = kind === 'delete'
        ? await dav('DELETE', path)
        : await dav('MOVE', path, { ...destHeader(join(folder, name)), Overwrite: 'F' });
      if (!r.ok) failed.push(`${name}: ${explain(r.status)}`);
    }
    await refresh();
    const done = rows.length - failed.length, verb = kind === 'delete' ? 'Deleted' : 'Moved';
    if (failed.length) toast(`${verb} ${done} of ${rows.length}. ${failed.join(' ')}`);
    else toast(`${verb} ${plural(done)}.`, 'ok');
  }

  function bulkAction(b) {
    const n = selectedRows().length;
    if (!n || b.disabled) return;
    if (b.id === 'bulk-clear') { rowChecks().forEach((c) => { c.checked = false; }); return updateBulk(); }
    if (b.id === 'bulk-delete') {
      if (b.dataset.confirm !== '1') {
        b.dataset.confirm = '1'; b.textContent = `Delete ${plural(n)} - sure?`;
        setTimeout(() => { if (b.isConnected && b.dataset.confirm === '1') { b.dataset.confirm = ''; b.textContent = 'Delete'; } }, 4000);
        return;
      }
      b.disabled = true;
      return bulk('delete');
    }
    if (b.id === 'bulk-move') {
      const host = document.getElementById('bulk-host'); host.hidden = false;
      inlineForm(host, { value: dirNow(), placeholder: 'Target folder, e.g. /Photos', button: `Move ${plural(n)}`, onSubmit: async (v) => {
        const folder = '/' + enc(v).split('/').map(decodeURIComponent).join('/');
        await bulk('move', folder);
      } });
    }
  }

  // ---------- events ----------
  document.addEventListener('click', async (e) => {
    closeMenus(e.target.closest('details.menu')); // a click outside an open "..." menu closes it
    const b = e.target.closest('button');
    if (!b) return;
    if (b.id === 'bulk-move' || b.id === 'bulk-delete' || b.id === 'bulk-clear') return bulkAction(b);
    if (b.id === 'btn-upload') return document.getElementById('file-input').click();
    if (b.id === 'btn-newfolder') {
      const host = document.getElementById('newfolder-host'); host.hidden = false;
      return inlineForm(host, { placeholder: 'Folder name', button: 'Create', onSubmit: async (v) => {
        if (!validName(v)) return toast('That is not a valid name.');
        const r = await dav('MKCOL', join(dirNow(), v));
        r.ok ? refresh() : toast(explain(r.status));
      } });
    }
    if (b.dataset.action) {
      const row = b.closest('.row');
      if (row) {
        const menu = b.closest('details.menu');
        const p = act(row, b.dataset.action);
        if (menu && b.dataset.confirm !== '1') menu.open = false;
        return p;
      }
    }
    if (b.id === 'share-close') return dialog().close();
    // trash bin: restore / delete one entry / empty
    if (b.dataset.trash) {
      const row = b.closest('.row'); const url = `${trashBase}/trash/${encodeURIComponent(row.dataset.entry)}`;
      const r = b.dataset.trash === 'restore'
        ? await raw('MOVE', url, { Destination: location.origin + trashBase + '/restore/x' })
        : await raw('DELETE', url);
      return r.ok ? location.reload() : toast(explain(r.status));
    }
    if (b.dataset.trashEmpty !== undefined) {
      if (b.dataset.confirm !== '1') { b.dataset.confirm = '1'; b.textContent = 'Really empty the trash?'; return; }
      const r = await raw('DELETE', `${trashBase}/trash/`);
      return r.ok ? location.reload() : toast(explain(r.status));
    }
    // versions dialog: restore / delete a version
    if (b.dataset.ver) {
      const id = document.getElementById('versions-state').dataset.id, ts = b.dataset.ts;
      const url = `${versionsBase}/versions/${id}/${ts}`;
      const r = b.dataset.ver === 'restore'
        ? await raw('MOVE', url, { Destination: location.origin + versionsBase + '/restore/x' })
        : await raw('DELETE', url);
      if (!r.ok) return shareMsg(explain(r.status));
      if (b.dataset.ver === 'restore') { dialog().close(); return refresh(); }
      return reloadVersions();
    }
    if (b.dataset.shareRemove) {
      const r = await ocs('DELETE', '/shares/' + b.dataset.shareRemove);
      return r.ok ? reloadShare() : shareMsg(r.message);
    }
  });
  document.addEventListener('change', async (e) => {
    const t = e.target;
    if (t.classList?.contains('sel')) updateBulk();
    if (t.id === 'sel-all') { rowChecks().forEach((c) => { c.checked = t.checked; }); updateBulk(); }
    if (t.id === 'file-input') { const files = [...t.files]; t.value = ''; if (files.length) background(uploadAll(files)); }
    if (t.dataset.sharePerm) {
      const r = await ocs('PUT', '/shares/' + t.dataset.sharePerm, { permissions: t.value });
      r.ok ? shareMsg('Saved.', 'ok') : shareMsg(r.message);
    }
  });
  document.addEventListener('input', (e) => {
    if (e.target.id === 'share-with') { clearTimeout(searchTimer); const q = e.target.value; searchTimer = setTimeout(() => searchSharees(q), 200); }
  });
  document.addEventListener('submit', async (e) => {
    if (e.target.id !== 'share-add') return;
    e.preventDefault();
    const f = e.target, who = f.elements.with.value.trim();
    if (!who) return;
    const r = await ocs('POST', '/shares', { shareType: shareeKinds[who] ?? 0, shareWith: who, path: f.elements.path.value, permissions: f.elements.permissions.value });
    r.ok ? reloadShare() : shareMsg(r.message);
  });

  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    const m = document.querySelector('details.menu[open]');
    if (m) { m.open = false; m.querySelector('summary')?.focus(); }
  });

  // drag & drop: files from the desktop onto the page, rows onto folders
  let dragDepth = 0;
  const zone = () => document.getElementById('dropzone');
  const hasFiles = (e) => [...(e.dataTransfer?.types || [])].includes('Files');
  window.addEventListener('dragenter', (e) => { if (hasFiles(e)) { dragDepth++; zone()?.classList.add('active'); } });
  window.addEventListener('dragleave', (e) => { if (hasFiles(e) && --dragDepth <= 0) { dragDepth = 0; zone()?.classList.remove('active'); } });
  window.addEventListener('dragover', (e) => { if (hasFiles(e) || window.__dragRow) e.preventDefault(); });
  window.addEventListener('drop', (e) => {
    zone()?.classList.remove('active'); dragDepth = 0;
    if (hasFiles(e)) { e.preventDefault(); background(uploadAll([...e.dataTransfer.files])); }
  });
  document.addEventListener('dragstart', (e) => { const row = e.target.closest?.('.row[draggable]'); if (row) { window.__dragRow = row; e.dataTransfer.setData('text/plain', row.dataset.path); } });
  document.addEventListener('dragend', () => { window.__dragRow = null; document.querySelectorAll('.drop-target').forEach((x) => x.classList.remove('drop-target')); });
  document.addEventListener('dragover', (e) => { const row = e.target.closest?.('.row[data-dir="1"]'); if (row && window.__dragRow && row !== window.__dragRow) { e.preventDefault(); row.classList.add('drop-target'); } });
  document.addEventListener('dragleave', (e) => e.target.closest?.('.row')?.classList.remove('drop-target'));
  document.addEventListener('drop', async (e) => {
    const row = e.target.closest?.('.row[data-dir="1"]'); const src = window.__dragRow;
    if (!row || !src || row === src) return;
    e.preventDefault();
    const r = await dav('MOVE', src.dataset.path, { ...destHeader(join(row.dataset.path, src.dataset.name)), Overwrite: 'F' });
    r.ok ? refresh() : toast(explain(r.status));
  });
})();
