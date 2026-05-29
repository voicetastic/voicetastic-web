// Entry point. Loads the wasm module, wires connect / disconnect /
// discover, runs the hash router + mobile nav, and bridges the
// `on_event` / `on_voice` callbacks to the events + chat modules.
//
// Module ownership map:
//   state.js     — shared mutable state object (client, myNode*, known*)
//   ui.js        — log, setStatus, codeEl, updateInfoCard
//   events.js    — handleEvent, waitForApplyConfirm + setEventHooks
//   chat.js      — chat threads, mic capture, voice playback
//   settings.js  — Settings page (Meshtastic + Audio)
//   app.js       — this file: bootstrap, routing, connect lifecycle

import init, { connect } from '../pkg/voicetastic_web.js';
import { state, resetDeviceState } from './state.js';
import { log, setStatus } from './ui.js';
import { handleEvent, setEventHooks } from './events.js';
import { initChat, onVoice, renderChat, clearThreads, setChatEnabled } from './chat.js';
import { initSettings, renderSettings, setAudioControlsEnabled } from './settings.js';

// ---------- DOM refs owned by this module ----------

const connectBtn = document.getElementById('connect');
const disconnectBtn = document.getElementById('disconnect');
const discoverBtn = document.getElementById('discover');
const connectHint = document.getElementById('connect-hint');
const infoCard = document.getElementById('info');

// ---------- hash routing ----------

const ROUTES = ['connect', 'chat', 'settings', 'map'];
function route() {
  const m = location.hash.match(/^#\/([a-z]+)/);
  const r = m && ROUTES.includes(m[1]) ? m[1] : 'connect';
  for (const id of ROUTES) {
    document.getElementById('page-' + id).classList.toggle('active', id === r);
  }
  for (const a of document.querySelectorAll('.nav-links a[data-route]')) {
    a.classList.toggle('active', a.dataset.route === r);
  }
}
window.addEventListener('hashchange', route);
if (!location.hash) location.hash = '#/connect';
route();

// Re-render the Settings page when the user navigates to it — the
// snapshot may have changed in the background.
window.addEventListener('hashchange', () => {
  if (location.hash.startsWith('#/settings')) renderSettings();
});

// ---------- mobile nav hamburger ----------

const navToggle = document.querySelector('.nav-toggle');
const navLinks = document.getElementById('nav-links');
navToggle?.addEventListener('click', () => {
  const open = navLinks.classList.toggle('open');
  navToggle.setAttribute('aria-expanded', String(open));
});
navLinks.querySelectorAll('a').forEach((a) =>
  a.addEventListener('click', () => {
    navLinks.classList.remove('open');
    navToggle?.setAttribute('aria-expanded', 'false');
  }),
);

// ---------- connect / disconnect UI ----------

// Toggle every input that should only be live while a radio is
// connected. Called with `true` after a successful connect, and
// `false` after disconnect (or to undo a partial connect).
function setConnectedUi(on) {
  setChatEnabled(on);
  setAudioControlsEnabled(on);
  discoverBtn.hidden = !on;
  if (!on) discoverBtn.disabled = true; // re-enabled at next ConfigComplete
  connectBtn.hidden = on;
  connectBtn.disabled = on;
  disconnectBtn.hidden = !on;
}

// events.js doesn't know about settings or the connect-page UI; let it
// hand control back here when ConfigComplete lands.
setEventHooks({
  onConfigComplete: () => {
    connectHint.textContent = 'Connected and configured. Go to Chat to start talking.';
    discoverBtn.disabled = false;
    renderSettings();
  },
});

// ---------- module init ----------

initChat();
initSettings();

// ---------- bootstrap ----------

if (!('serial' in navigator)) {
  log('Web Serial unavailable. Use Chrome/Edge or Firefox 151+ over localhost/HTTPS.');
  connectBtn.disabled = true;
  setStatus('Unsupported', 'error');
} else {
  await init();
  log('WASM loaded. Ready to connect.');

  connectBtn.onclick = async () => {
    connectBtn.disabled = true;
    setStatus('Connecting…', 'connecting');
    connectHint.textContent = 'Pick a serial port in the browser prompt…';
    log('Requesting port…');
    try {
      state.client = await connect(handleEvent, onVoice);
      log('Connected. Config handshake in flight…');
      setStatus('Connected', 'connecting');
      connectHint.textContent = 'Connected — waiting for ConfigComplete…';
      setConnectedUi(true);
      renderChat(); // refresh placeholder text
    } catch (e) {
      log('❌ ' + e);
      setStatus('Disconnected');
      connectBtn.disabled = false;
      connectHint.textContent = 'Click Connect, then pick the serial port.';
    }
  };

  disconnectBtn.onclick = async () => {
    if (!state.client) return;
    // Tear down the JS-side state up front, before the awaited
    // disconnect — `disconnect()` consumes the WebClient on the Rust
    // side, so any sendText/sendVoice that lands between the await
    // resolving and the UI-gate update would hit a freed proxy.
    const client = state.client;
    state.client = null;
    disconnectBtn.disabled = true;
    setConnectedUi(false);
    log('Disconnecting…');
    try {
      await client.disconnect();
      log('Disconnected.');
    } catch (e) {
      log('disconnect: ' + e);
    }
    disconnectBtn.disabled = false;
    setStatus('Disconnected');
    connectHint.textContent = 'Click Connect, then pick the serial port.';
    resetDeviceState();
    infoCard.hidden = true;
    clearThreads();
    renderSettings();
  };

  discoverBtn.onclick = async () => {
    if (!state.client) return;
    discoverBtn.disabled = true;
    const prevLabel = discoverBtn.textContent;
    discoverBtn.textContent = '🔍 Scanning…';
    log('  ⟶ broadcasting NodeInfo discovery ping (want_response=true)');
    try {
      await state.client.discoverNodes();
      log('  ⟶ scan ping sent — replies arrive over the next few seconds as NodeInfo events');
    } catch (e) {
      log('❌ scan failed: ' + e);
    } finally {
      // Cooldown so we don't saturate the mesh.
      setTimeout(() => {
        discoverBtn.textContent = prevLabel;
        discoverBtn.disabled = false;
      }, 4000);
    }
  };
}
