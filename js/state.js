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
};

/// Clear device-specific state — call between `disconnect()` and the
/// next `connect()` so a fresh radio doesn't see stale node/channel
/// names from the previous session.
export function resetDeviceState() {
  state.myNodeNum = null;
  state.myNodeHex = null;
  state.fwVersion = null;
  state.knownChannels.clear();
  state.knownNodes.clear();
}
