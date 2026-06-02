// Cross-module mutable state. Imports read live (`state.client`, etc.)
// — don't destructure on import or you'll capture a stale snapshot.
//
// Module-local state stays inside its module file: thread lists in
// chat.js, pending-apply watchers in events.js, etc. Only fields that
// truly cross module boundaries live here.

export const state = {
  /// The wasm-side `WebClient` once `connect(...)` resolves. Null while
  /// disconnected; set by app.js and consumed by chat/settings for
  /// `sendText`, `sendVoice`, `setFixedPosition`, etc.
  client: null,

  /// Which transport the active `client` was opened with: `'serial'` or
  /// `'ble'` while connected, `null` between sessions. Surfaces the
  /// transport-specific UI bits (e.g. the BLE "Forget device" button)
  /// without re-deriving it from `client` (which would need a wasm
  /// round-trip).
  transport: null,

  /// Identity + capability of the attached radio. Set by events.js as
  /// the corresponding `InboundEvent`s land; consumed by chat (DM
  /// routing) and the info card. Reset on disconnect via
  /// `resetDeviceState`.
  myNodeNum: null,
  myNodeHex: null,
  fwVersion: null,

  /// Indexed lookups built up from `node_info` / `channel` events.
  /// chat.js threads its DM rows by node hex; the info card shows the
  /// counts. Mirrored on disconnect.
  knownChannels: new Map(),
  knownNodes: new Map(),

  /// Structured ring buffer of inbound events for the Debug tab. Each
  /// entry: { at: epoch_ms, level: 'info'|'warn'|'error', source, msg }.
  /// FIFO eviction past 500 entries so a long session doesn't leak.
  debugLog: [],
};

export const DEBUG_LOG_CAP = 500;

/// Per-node history of telemetry samples (battery, snr). Keyed by
/// node_num as a JS number. Each entry is a bounded array (cap 60)
/// of `{ at, battery, snr }`. Fed from the `node_info` event handler
/// and rendered as sparklines in the chat-tab node-detail panel.
state.nodeHistory = new Map();
export const NODE_HISTORY_CAP = 60;

/// Append a telemetry sample for `nodeNum`, evicting FIFO past the
/// cap. Skipped when neither battery nor snr has changed since the
/// last sample to keep the buffer trend-shaped instead of repeating
/// the same values from every NodeInfo broadcast.
export function pushNodeSample(nodeNum, battery, snr) {
  if (!state.nodeHistory.has(nodeNum)) state.nodeHistory.set(nodeNum, []);
  const buf = state.nodeHistory.get(nodeNum);
  const last = buf[buf.length - 1];
  if (last && last.battery === battery && Math.abs(last.snr - snr) < 0.01) return;
  buf.push({ at: Date.now(), battery, snr });
  if (buf.length > NODE_HISTORY_CAP) buf.splice(0, buf.length - NODE_HISTORY_CAP);
}

/// Clear device-specific state — call between `disconnect()` and the
/// next `connect()` so a fresh radio doesn't see stale node/channel
/// names from the previous session.
export function resetDeviceState() {
  state.myNodeNum = null;
  state.myNodeHex = null;
  state.fwVersion = null;
  state.transport = null;
  state.knownChannels.clear();
  state.knownNodes.clear();
}

/// Append a structured entry to `state.debugLog`, evicting FIFO past
/// the cap. Called from events.js for every inbound event and from
/// chat/settings for user actions worth surfacing.
export function pushDebug(source, msg, level = 'info') {
  state.debugLog.push({ at: Date.now(), level, source, msg });
  if (state.debugLog.length > DEBUG_LOG_CAP) {
    state.debugLog.splice(0, state.debugLog.length - DEBUG_LOG_CAP);
  }
}
