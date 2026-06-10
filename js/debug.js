// Debug tab: structured event log rendered from `state.debugLog`.
// Re-renders on hashchange to /debug and on the Clear button; the
// log itself is populated by events.js (every inbound event) and
// the page is intentionally not pushed-into on every event to keep
// the main thread free. The user sees the current snapshot whenever
// they navigate to /debug.

import { state } from './state.js';

let listEl, sourceEl, levelEl, countEl;
// Length of state.debugLog at the last render, so the auto-refresh tick can
// skip the (relatively expensive) re-render when nothing new has arrived.
let lastRenderedLen = -1;

export function initDebug() {
  listEl = document.getElementById('debug-list');
  sourceEl = document.getElementById('debug-source');
  levelEl = document.getElementById('debug-level');
  countEl = document.getElementById('debug-count');
  document.getElementById('debug-clear').onclick = () => {
    state.debugLog.length = 0;
    renderDebug();
  };
  sourceEl.onchange = renderDebug;
  levelEl.onchange = renderDebug;

  // Auto-refresh while the Debug tab is visible. events.js appends to
  // state.debugLog without re-rendering (to keep the main thread free), so
  // poll once a second and re-render only when the tab is active and the log
  // has actually grown. The length guard makes the idle tick nearly free.
  setInterval(() => {
    if (!location.hash.startsWith('#/debug')) return;
    if (state.debugLog.length === lastRenderedLen) return;
    renderDebug();
  }, 1000);
}

export function renderDebug() {
  if (!listEl) return;

  // Populate the source filter from the union of types seen so far,
  // preserving the user's current selection.
  const seenSources = new Set(state.debugLog.map((e) => e.source).filter(Boolean));
  const current = sourceEl.value;
  const known = new Set(Array.from(sourceEl.options).map((o) => o.value).filter(Boolean));
  for (const src of seenSources) {
    if (!known.has(src)) {
      const opt = document.createElement('option');
      opt.value = src;
      opt.textContent = src;
      sourceEl.appendChild(opt);
    }
  }
  if (current) sourceEl.value = current;

  const sourceFilter = sourceEl.value;
  const levelFilter = levelEl.value;
  const entries = state.debugLog
    .filter((e) => !sourceFilter || e.source === sourceFilter)
    .filter((e) => !levelFilter || e.level === levelFilter);

  // Mark this render so the auto-refresh tick can short-circuit until the
  // log grows again. Set before the early return so an empty log counts too.
  lastRenderedLen = state.debugLog.length;

  countEl.textContent = `${entries.length} of ${state.debugLog.length} entries`;
  if (entries.length === 0) {
    listEl.innerHTML = '<div class="placeholder muted">No events match the filter.</div>';
    return;
  }

  // Stick to the bottom only if the user is already there; if they've
  // scrolled up to read history, preserve their position across the refresh.
  const atBottom = listEl.scrollHeight - listEl.scrollTop - listEl.clientHeight < 40;
  const prevScroll = listEl.scrollTop;

  const icon = (l) => l === 'error' ? '✗' : l === 'warn' ? '⚠' : '·';
  const fmt = (e) => {
    const d = new Date(e.at);
    const ts = `${d.getHours().toString().padStart(2,'0')}:${d.getMinutes().toString().padStart(2,'0')}:${d.getSeconds().toString().padStart(2,'0')}`;
    const escape = (s) => String(s).replace(/[&<>]/g, (c) => ({ '&':'&amp;','<':'&lt;','>':'&gt;' }[c]));
    return `<div class="debug-line debug-${e.level}"><span class="ts">${ts}</span> ${icon(e.level)} <span class="src">[${escape(e.source)}]</span> ${escape(e.msg)}</div>`;
  };
  // Newest at bottom (oldest first); the container scrolls.
  listEl.innerHTML = entries.map(fmt).join('');
  listEl.scrollTop = atBottom ? listEl.scrollHeight : prevScroll;
}
