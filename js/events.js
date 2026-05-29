// Inbound-event dispatch + apply-confirmation watcher.
//
// `handleEvent` is the JS side of the wasm crate's `on_event` callback
// (events.rs::build_event). The wasm side ships structured
// `{ type, text, ...fields }` objects so we can switch on `type`
// instead of regex-parsing a string. `text` is always set, and lands
// in the user-visible log untouched.
//
// `waitForApplyConfirm` is the small promise registry settings.js uses
// to wait for the radio's re-emit of a Config/Owner/Channel event
// after an admin write. Each pending key resolves on the next matching
// event or rejects via timeout.

import { state } from './state.js';
import { log, setStatus, updateInfoCard } from './ui.js';
import { ensureThread, routeIncoming } from './chat.js';

// ---------- callbacks owned by app.js ----------
//
// The events module needs to trigger a Settings re-render and some
// connect-page state updates on `config_complete`. Rather than
// importing from settings/app (which would create a cycle / pull DOM
// concerns into events), app.js installs hooks here.

let hooks = {};
export function setEventHooks(h) { hooks = h; }

// ---------- apply-confirmation watchers ----------

const pendingApplies = new Map();

/// Wait for the radio to re-emit an event matching `key` after an
/// admin write. Resolves `true` on confirmation, `false` on timeout
/// (default 3 s). A second waiter for the same key supersedes the
/// first (that one resolves `false`).
export function waitForApplyConfirm(key, timeoutMs = 3000) {
  return new Promise((resolve) => {
    pendingApplies.get(key)?.cancel();
    const timer = setTimeout(() => {
      pendingApplies.delete(key);
      resolve(false);
    }, timeoutMs);
    pendingApplies.set(key, {
      confirm() { clearTimeout(timer); pendingApplies.delete(key); resolve(true); },
      cancel() { clearTimeout(timer); pendingApplies.delete(key); resolve(false); },
    });
  });
}

function fireApplyConfirm(key) {
  pendingApplies.get(key)?.confirm();
}

// ---------- main dispatch ----------

/// Dispatch one structured event from the wasm driver.
export function handleEvent(ev) {
  log('  ⟵ ' + ev.text);
  switch (ev.type) {
    case 'my_info': {
      state.myNodeNum = ev.node_num;
      state.myNodeHex = '!' + ev.node_num.toString(16);
      updateInfoCard();
      break;
    }
    case 'metadata': {
      state.fwVersion = ev.firmware_version;
      updateInfoCard();
      break;
    }
    case 'channel': {
      const idx = ev.index;
      state.knownChannels.set(idx, `Channel ${idx}`);
      ensureThread(`channel:${idx}`, idx === 0 ? 'Broadcast — Channel 0' : `Channel ${idx}`);
      updateInfoCard();
      break;
    }
    case 'node_info': {
      const hex = '!' + ev.node_num.toString(16);
      const name = ev.long_name || hex;
      state.knownNodes.set(hex, name);
      if (hex !== state.myNodeHex) ensureThread(`node:${hex}`, `DM — ${name} (${hex})`);
      updateInfoCard();
      break;
    }
    case 'config_complete': {
      setStatus('Ready', 'ready');
      hooks.onConfigComplete?.();
      break;
    }
    case 'incoming_text': {
      routeIncoming(ev.from, ev.to, ev.channel, ev.body, 'in');
      break;
    }
    // Apply-confirmation watchers fire on the radio's re-emit.
    case 'config':
      fireApplyConfirm(`config:${ev.section}`);
      break;
    case 'owner':
      fireApplyConfirm('owner');
      break;
    // 'log', 'queue_status', 'incoming_data' — log only.
    default:
      break;
  }
  // `channel` events also feed the channel-apply watcher; done outside
  // the switch because 'channel' is also used for chat-thread bootstrap
  // during the initial config burst above.
  if (ev.type === 'channel') fireApplyConfirm(`channel:${ev.index}`);
}
