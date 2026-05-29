// Small DOM helpers shared across modules. No state, no init step —
// each function looks up the element it needs. Cheap because the DOM
// tree is cached and these aren't on hot paths.

import { state } from './state.js';

/// Update the status pill in the nav. `kind` ∈ undefined | 'connecting'
/// | 'ready' | 'error' — drives the colour via CSS classes.
export function setStatus(text, kind) {
  const el = document.getElementById('status');
  el.textContent = text;
  el.className = 'status-pill' + (kind ? ' ' + kind : '');
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
    const line3 = document.createElement('div');
    line3.textContent = `${state.knownChannels.size} channel(s), ${state.knownNodes.size} node(s) known`;
    infoBody.append(line3);
  }
  infoCard.hidden = false;
}
