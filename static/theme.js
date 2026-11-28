// Applies the stored colour theme before the page is painted (no flash). Loaded synchronously from <head>.
(() => {
  try {
    const t = localStorage.getItem('rbTheme');
    if (t === 'dark' || t === 'light') document.documentElement.dataset.theme = t;
  } catch (_) { /* storage blocked: stay on the system setting */ }
})();
