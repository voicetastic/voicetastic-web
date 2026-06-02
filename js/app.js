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

import init, { connect, connectBle } from '../pkg/voicetastic_web.js';
import { state, resetDeviceState } from './state.js';
import { log, setStatus } from './ui.js';
import { handleEvent, setEventHooks } from './events.js';
import { initChat, onVoice, renderChat, clearThreads, setChatEnabled, renderNodes } from './chat.js';
import { initSettings, renderSettings, setAudioControlsEnabled } from './settings.js';
import { initDebug, renderDebug } from './debug.js';
import { renderMap } from './map.js';

// ---------- DOM refs owned by this module ----------

const connectBtn = document.getElementById('connect');
const connectBleBtn = document.getElementById('connect-ble');
const disconnectBtn = document.getElementById('disconnect');
const discoverBtn = document.getElementById('discover');
const connectHint = document.getElementById('connect-hint');
const infoCard = document.getElementById('info');

// ---------- hash routing ----------

const ROUTES = ['connect', 'chat', 'settings', 'map', 'debug'];
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
  if (location.hash.startsWith('#/debug')) renderDebug();
  // Leaflet needs a sized container; calling renderMap on the
  // hashchange (rather than at startup) means the /map section is
  // already display:block by the time the layer initialises.
  if (location.hash.startsWith('#/map')) renderMap();
});

// ---------- appearance / theme picker ----------
//
// Theme preference lives in localStorage as `voicetastic-theme` ∈
// {system, dark, light}. `system` follows the OS via prefers-color-scheme.
// Anything else pins the `data-theme` attribute on <html>. Settings page
// has the picker; here we apply the saved preference at startup and
// listen for OS theme changes when in `system` mode.

const themePicker = document.getElementById('theme-picker');
const themeMql = window.matchMedia('(prefers-color-scheme: light)');

function applyTheme(pref) {
  const root = document.documentElement;
  const resolved = pref === 'system' ? (themeMql.matches ? 'light' : 'dark') : pref;
  if (resolved === 'dark') root.removeAttribute('data-theme');
  else root.setAttribute('data-theme', resolved);
}

const savedTheme = localStorage.getItem('voicetastic-theme') || 'system';
if (themePicker) themePicker.value = savedTheme;
applyTheme(savedTheme);

themePicker?.addEventListener('change', () => {
  const pref = themePicker.value;
  localStorage.setItem('voicetastic-theme', pref);
  applyTheme(pref);
});

themeMql.addEventListener('change', () => {
  const pref = localStorage.getItem('voicetastic-theme') || 'system';
  if (pref === 'system') applyTheme('system');
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
  connectBleBtn.hidden = on;
  connectBleBtn.disabled = on;
  disconnectBtn.hidden = !on;
}

function hasWebBluetooth() {
  return typeof navigator !== 'undefined' && 'bluetooth' in navigator;
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
initDebug();

// ---------- bootstrap ----------

// We leave both buttons clickable even when the corresponding API
// isn't visible to JS. The browser hides `navigator.serial` and
// `navigator.bluetooth` entirely on insecure origins (HTTP that
// isn't `localhost`), so a "greyed out" button looks like a bug
// to the user when really the page just needs HTTPS. Letting the
// click through and surfacing the underlying error ("Web Bluetooth
// not available — use Chrome/Edge/Opera over localhost or HTTPS")
// is more honest. The wasm side returns a clear message in both
// the "wrong browser" and "wrong context" cases.
const hasSerial = 'serial' in navigator;
const hasBle = hasWebBluetooth();
{
  // Tooltips give the user a hint before they click; the click
  // itself never silently fails because the wasm helper rejects
  // with the same explanation.
  if (!hasSerial) {
    connectBtn.title =
      'Web Serial needs a Chromium browser or Firefox 151+, served over HTTPS or localhost.';
  }
  if (!hasBle) {
    connectBleBtn.title =
      'Web Bluetooth needs a Chromium browser (Chrome / Edge / Opera), served over HTTPS or localhost.';
  }
  await init();
  log('WASM loaded. Ready to connect.');

  /// Shared post-connect bookkeeping for both Serial and BLE paths.
  /// Mirrors the previous inline body of the Serial onclick handler.
  const onClientConnected = (client) => {
    state.client = client;
    log('Connected. Config handshake in flight…');
    setStatus('Connected', 'connecting');
    connectHint.textContent = 'Connected — waiting for ConfigComplete…';
    setConnectedUi(true);
    renderChat();
  };

  connectBtn.onclick = async () => {
    connectBtn.disabled = true;
    connectBleBtn.disabled = true;
    setStatus('Connecting…', 'connecting');
    connectHint.textContent = 'Pick a serial port in the browser prompt…';
    log('Requesting port…');
    try {
      onClientConnected(await connect(handleEvent, onVoice));
    } catch (e) {
      log('❌ ' + e);
      setStatus('Disconnected');
      connectBtn.disabled = false;
      connectBleBtn.disabled = false;
      connectHint.textContent = 'Pick a transport above, then approve the device in the browser prompt.';
    }
  };

  connectBleBtn.onclick = async () => {
    connectBtn.disabled = true;
    connectBleBtn.disabled = true;
    setStatus('Connecting…', 'connecting');
    connectHint.textContent = 'Pick a Meshtastic device in the Bluetooth prompt…';
    log('Requesting BLE device…');
    try {
      onClientConnected(await connectBle(handleEvent, onVoice));
    } catch (e) {
      log('❌ ' + e);
      setStatus('Disconnected');
      connectBtn.disabled = false;
      connectBleBtn.disabled = false;
      connectHint.textContent = 'Pick a transport above, then approve the device in the browser prompt.';
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
    renderNodes();
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
