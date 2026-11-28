// In-page viewer for images and videos: a click or tap on such a file in the list shows it full screen,
// the X (top right) or Esc closes it. Images can be zoomed (wheel, pinch, +/- buttons, double click/tap) and
// moved when zoomed in. Without JavaScript the link still downloads the file. No inline script or style (CSP):
// positions are set through the element's style object, which the policy allows.
(() => {
  'use strict';
  const MAX_ZOOM = 8;
  let box = null; // the open viewer: { root, close, ... }

  const el = (tag, cls, attrs = {}) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
    return e;
  };

  // the media rows of the list that is on the screen now, in order (for the arrows)
  const mediaRows = () => Array.from(document.querySelectorAll('#listing .row[data-media]'));
  const rowUrl = (row) => row.querySelector('.name a')?.getAttribute('href') || '';

  function open(row) {
    close();
    const rows = mediaRows();
    let index = Math.max(0, rows.indexOf(row));
    const root = el('div', 'viewer', { role: 'dialog', 'aria-modal': 'true', 'aria-label': 'Viewer' });
    const bar = el('div', 'viewer-bar');
    const title = el('span', 'viewer-title');
    const zoomOut = el('button', 'viewer-btn', { type: 'button', 'aria-label': 'Zoom out', title: 'Zoom out (-)' });
    zoomOut.textContent = '−';
    const zoomLabel = el('span', 'viewer-zoom', { 'aria-live': 'polite' });
    const zoomIn = el('button', 'viewer-btn', { type: 'button', 'aria-label': 'Zoom in', title: 'Zoom in (+)' });
    zoomIn.textContent = '+';
    const fit = el('button', 'viewer-btn', { type: 'button', 'aria-label': 'Fit to screen', title: 'Fit to screen (0)' });
    fit.textContent = 'Fit';
    const download = el('a', 'viewer-btn', { 'aria-label': 'Download', title: 'Download' });
    download.textContent = '↓';
    const closeBtn = el('button', 'viewer-btn viewer-close', { type: 'button', 'aria-label': 'Close', title: 'Close (Esc)' });
    closeBtn.textContent = '✕';
    bar.append(title, zoomOut, zoomLabel, zoomIn, fit, download, closeBtn);
    const stage = el('div', 'viewer-stage');
    const prev = el('button', 'viewer-nav viewer-prev', { type: 'button', 'aria-label': 'Previous' });
    prev.textContent = '‹';
    const next = el('button', 'viewer-nav viewer-next', { type: 'button', 'aria-label': 'Next' });
    next.textContent = '›';
    root.append(stage, bar, prev, next);
    document.body.append(root);
    document.body.classList.add('viewer-open');

    // ---- zoom and pan (images): scale is relative to "fit to screen"
    let img = null;
    let scale = 1, tx = 0, ty = 0;
    const pointers = new Map();
    let pinch = null, drag = null, lastTap = 0;

    const limits = () => {
      if (!img) return { mx: 0, my: 0 };
      const w = img.clientWidth * scale, h = img.clientHeight * scale;
      return { mx: Math.max(0, (w - stage.clientWidth) / 2), my: Math.max(0, (h - stage.clientHeight) / 2) };
    };
    const apply = () => {
      if (!img) return;
      const { mx, my } = limits();
      tx = Math.min(mx, Math.max(-mx, tx));
      ty = Math.min(my, Math.max(-my, ty));
      img.style.transform = `translate(${tx}px, ${ty}px) scale(${scale})`;
      zoomLabel.textContent = Math.round(scale * 100) + '%';
      root.classList.toggle('zoomed', scale > 1.001);
    };
    // zoom to `s`, keeping the point (cx, cy) of the stage (in px from its centre) where it is
    const zoomTo = (s, cx = 0, cy = 0) => {
      const ns = Math.min(MAX_ZOOM, Math.max(1, s));
      const k = ns / scale;
      tx = cx - (cx - tx) * k;
      ty = cy - (cy - ty) * k;
      scale = ns;
      if (scale === 1) { tx = 0; ty = 0; }
      apply();
    };
    const centre = (clientX, clientY) => {
      const r = stage.getBoundingClientRect();
      return [clientX - (r.left + r.width / 2), clientY - (r.top + r.height / 2)];
    };

    stage.addEventListener('wheel', (e) => {
      if (!img) return;
      e.preventDefault();
      const [cx, cy] = centre(e.clientX, e.clientY);
      zoomTo(scale * Math.exp(-e.deltaY * 0.0015), cx, cy);
    }, { passive: false });
    stage.addEventListener('pointerdown', (e) => {
      if (!img || e.target !== img) return;
      stage.setPointerCapture(e.pointerId);
      pointers.set(e.pointerId, [e.clientX, e.clientY]);
      if (pointers.size === 2) {
        const [a, b] = [...pointers.values()];
        pinch = { d: Math.hypot(a[0] - b[0], a[1] - b[1]), s: scale };
        drag = null;
      } else {
        drag = { x: e.clientX, y: e.clientY, tx, ty, moved: false };
      }
    });
    stage.addEventListener('pointermove', (e) => {
      if (!pointers.has(e.pointerId)) return;
      pointers.set(e.pointerId, [e.clientX, e.clientY]);
      if (pinch && pointers.size >= 2) {
        const [a, b] = [...pointers.values()];
        const [cx, cy] = centre((a[0] + b[0]) / 2, (a[1] + b[1]) / 2);
        zoomTo(pinch.s * Math.hypot(a[0] - b[0], a[1] - b[1]) / pinch.d, cx, cy);
      } else if (drag && scale > 1) {
        const dx = e.clientX - drag.x, dy = e.clientY - drag.y;
        if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
        tx = drag.tx + dx; ty = drag.ty + dy;
        apply();
      }
    });
    const lift = (e) => {
      const was = drag;
      pointers.delete(e.pointerId);
      if (pointers.size < 2) pinch = null;
      if (pointers.size === 0) {
        drag = null;
        // double tap / double click: toggle between fit and 2.5x at that spot
        if (was && !was.moved && e.type === 'pointerup') {
          const now = Date.now();
          if (now - lastTap < 350) {
            const [cx, cy] = centre(e.clientX, e.clientY);
            zoomTo(scale > 1.001 ? 1 : 2.5, cx, cy);
            lastTap = 0;
          } else {
            lastTap = now;
          }
        }
      } else if (pointers.size === 1) {
        const [x, y] = [...pointers.values()][0];
        drag = { x, y, tx, ty, moved: true };
      }
    };
    stage.addEventListener('pointerup', lift);
    stage.addEventListener('pointercancel', lift);
    zoomIn.addEventListener('click', () => zoomTo(scale * 1.5));
    zoomOut.addEventListener('click', () => zoomTo(scale / 1.5));
    fit.addEventListener('click', () => zoomTo(1));

    // ---- show one item
    function show(i) {
      index = i;
      const r = rows[i];
      const url = rowUrl(r);
      const name = r.dataset.name || '';
      title.textContent = name;
      download.href = url;
      download.setAttribute('download', name);
      stage.replaceChildren();
      img = null; scale = 1; tx = 0; ty = 0; pointers.clear(); pinch = drag = null;
      const isImage = r.dataset.media === 'image';
      root.classList.toggle('is-video', !isImage);
      for (const b of [zoomOut, zoomIn, fit]) b.hidden = !isImage;
      zoomLabel.hidden = !isImage;
      if (isImage) {
        img = el('img', 'viewer-img', { src: url, alt: name, draggable: 'false' });
        img.addEventListener('load', apply);
        stage.append(img);
        apply();
      } else {
        const v = el('video', 'viewer-video', { src: url, controls: '', playsinline: '', preload: 'metadata' });
        stage.append(v);
        zoomLabel.textContent = '';
      }
      prev.hidden = i <= 0;
      next.hidden = i >= rows.length - 1;
      closeBtn.focus({ preventScroll: true });
    }

    const step = (d) => { const n = index + d; if (n >= 0 && n < rows.length) show(n); };
    prev.addEventListener('click', () => step(-1));
    next.addEventListener('click', () => step(1));
    closeBtn.addEventListener('click', close);
    // a click on the dark area around the picture closes it too (not on the bar, arrows, picture or video controls)
    stage.addEventListener('click', (e) => { if (e.target === stage) close(); });

    const onKey = (e) => {
      if (e.key === 'Escape') { e.preventDefault(); close(); }
      else if (e.key === 'ArrowLeft') step(-1);
      else if (e.key === 'ArrowRight') step(1);
      else if (img && (e.key === '+' || e.key === '=')) zoomTo(scale * 1.5);
      else if (img && (e.key === '-' || e.key === '_')) zoomTo(scale / 1.5);
      else if (img && e.key === '0') zoomTo(1);
    };
    document.addEventListener('keydown', onKey);
    const onResize = () => apply();
    window.addEventListener('resize', onResize);

    box = {
      root,
      dispose() {
        document.removeEventListener('keydown', onKey);
        window.removeEventListener('resize', onResize);
        root.remove();
        document.body.classList.remove('viewer-open');
      },
    };
    show(index);
  }

  function close() {
    if (!box) return;
    const b = box;
    box = null;
    b.dispose();
  }

  // click or tap on the name of an image/video: show it (modified clicks keep their normal meaning)
  document.addEventListener('click', (e) => {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    const a = e.target.closest?.('#listing .row[data-media] .name a');
    if (!a) return;
    e.preventDefault();
    open(a.closest('.row'));
  });
})();
