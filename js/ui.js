// Small DOM helpers shared across modules. No state, no init step —
// each function looks up the element it needs. Cheap because the DOM
// tree is cached and these aren't on hot paths.

import { state } from './state.js';

// The most recent `setStatus` kind, so `updateBrand` (which also fires
// on later node-info updates) can re-derive the header state without
// the caller re-passing it.
let currentStatusKind = null;

/// Update the status pill in the nav. `kind` ∈ undefined | 'connecting'
/// | 'ready' | 'error' — drives the colour via CSS classes.
export function setStatus(text, kind) {
  const el = document.getElementById('status');
  if (el) {
    el.textContent = text;
    el.className = 'status-pill' + (kind ? ' ' + kind : '');
  }
  currentStatusKind = kind || null;
  updateBrand();
}

/// Reflect connection state + self identity in the header brand,
/// mirroring the Android top app bar: the logo gets a coloured ring
/// (green when ready, amber + pulse while connecting, red when
/// disconnected) and, once fully connected, the "Voicetastic / web"
/// wordmark is replaced by our node's name and id. The long name
/// arrives a beat after the id (via the self NodeInfo), so we fall back
/// to the id-only view until it lands — matching the Android behaviour.
export function updateBrand() {
  const brand = document.getElementById('brand');
  const textEl = document.getElementById('brand-text');
  if (!brand || !textEl) return;

  const ringClass = currentStatusKind === 'ready' ? 'is-connected'
    : currentStatusKind === 'connecting' ? 'is-connecting'
    : 'is-disconnected';
  brand.classList.remove('is-connected', 'is-connecting', 'is-disconnected');
  brand.classList.add(ringClass);

  textEl.replaceChildren();
  if (currentStatusKind === 'ready' && state.myNodeHex) {
    const name = state.knownNodes.get(state.myNodeHex);
    const longName = name && name !== state.myNodeHex ? name : null;
    const primary = document.createElement('span');
    primary.className = 'brand-name';
    primary.textContent = longName || state.myNodeHex;
    textEl.append(primary);
    if (longName) {
      const sub = document.createElement('span');
      sub.className = 'brand-sub';
      sub.textContent = state.myNodeHex;
      textEl.append(sub);
    }
    brand.title = `Connected as ${longName || state.myNodeHex}`;
  } else {
    const nameEl = document.createElement('span');
    nameEl.className = 'brand-name';
    nameEl.textContent = 'Voicetastic';
    const tag = document.createElement('span');
    tag.className = 'brand-tag';
    tag.textContent = 'web';
    textEl.append(nameEl, tag);
    brand.removeAttribute('title');
  }
}

/// Append one line to the Connect-page event log, scrolling to the
/// bottom. Replaces the initial 'Idle.' placeholder rather than
/// stacking under it.
export function log(line) {
  const el = document.getElementById('log');
  el.classList.remove('muted');
  el.textContent += (el.textContent === 'Idle.' ? '' : '\n') + line;
  el.scrollTop = el.scrollHeight;
}

/// Build a <code>text</code> element via textContent so radio-supplied
/// strings (firmware version, node hex) are never parsed as HTML.
export function codeEl(text) {
  const c = document.createElement('code');
  c.textContent = text;
  return c;
}

/// Canonical Meshtastic node address from a 32-bit id: `!aabbccdd`,
/// always 8 hex digits, lowercase, leading `!`. This is the only place
/// in the UI that does the hex formatting — every caller takes the raw
/// number from the wasm boundary and runs it through here.
export function nodeAddr(n) {
  return '!' + ((n >>> 0).toString(16).padStart(8, '0'));
}

/// Display name for a node: `"Long Name (!aabbccdd)"` when we've seen a
/// `NodeInfo` for it, otherwise just `"!aabbccdd"`. Source of truth is
/// `state.knownNodes`, keyed by `nodeAddr(n)`.
export function nodeDisplay(n) {
  const addr = nodeAddr(n);
  const name = state.knownNodes.get(addr);
  return name && name !== addr ? `${name} (${addr})` : addr;
}

/// Redraw the Connect-page info card from current `state.*` fields.
/// Called from event handlers as MyInfo/Metadata/NodeInfo/Channel
/// events land; everything reads from `state` so no parameters are
/// needed.
export function updateInfoCard() {
  const infoBody = document.getElementById('info-body');
  const infoCard = document.getElementById('info');
  if (state.myNodeNum == null) {
    infoBody.textContent = 'Waiting for config…';
  } else {
    infoBody.replaceChildren();
    const line1 = document.createElement('div');
    line1.append('Node ', codeEl(state.myNodeHex), ` (${state.myNodeNum})`);
    infoBody.append(line1);
    if (state.fwVersion) {
      const line2 = document.createElement('div');
      line2.append('Firmware ', codeEl(state.fwVersion));
      infoBody.append(line2);
    }
    // Mirror the firmware version on the Settings → Firmware
    // update card so the user can compare against a downloaded
    // release without leaving the Settings tab.
    const fwCurrent = document.getElementById('fw-current');
    if (fwCurrent) {
      fwCurrent.textContent = state.fwVersion || '—';
    }
    const line3 = document.createElement('div');
    line3.textContent = `${state.knownChannels.size} channel(s), ${state.knownNodes.size} node(s) known`;
    infoBody.append(line3);
  }
  infoCard.hidden = false;
  // The self node's long name lands via NodeInfo after we're already
  // 'ready'; refresh the header so it fills in the name once known.
  updateBrand();
}
