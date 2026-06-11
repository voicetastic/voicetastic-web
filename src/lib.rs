//! Browser driver for Voicetastic over Web Serial.
//!
//! This is the wasm sibling of `voicetastic-core`'s native `MeshtasticService`:
//! it drives the **same** sans-IO protocol core (`voicetastic_core::protocol`)
//! from the browser event loop. The radio bytes flow:
//!
//!   Web Serial read  → deframe (0x94 0xc3) → `protocol::decode_inbound`
//!                     → `ProtocolState::apply` (+ surface event to JS)
//!   `protocol::*_packet` builder → encode `ToRadio` → frame → Web Serial write
//!
//! No Meshtastic decode/build/state logic lives here — only the platform glue
//! (Web Serial, framing, and ferrying events to a JS callback). That's the
//! point of the sans-IO refactor: one protocol implementation, two drivers.

mod ble;
mod events;
mod serial;
mod settings;
mod util;
mod voice;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use prost::Message as _;
use voicetastic_core::codec::{amrnb_init, opus_init};
use voicetastic_core::proto::ToRadio;
use voicetastic_core::protocol::{self, InboundCtx, InboundEvent, ProtocolState};
use voicetastic_core::service::modem_preset_from_proto;
use voicetastic_core::voice::{AssemblerConfig, OutgoingVoiceRegistry, VoiceAssembler};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, future_to_promise};

use crate::events::{build_event, emit};
use crate::serial::{BAUD, frame_serial, next_frame};
use crate::util::{err, log, rand_u32};
use crate::voice::{handle_voice, nack_tick_loop};

/// Default Codec2 mode for outgoing voice. Mode 0 = 3200 bps, the highest-
/// quality Codec2 mode. Modes 0..=5 progress 3200→2400→1600→1400→1300→1200 bps;
/// lower bps saves airtime but sounds more robotic. Runtime-settable via
/// `WebClient.setCodec2Mode` — stored on `Inner.codec_param`.
const DEFAULT_CODEC2_MODE: u8 = 0;
/// Inter-frame pacing fallback before the radio's LoRa config is known.
pub(crate) const DEFAULT_PACING_MS: u64 = 250;

/// Shared per-connection state. `!Send` (holds JS handles), which is fine on
/// wasm's single thread. `pub(crate)` so sibling modules (voice, settings)
/// can carry their own `impl Inner` blocks.
/// Transport-specific handles. One per connection; the rest of `Inner`
/// is transport-agnostic (state, voice pipeline, codec selection,
/// telemetry). `Serial` covers Web Serial / USB; `Ble` covers Web
/// Bluetooth on Chromium-based browsers.
pub(crate) enum InnerTransport {
    Serial {
        /// Kept alive so the connection persists; closed explicitly
        /// by [`WebClient::disconnect`].
        port: web_sys::SerialPort,
        writer: web_sys::WritableStreamDefaultWriter,
        /// Held here (rather than as a local in `read_loop`) so
        /// `disconnect()` can cancel it, which causes the loop's
        /// pending `read()` to resolve with `done: true` and exit
        /// cleanly.
        reader: web_sys::ReadableStreamDefaultReader,
    },
    Ble {
        /// The `BluetoothDevice` reference is the stable handle across
        /// reconnects: characteristics are invalidated by every GATT
        /// drop, but the device reference keeps working as long as
        /// the page has permission. Calling `device.gatt().disconnect()`
        /// in `disconnect()` is what shuts the BLE link down.
        device: web_sys::BluetoothDevice,
        /// The three Meshtastic characteristics. Wrapped in `RefCell`
        /// because reconnect swaps the whole bundle for a freshly-
        /// discovered one (Web Bluetooth invalidates the JS handles
        /// after `gattserverdisconnected`). Every read site borrows,
        /// clones the JS handle out, and drops the borrow before the
        /// next `.await` — RefCell guards held across awaits cause
        /// panics when reconnect tries to borrow_mut.
        chars: std::cell::RefCell<ble::BleChars>,
        /// Set to `true` by user-initiated `disconnect()` / `forget()`
        /// before the GATT drop. Distinguishes a user stop (don't
        /// reconnect) from an unexpected drop (start the reconnect
        /// campaign). Also bails any in-flight drain on its next
        /// iteration.
        stopped: std::cell::Cell<bool>,
        /// Re-entrancy guard for the notification-driven drain.
        /// `fromNum` can fire while a drain is still in flight; the
        /// in-flight drain reads `fromRadio` until empty so any
        /// fresh data will land in that loop. The new notification
        /// just sets a "re-poll when done" flag.
        drain_active: std::cell::Cell<bool>,
        drain_pending: std::cell::Cell<bool>,
        /// `true` while a reconnect campaign is running. Prevents
        /// `gattserverdisconnected` from spawning concurrent
        /// campaigns (each new GATT attempt during the campaign can
        /// itself fire the event).
        reconnecting: std::cell::Cell<bool>,
        /// Held to keep the disconnect-event handler alive. Bound
        /// once to the `BluetoothDevice` and survives reconnects.
        _disconnect_listener: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
        /// Held to keep the `fromNum` notification handler alive.
        /// `None` when the firmware lacks `fromNum`. The closure
        /// itself is stable across reconnects; reconnect re-binds it
        /// to the new `fromNum` characteristic via
        /// `addEventListener`. We don't `removeEventListener` the
        /// old binding — the old characteristic is invalid post-
        /// disconnect and gets GC'd along with its listener record.
        _notify_listener: Option<wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>>,
    },
}

pub(crate) struct Inner {
    /// Active transport for this connection. Match on it for every
    /// per-transport hot path (write_payload, read loop, disconnect).
    pub(crate) transport: InnerTransport,
    /// The canonical protocol snapshot — core's `ProtocolState`, exactly as the
    /// native driver uses it.
    pub(crate) state: RefCell<ProtocolState>,
    /// Outbound packet-id counter (the runtime-owned bit the core leaves to the
    /// driver). Seeded from the RNG like the native service.
    pub(crate) next_id: Cell<u32>,
    /// RX-side voice reassembly — core's sans-IO `VoiceAssembler`.
    pub(crate) assembler: VoiceAssembler,
    /// TX-side retransmit registry — core's sync `OutgoingVoiceRegistry`.
    /// Tracks every message we've sent so we can service incoming NACKs by
    /// reshipping the exact missing chunks (with cooldown + dedup, all in core).
    pub(crate) registry: OutgoingVoiceRegistry,
    /// Latest firmware queue depth (from `QueueStatus`); gates voice TX so we
    /// don't overflow the radio. `u32::MAX` until the first report.
    pub(crate) queue_free: Cell<u32>,
    /// Run captured audio through core's RNNoise denoiser before encoding.
    /// On by default; runtime-toggleable via `WebClient::setDenoiseEnabled`.
    /// Requires 48 kHz input — skipped if the AudioContext is at another rate.
    pub(crate) denoise_enabled: Cell<bool>,
    /// Codec2 mode (0..=5) for outgoing voice. Runtime-settable via
    /// `WebClient::setCodec2Mode`.
    pub(crate) codec_param: Cell<u8>,
    /// Which codec to use for outgoing voice. Numeric so it crosses the
    /// wasm-bindgen boundary directly: 0 = Codec2 (default, LoRa-optimal),
    /// 1 = AMR-NB (telephony interop), 2 = Opus (best quality, higher airtime).
    pub(crate) send_codec: Cell<u8>,
    /// AMR-NB mode (0..=7) for outgoing voice when send_codec == AMR-NB.
    /// Default 5 = MR795 (7.95 kbps), matching the desktop GUI's default.
    pub(crate) amrnb_mode: Cell<u8>,
    /// Opus target bitrate in kbps for outgoing voice when send_codec == Opus.
    /// Default 12 kbps matches the desktop GUI's `OPUS_BITRATE` constant —
    /// good VoIP quality at modest airtime. Range 6..=128 per RFC 6716.
    pub(crate) opus_kbps: Cell<u8>,
    /// Sender-side FEC parity policy. Numeric so it crosses wasm-bindgen
    /// directly. 0 = Auto (the recommended default), 1 = Off, 2 = Light,
    /// 3 = Medium, 4 = Heavy. Mapped to [`VoiceFecMode`] per-message in
    /// `Inner::send_voice` via [`voice::fec_mode_from_u8`].
    pub(crate) fec_mode: Cell<u8>,
    /// Wall-clock of the most recent successfully-decoded inbound frame.
    /// Updated by `read_loop`; consulted by `nack_tick_loop` to drive the
    /// idle-probe. Mirrors the desktop service's silent-probe path: when
    /// a radio's RF state machine parks (e.g. after a long string of
    /// MaxRetransmit failures) the host write path is still alive so we
    /// can poke it with a `WantConfigId`. Seeded at connect with
    /// `Instant::now()` so the first probe waits the full quiet window.
    pub(crate) last_inbound_at: Cell<web_time::Instant>,
    /// Cached JS event sinks supplied by the JS-side `connect()` /
    /// `connectBle()` caller. The BLE notification handler needs to
    /// emit through these without re-threading them through every
    /// helper. `None` only before the first connect — populated for
    /// the lifetime of `Inner`.
    pub(crate) on_event: RefCell<Option<js_sys::Function>>,
    pub(crate) on_voice: RefCell<Option<js_sys::Function>>,
    /// Consecutive silent probes sent without any inbound reply since.
    /// Cleared each time `last_inbound_at` advances. Surfaced as a log
    /// line at the second consecutive probe so a stuck radio is visible.
    pub(crate) silent_probes: Cell<u32>,
}

impl Inner {
    /// Reserve the next non-zero packet id.
    pub(crate) fn alloc_id(&self) -> u32 {
        let mut id = self.next_id.get().wrapping_add(1);
        if id == 0 {
            id = 1;
        }
        self.next_id.set(id);
        id
    }

    /// Encode a `ToRadio` payload and ship it via the active
    /// transport. Serial wraps the bytes in the `0x94 0xc3 + length`
    /// frame; BLE writes the raw protobuf to the `toRadio`
    /// characteristic with no extra framing.
    pub(crate) async fn write_payload(
        &self,
        payload: voicetastic_core::proto::to_radio::PayloadVariant,
    ) -> Result<(), JsValue> {
        let msg = ToRadio {
            payload_variant: Some(payload),
        };
        let mut buf = Vec::with_capacity(msg.encoded_len());
        msg.encode(&mut buf).map_err(|e| err(&format!("encode: {e}")))?;
        // Pull the per-transport handle out of the borrow / RefCell
        // BEFORE awaiting — holding a borrow across the await would
        // race a concurrent reconnect that wants `borrow_mut`.
        enum WriteTarget {
            Serial(web_sys::WritableStreamDefaultWriter),
            Ble(web_sys::BluetoothRemoteGattCharacteristic),
        }
        let target = match &self.transport {
            InnerTransport::Serial { writer, .. } => WriteTarget::Serial(writer.clone()),
            InnerTransport::Ble { chars, .. } => WriteTarget::Ble(chars.borrow().to_radio.clone()),
        };
        match target {
            WriteTarget::Serial(writer) => {
                let frame = frame_serial(&buf);
                let chunk = js_sys::Uint8Array::from(frame.as_slice());
                JsFuture::from(writer.write_with_chunk(chunk.as_ref())).await?;
            }
            WriteTarget::Ble(to_radio) => {
                ble::write_to_radio(&to_radio, &buf).await?;
            }
        }
        Ok(())
    }

    async fn send_text(&self, text: &str, channel: u32, to: Option<u32>) -> Result<u32, JsValue> {
        let id = self.alloc_id();
        let payload = protocol::text_packet(id, text, channel, to)
            .map_err(|e| err(&format!("build text: {e}")))?;
        self.write_payload(payload).await?;
        log(&format!("sent text id={id}"));
        Ok(id)
    }

    /// Build and write an admin message (config write, fixed-position, etc.)
    /// addressed to our own node. Equivalent to `MeshtasticService::send_admin`
    /// on native; routes through core's `protocol::admin_packet` builder.
    pub(crate) async fn send_admin(
        &self,
        payload: voicetastic_core::proto::admin_message::PayloadVariant,
    ) -> Result<(), JsValue> {
        let to = self
            .state
            .borrow()
            .my_info
            .as_ref()
            .map(|i| i.my_node_num)
            .ok_or_else(|| err("not connected — own node number unknown"))?;
        let id = self.alloc_id();
        let pv = protocol::admin_packet(id, to, payload)
            .map_err(|e| err(&format!("build admin: {e}")))?;
        self.write_payload(pv).await?;
        log(&format!("sent admin id={id}"));
        Ok(())
    }

    /// Inter-frame pacing from the radio's LoRa modem preset (core's policy);
    /// falls back to a safe default before the config burst lands.
    pub(crate) fn pacing(&self) -> std::time::Duration {
        let preset = self
            .state
            .borrow()
            .lora
            .as_ref()
            .and_then(|l| modem_preset_from_proto(l.modem_preset));
        match preset {
            Some(p) => p.pacing(),
            None => std::time::Duration::from_millis(DEFAULT_PACING_MS),
        }
    }

}

/// Handle to a connected radio. Returned by [`connect`]; lives as long as JS
/// holds it. The inbound read loop runs in the background via `spawn_local`.
#[wasm_bindgen]
pub struct WebClient {
    pub(crate) inner: Rc<Inner>,
}

#[wasm_bindgen]
impl WebClient {
    /// Send a text message. `to` undefined = broadcast. Returns a Promise.
    #[wasm_bindgen(js_name = sendText)]
    pub fn send_text(&self, text: String, channel: u32, to: Option<u32>) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let id = inner.send_text(&text, channel, to).await?;
            // Resolve with the mesh packet id so JS can correlate the
            // outgoing message with the eventual `ack_or_nak` event and
            // flip its delivery-status icon.
            Ok(JsValue::from(id))
        })
    }

    /// Re-request the full config burst.
    #[wasm_bindgen(js_name = requestConfig)]
    pub fn request_config(&self) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            inner.write_payload(protocol::want_config(rand_u32())).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Active node-discovery ping: broadcast our `User` on `NODEINFO_APP` with
    /// `want_response = true` so peers reply with their own NodeInfo. Replies
    /// arrive over the next several seconds as the normal `NodeInfo` events,
    /// updating `ProtocolState.nodes`. Rejects with an error if the radio
    /// hasn't yet reported our owner (call after `ConfigComplete`).
    #[wasm_bindgen(js_name = discoverNodes)]
    pub fn discover_nodes(&self) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let owner = inner
                .state
                .borrow()
                .owner
                .clone()
                .ok_or_else(|| err("owner not yet known — wait for ConfigComplete"))?;
            let id = inner.alloc_id();
            let pv = protocol::nodeinfo_request_packet(id, &owner, 0)
                .map_err(|e| err(&format!("build discovery: {e}")))?;
            inner.write_payload(pv).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Reboot the connected radio after `secs` seconds. Wraps
    /// `AdminMessage::RebootSeconds` (mirrors core's `reboot()`).
    #[wasm_bindgen(js_name = reboot)]
    pub fn reboot(&self, secs: i32) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            use voicetastic_core::proto::admin_message::PayloadVariant;
            inner.send_admin(PayloadVariant::RebootSeconds(secs)).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Factory-reset the connected radio's config (mirrors core's
    /// `factory_reset()`). Wipes owner, channels, and module configs.
    #[wasm_bindgen(js_name = factoryReset)]
    pub fn factory_reset(&self) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            use voicetastic_core::proto::admin_message::PayloadVariant;
            inner.send_admin(PayloadVariant::FactoryResetConfig(1)).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Wipe the connected radio's NodeDB (every learned NodeInfo), drop
    /// our cached peer map in lockstep, and re-request the config burst so
    /// the local snapshot reflects the wipe. Mirrors core's
    /// `MeshtasticService::reset_nodedb_and_refresh`. The firmware never
    /// re-bursts NodeInfo for an empty NodeDB, so clearing the local cache
    /// here is what actually makes the UI forget the stale peers; JS-side
    /// mirrors (`state.knownNodes`) still need to be cleared by the caller.
    #[wasm_bindgen(js_name = resetNodedb)]
    pub fn reset_nodedb(&self) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            use voicetastic_core::proto::admin_message::PayloadVariant;
            inner.send_admin(PayloadVariant::NodedbReset(true)).await?;
            // `ProtocolState.nodes` is a public field; clearing it inline
            // works regardless of the pinned core revision. Switch to
            // `clear_nodes()` once the git dep is bumped past the helper.
            inner.state.borrow_mut().nodes.clear();
            inner.write_payload(protocol::want_config(rand_u32())).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Graceful teardown: cancel the inbound stream, close the writer, and
    /// close the port. Consumes the `WebClient` — wasm-bindgen marks the JS
    /// proxy as freed, so any subsequent method call from JS will throw.
    ///
    /// The background read loop sees the cancelled reader, exits with
    /// `Ok(())`, and drops its `Rc<Inner>`. Once this method's future also
    /// drops `self`, the only remaining `Rc<Inner>` is the NACK tick loop's
    /// own clone, and its `Rc::strong_count <= 1` check terminates it
    /// within the next tick (~500 ms).
    ///
    /// Each step's error is swallowed: a half-broken connection still needs
    /// to make as much progress towards closure as possible.
    /// Revoke the page's permission to talk to the currently-connected
    /// BLE device — equivalent to "forget this device" in Chromium's
    /// per-site Bluetooth settings. Closes the GATT link first and
    /// then drops the persisted permission so a subsequent
    /// `connect_ble()` shows the device picker again instead of
    /// auto-resuming. No-op on the Serial transport.
    ///
    /// Note: this does NOT unpair the device at the OS level — Web
    /// Bluetooth deliberately can't reach BlueZ's bond database. Use
    /// the system Bluetooth settings (or `bluetoothctl remove`) for
    /// full unpairing.
    #[wasm_bindgen(js_name = forget)]
    pub fn forget(self) -> js_sys::Promise {
        future_to_promise(async move {
            if let InnerTransport::Ble { device, stopped, .. } = &self.inner.transport {
                stopped.set(true);
                if let Some(gatt) = device.gatt() {
                    gatt.disconnect();
                }
                // `BluetoothDevice.forget()` isn't bound in web-sys 0.3
                // yet, so reach it via Reflect. Available in Chromium
                // 116+; older browsers silently no-op (the .get returns
                // undefined which dyn_into fails on).
                if let Ok(method) =
                    js_sys::Reflect::get(device, &JsValue::from_str("forget"))
                    && let Ok(method) = method.dyn_into::<js_sys::Function>()
                    && let Ok(promise) = method.call0(device.as_ref())
                    && let Ok(promise) = promise.dyn_into::<js_sys::Promise>()
                {
                    let _ = JsFuture::from(promise).await;
                    log("ble: device permission forgotten");
                } else {
                    log("ble: device.forget() unavailable (Chromium 116+ required)");
                }
            }
            Ok(JsValue::UNDEFINED)
        })
    }

    #[wasm_bindgen(js_name = disconnect)]
    pub fn disconnect(self) -> js_sys::Promise {
        future_to_promise(async move {
            match &self.inner.transport {
                InnerTransport::Serial { port, writer, reader } => {
                    let _ = JsFuture::from(reader.cancel()).await;
                    let _ = JsFuture::from(writer.close()).await;
                    let _ = JsFuture::from(port.close()).await;
                }
                InnerTransport::Ble {
                    device,
                    chars,
                    stopped,
                    _notify_listener,
                    _disconnect_listener,
                    ..
                } => {
                    // Set the user-stop flag FIRST so the
                    // gattserverdisconnected listener (which fires
                    // synchronously inside `gatt.disconnect()` below)
                    // distinguishes "user wanted this" from "link
                    // dropped" and does not spawn a reconnect.
                    stopped.set(true);
                    let from_num = chars.borrow().from_num.clone();
                    // Stop the BLE notify subscription before tearing
                    // down the listener. Best-effort: a failed
                    // `stopNotifications` doesn't block disconnect.
                    if let Some(fr) = from_num.as_ref() {
                        let _ = JsFuture::from(fr.stop_notifications()).await;
                        let _ = fr.remove_event_listener_with_callback(
                            "characteristicvaluechanged",
                            _notify_listener
                                .as_ref()
                                .map(|c| c.as_ref().unchecked_ref())
                                .unwrap_or(&js_sys::Function::new_no_args("")),
                        );
                    }
                    let _ = device.remove_event_listener_with_callback(
                        "gattserverdisconnected",
                        _disconnect_listener.as_ref().unchecked_ref(),
                    );
                    // GATT disconnect drops the link itself.
                    if let Some(gatt) = device.gatt() {
                        gatt.disconnect();
                    }
                }
            }
            Ok(JsValue::UNDEFINED)
        })
    }

    // Settings surface (`snapshot`, `writeOwner`, the eight `writeConfig*`s,
    // `writeChannel`, `setFixedPosition`) lives in src/settings.rs as its
    // own `impl WebClient` block. See the `write_config!` macro there for
    // the per-section boilerplate.
}

/// Connect to a user-selected Meshtastic radio over Web Serial and start
/// driving `voicetastic_core`'s protocol core. `on_event` is invoked with a
/// short string for every decoded inbound event. Resolves once connected (the
/// read loop continues in the background).
///
/// Must be called from a user gesture (the Web Serial port picker requires it).
#[wasm_bindgen]
pub async fn connect(
    on_event: js_sys::Function,
    on_voice: js_sys::Function,
) -> Result<WebClient, JsValue> {
    let window = web_sys::window().ok_or_else(|| err("no window"))?;
    let serial = window.navigator().serial();

    let port: web_sys::SerialPort = JsFuture::from(serial.request_port()).await?.dyn_into()?;
    JsFuture::from(port.open(&web_sys::SerialOptions::new(BAUD))).await?;
    log(&format!("serial: port open @{BAUD}"));

    let writer = port
        .writable()
        .get_writer()
        .map_err(|e| err(&format!("get_writer: {e:?}")))?;
    let reader: web_sys::ReadableStreamDefaultReader =
        port.readable().get_reader().dyn_into()?;

    let inner = Rc::new(Inner {
        transport: InnerTransport::Serial { port, writer, reader },
        state: RefCell::new(ProtocolState::default()),
        next_id: Cell::new(rand_u32()),
        assembler: VoiceAssembler::new(AssemblerConfig::default()),
        registry: OutgoingVoiceRegistry::default(),
        queue_free: Cell::new(u32::MAX),
        denoise_enabled: Cell::new(true),
        codec_param: Cell::new(DEFAULT_CODEC2_MODE),
        send_codec: Cell::new(0), // 0 = Codec2
        amrnb_mode: Cell::new(5), // MR795 — same default as desktop GUI
        opus_kbps: Cell::new(12), // 12 kbps — same default as desktop GUI
        fec_mode: Cell::new(0), // 0 = Auto — recommended default
        last_inbound_at: Cell::new(web_time::Instant::now()),
        on_event: RefCell::new(Some(on_event.clone())),
        on_voice: RefCell::new(Some(on_voice.clone())),
        silent_probes: Cell::new(0),
    });

    // Background inbound loop: read → deframe → core decode → core state/voice.
    // The reader itself lives on `Inner` so `disconnect()` can cancel it.
    let rx = inner.clone();
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = serial_read_loop(rx, on_event, on_voice).await {
            log(&format!("serial read loop ended: {e:?}"));
        }
    });
    // Background NACK loop: drive VoiceAssembler::tick() periodically and
    // forward the framed NACKs to senders, matching the desktop's RX-side
    // reliability behaviour.
    let nack_inner = inner.clone();
    wasm_bindgen_futures::spawn_local(async move {
        nack_tick_loop(nack_inner).await;
    });

    // Hand the vendored codec wasms to their JS shims so the first voice
    // operation doesn't pay the WebAssembly.instantiate latency. Errors are
    // logged but non-fatal — Codec2 paths still work without either codec.
    wasm_bindgen_futures::spawn_local(async {
        if let Err(e) = amrnb_init().await {
            log(&format!("amrnb shim init failed: {e:?}"));
        }
    });
    wasm_bindgen_futures::spawn_local(async {
        if let Err(e) = opus_init().await {
            log(&format!("opus shim init failed: {e:?}"));
        }
    });

    // Kick off the config handshake using the core builder.
    let nonce = rand_u32();
    inner.write_payload(protocol::want_config(nonce)).await?;
    log(&format!("serial: sent WantConfigId nonce={nonce}"));

    Ok(WebClient { inner })
}

/// Connect to a user-selected Meshtastic radio over Web Bluetooth and
/// start driving `voicetastic_core`'s protocol core. Mirrors
/// [`connect`] but uses GATT instead of Web Serial.
///
/// Must be called from a user gesture (the BLE picker requires it).
/// Available only on Chromium-based browsers — Firefox + Safari fall
/// back to the Web Serial path.
#[wasm_bindgen(js_name = connectBle)]
pub async fn connect_ble(
    on_event: js_sys::Function,
    on_voice: js_sys::Function,
) -> Result<WebClient, JsValue> {
    let handles = ble::open().await?;
    log(&format!(
        "ble: connected to '{}' ({})",
        handles.device.name().unwrap_or_else(|| "<unknown>".into()),
        handles.device.id(),
    ));

    // Build Inner first so the closures below can capture a Weak<Inner>
    // (the GATT-disconnect + fromNum notification listeners need to call
    // back into Inner). `Rc::new_cyclic` lets us hand the closures a
    // `Weak<Inner>` before `Inner` is fully constructed — they upgrade
    // it on each fire and no-op if Inner has already been dropped.
    let inner = Rc::new_cyclic(|weak_inner: &std::rc::Weak<Inner>| {
        // gattserverdisconnected — fires when the BLE link drops for
        // any reason (radio powered off, out of range, OS unpair, …).
        // Distinguishes user-initiated stops (`stopped` already set
        // by `disconnect()` / `forget()`) from unexpected drops; the
        // latter kicks off the reconnect campaign via the sans-IO
        // policy in core.
        let weak_d = weak_inner.clone();
        let disconnect_listener = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::Event)>::new(
            move |_ev: web_sys::Event| {
                let Some(inner) = weak_d.upgrade() else { return };
                let InnerTransport::Ble { stopped, .. } = &inner.transport else { return };
                if stopped.get() {
                    log("ble: gattserverdisconnected (user stop)");
                } else {
                    log("ble: gattserverdisconnected — radio link dropped, reconnecting…");
                    let inner = inner.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        ble_reconnect_loop(inner).await;
                    });
                }
            },
        );

        // characteristicvaluechanged on `fromNum` — fires each time the
        // firmware has new `fromRadio` data. `weak_n.upgrade()` is the
        // standard pattern for closures that outlive their owner.
        // Only registered when `from_num` is present.
        let notify_listener = handles.from_num.as_ref().map(|_| {
            let weak_n = weak_inner.clone();
            wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::Event)>::new(
                move |_ev: web_sys::Event| {
                    if let Some(inner) = weak_n.upgrade() {
                        schedule_ble_drain(&inner);
                    }
                },
            )
        });

        Inner {
            transport: InnerTransport::Ble {
                device: handles.device,
                chars: std::cell::RefCell::new(ble::BleChars {
                    from_radio: handles.from_radio,
                    to_radio: handles.to_radio,
                    from_num: handles.from_num,
                }),
                stopped: std::cell::Cell::new(false),
                drain_active: std::cell::Cell::new(false),
                drain_pending: std::cell::Cell::new(false),
                reconnecting: std::cell::Cell::new(false),
                _disconnect_listener: disconnect_listener,
                _notify_listener: notify_listener,
            },
            state: RefCell::new(ProtocolState::default()),
            next_id: Cell::new(rand_u32()),
            assembler: VoiceAssembler::new(AssemblerConfig::default()),
            registry: OutgoingVoiceRegistry::default(),
            queue_free: Cell::new(u32::MAX),
            denoise_enabled: Cell::new(true),
            codec_param: Cell::new(DEFAULT_CODEC2_MODE),
            send_codec: Cell::new(0),
            amrnb_mode: Cell::new(5),
            opus_kbps: Cell::new(12),
            fec_mode: Cell::new(0),
            last_inbound_at: Cell::new(web_time::Instant::now()),
            on_event: RefCell::new(Some(on_event.clone())),
            on_voice: RefCell::new(Some(on_voice.clone())),
            silent_probes: Cell::new(0),
        }
    });

    // Register the listeners now that the closures live inside Inner
    // (they're held there to keep them alive for the connection's
    // lifetime). `start_notifications()` on `fromNum` arms the BLE
    // notify subscription.
    if let InnerTransport::Ble {
        device,
        chars,
        _disconnect_listener,
        _notify_listener,
        ..
    } = &inner.transport
    {
        device.add_event_listener_with_callback(
            "gattserverdisconnected",
            _disconnect_listener.as_ref().unchecked_ref(),
        )?;
        // Pull `from_num` out of the RefCell + drop the borrow before
        // awaiting `start_notifications`. The reconnect path mirrors
        // this same pattern.
        let from_num = chars.borrow().from_num.clone();
        if let (Some(from_num), Some(listener)) = (from_num.as_ref(), _notify_listener.as_ref()) {
            from_num.add_event_listener_with_callback(
                "characteristicvaluechanged",
                listener.as_ref().unchecked_ref(),
            )?;
            JsFuture::from(from_num.start_notifications()).await?;
        }
    }

    // Pick the right read driver. With `fromNum` notifications armed we
    // rely entirely on the event listener + drain; the polling fallback
    // covers firmware that doesn't expose `fromNum`. Either way an
    // initial drain catches any data already queued before the listener
    // was attached.
    let drain_init = inner.clone();
    wasm_bindgen_futures::spawn_local(async move {
        schedule_ble_drain(&drain_init);
    });
    let has_from_num = matches!(
        &inner.transport,
        InnerTransport::Ble { chars, .. } if chars.borrow().from_num.is_some(),
    );
    if !has_from_num {
        let poll_rx = inner.clone();
        let poll_event = on_event.clone();
        let poll_voice = on_voice.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = ble_read_loop(poll_rx, poll_event, poll_voice).await {
                log(&format!("ble polling read loop ended: {e:?}"));
            }
        });
    }
    let nack_inner = inner.clone();
    wasm_bindgen_futures::spawn_local(async move {
        nack_tick_loop(nack_inner).await;
    });

    wasm_bindgen_futures::spawn_local(async {
        if let Err(e) = amrnb_init().await {
            log(&format!("amrnb shim init failed: {e:?}"));
        }
    });
    wasm_bindgen_futures::spawn_local(async {
        if let Err(e) = opus_init().await {
            log(&format!("opus shim init failed: {e:?}"));
        }
    });

    let nonce = rand_u32();
    inner.write_payload(protocol::want_config(nonce)).await?;
    log(&format!("ble: sent WantConfigId nonce={nonce}"));

    Ok(WebClient { inner })
}

/// Drain `fromRadio` until the firmware reports empty, processing each
/// frame through [`process_payload`]. Spawned on every `fromNum`
/// notification (and once at connect time to catch anything queued
/// before the listener attached).
///
/// `drain_active` / `drain_pending` form a tiny re-entrancy guard:
/// a notification that arrives while a drain is in flight just sets
/// `drain_pending`, and the in-flight loop re-checks after the radio
/// reports empty. That way we never spawn two concurrent reads on the
/// same characteristic (BLE GATT doesn't define concurrent reads).
pub(crate) fn schedule_ble_drain(inner: &Rc<Inner>) {
    let (active, pending) = match &inner.transport {
        InnerTransport::Ble {
            drain_active,
            drain_pending,
            stopped,
            ..
        } => {
            if stopped.get() {
                return;
            }
            if drain_active.get() {
                drain_pending.set(true);
                return;
            }
            drain_active.set(true);
            (drain_active, drain_pending)
        }
        _ => return,
    };
    // We can't borrow `active` / `pending` across `await` (Cell is
    // !Send and the borrow is to fields of Inner). Resolve the
    // references inside the spawned task instead.
    let _ = (active, pending);
    let inner = inner.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let on_event = inner.on_event.borrow().clone();
        let on_voice = inner.on_voice.borrow().clone();
        let (Some(on_event), Some(on_voice)) = (on_event, on_voice) else {
            return;
        };
        // The `loop { match … }` body has three early-exit arms, none
        // of which fit a `while let` shape cleanly; opt out of the
        // lint locally rather than contort the control flow.
        #[allow(clippy::while_let_loop)]
        loop {
            let from_radio = match &inner.transport {
                InnerTransport::Ble {
                    chars,
                    stopped,
                    ..
                } => {
                    if stopped.get() {
                        break;
                    }
                    // Borrow + clone the handle out and drop the borrow
                    // before awaiting `read_value` — reconnect's
                    // `borrow_mut` would clash otherwise.
                    chars.borrow().from_radio.clone()
                }
                _ => break,
            };
            match ble::read_from_radio(&from_radio).await {
                Ok(Some(bytes)) => process_payload(&inner, &bytes, &on_event, &on_voice),
                Ok(None) => break,
                Err(e) => {
                    log(&format!("ble drain: {e:?}"));
                    break;
                }
            }
        }
        // Drain done. If a notification fired while we were running,
        // honour it now by re-entering with a fresh drain.
        if let InnerTransport::Ble {
            drain_active,
            drain_pending,
            ..
        } = &inner.transport
        {
            drain_active.set(false);
            if drain_pending.replace(false) {
                schedule_ble_drain(&inner);
            }
        }
    });
}

/// Drive the reconnect campaign after a `gattserverdisconnected`
/// event. Uses core's sans-IO [`BleReconnectPolicy`] for the backoff
/// curve; sleeps the policy's delay, attempts service re-discovery on
/// the cached `BluetoothDevice`, swaps the fresh characteristics into
/// `InnerTransport::Ble.chars`, re-arms the `fromNum` notify, and
/// resends `WantConfigId` so the firmware re-bursts our state. On
/// failure the policy's attempt counter bumps and we sleep again.
///
/// Bails immediately on user-initiated disconnect (`stopped` set by
/// [`WebClient::disconnect`] / [`WebClient::forget`]). `reconnecting`
/// guards against concurrent campaigns when each retry's own
/// `gatt.connect()` failure fires another `gattserverdisconnected`.
async fn ble_reconnect_loop(inner: Rc<Inner>) {
    use voicetastic_core::meshtastic::reconnect::BleReconnectPolicy;
    use crate::util::sleep_ms;

    // Concurrency guard. `replace(true)` returns the OLD value: if it
    // was already true, another campaign is in flight and we bail.
    if let InnerTransport::Ble { reconnecting, .. } = &inner.transport {
        if reconnecting.replace(true) {
            return;
        }
    } else {
        return;
    }

    let mut policy = BleReconnectPolicy::default();
    loop {
        // Re-check the user-stop flag every iteration: a `disconnect()`
        // mid-campaign should land before the next attempt.
        let stopped = match &inner.transport {
            InnerTransport::Ble { stopped, .. } => stopped.get(),
            _ => true,
        };
        if stopped || policy.should_give_up() {
            break;
        }

        let delay = policy.next_delay();
        let attempt = policy.attempts() + 1;
        log(&format!(
            "ble reconnect: waiting {} ms before attempt {}",
            delay.as_millis(),
            attempt,
        ));
        sleep_ms(delay.as_millis() as i32).await;

        // Re-check stop after the sleep — user may have clicked
        // Disconnect during the backoff window.
        if let InnerTransport::Ble { stopped, .. } = &inner.transport
            && stopped.get()
        {
            break;
        }

        let device = match &inner.transport {
            InnerTransport::Ble { device, .. } => device.clone(),
            _ => break,
        };
        match ble::discover_chars(&device).await {
            Ok(new_chars) => {
                // Swap the fresh handles in, then re-arm the notify
                // subscription against the new `from_num`. The notify
                // closure itself is stable (held on Inner); we just
                // re-bind it via `addEventListener`.
                let from_num = new_chars.from_num.clone();
                if let InnerTransport::Ble { chars, _notify_listener, .. } =
                    &inner.transport
                {
                    *chars.borrow_mut() = new_chars;
                    if let (Some(fn_char), Some(listener)) =
                        (from_num.as_ref(), _notify_listener.as_ref())
                    {
                        let _ = fn_char.add_event_listener_with_callback(
                            "characteristicvaluechanged",
                            listener.as_ref().unchecked_ref(),
                        );
                        let _ = JsFuture::from(fn_char.start_notifications()).await;
                    }
                }
                policy.reset();
                log(&format!(
                    "ble reconnect: link re-established on attempt {attempt}"
                ));
                // Re-issue WantConfigId so the firmware bursts state
                // we may have missed during the outage; also kick a
                // drain in case data is pending.
                let nonce = rand_u32();
                if let Err(e) = inner.write_payload(protocol::want_config(nonce)).await {
                    log(&format!("ble reconnect: re-handshake failed: {e:?}"));
                }
                schedule_ble_drain(&inner);
                break;
            }
            Err(e) => {
                log(&format!("ble reconnect attempt {attempt} failed: {e:?}"));
                policy.record_failure();
            }
        }
    }

    if let InnerTransport::Ble { reconnecting, stopped, .. } = &inner.transport {
        reconnecting.set(false);
        if policy.should_give_up() && !stopped.get() {
            log(&format!(
                "ble reconnect: gave up after {} attempts",
                policy.attempts()
            ));
        }
    }
}

/// Process one decoded `FromRadio` payload — common to both the
/// Web Serial deframed-stream loop and the Web Bluetooth one-protobuf-
/// per-read loop. Extracted to keep the two read loops focussed on
/// their transport quirks (framing vs polling).
fn process_payload(
    inner: &Rc<Inner>,
    payload: &[u8],
    on_event: &js_sys::Function,
    on_voice: &js_sys::Function,
) {
    // Any well-formed frame (snapshot, voice, data, queue status,
    // routing ack) is proof the radio's host pipe is still alive.
    // Reset the silence timer here so the idle-probe in
    // `nack_tick_loop` only fires when the radio actually goes quiet,
    // not just when there's no voice traffic.
    inner.last_inbound_at.set(web_time::Instant::now());
    inner.silent_probes.set(0);
    // Hold the immutable borrow for the duration of decode — the
    // ctx carries `&state.nodes`. Drop it before the apply loop
    // below mutably borrows. `our_private_key` is intentionally
    // `None`: PKC DM decrypt isn't wired in the browser yet, so
    // PKC-encrypted packets that bypassed firmware decrypt remain
    // unreadable here (same behaviour as before the PKC work).
    let events = {
        let state = inner.state.borrow();
        let ctx = InboundCtx {
            my_node_num: state.my_info.as_ref().map(|i| i.my_node_num),
            our_private_key: None,
            nodes: &state.nodes,
            // PKC DM decrypt isn't wired in the browser (private key is
            // `None`), so there's no rescued-DM stream to replay-dedup.
            pkc_seen: None,
        };
        protocol::decode_inbound(payload, &ctx)
    };
    match events {
        Ok(events) => {
            for ev in events {
                if ev.is_snapshot() {
                    inner.state.borrow_mut().apply(&ev);
                }
                match &ev {
                    // Track queue depth for voice TX backpressure; still
                    // forward the structured event so the JS log shows it.
                    InboundEvent::QueueStatus(qs) => {
                        inner.queue_free.set(qs.free);
                        emit(on_event, &build_event(&ev, &inner.state.borrow()));
                    }
                    // Voice frames go through core's reassembler; a
                    // completed message is decoded and handed to JS.
                    InboundEvent::Voice(vd) => {
                        handle_voice(inner, vd, on_event, on_voice);
                    }
                    _ => emit(on_event, &build_event(&ev, &inner.state.borrow())),
                }
            }
        }
        Err(e) => log(&format!("decode FromRadio failed: {e}")),
    }
}

/// Read frames off the port forever, feeding each through the core
/// decoder. Exits with `Ok(())` when the reader is cancelled
/// (graceful disconnect) or with `Err` on transport failure (e.g.
/// the cable was unplugged).
async fn serial_read_loop(
    inner: Rc<Inner>,
    on_event: js_sys::Function,
    on_voice: js_sys::Function,
) -> Result<(), JsValue> {
    let reader = match &inner.transport {
        InnerTransport::Serial { reader, .. } => reader.clone(),
        InnerTransport::Ble { .. } => unreachable!("serial_read_loop called on BLE transport"),
    };
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let result = JsFuture::from(reader.read()).await?;
        let done = js_sys::Reflect::get(&result, &JsValue::from_str("done"))?
            .as_bool()
            .unwrap_or(false);
        if done {
            return Ok(());
        }
        let value = js_sys::Reflect::get(&result, &JsValue::from_str("value"))?;
        let arr = js_sys::Uint8Array::new(&value);
        let mut chunk = vec![0u8; arr.length() as usize];
        arr.copy_to(&mut chunk);
        buf.extend_from_slice(&chunk);

        while let Some((payload, consumed)) = next_frame(&buf) {
            buf.drain(..consumed);
            if payload.is_empty() {
                continue; // resync marker
            }
            process_payload(&inner, &payload, &on_event, &on_voice);
        }
    }
}

/// Poll the BLE `fromRadio` characteristic forever, feeding each
/// already-deframed protobuf through the core decoder. The polling
/// cadence is set per-state: tight (50 ms) right after a write so we
/// pick up the response, otherwise relaxed (250 ms) to avoid burning
/// the radio's BLE controller. Exits when `transport.stopped` is set
/// by `disconnect()`.
async fn ble_read_loop(
    inner: Rc<Inner>,
    on_event: js_sys::Function,
    on_voice: js_sys::Function,
) -> Result<(), JsValue> {
    use crate::util::sleep_ms;
    loop {
        let (from_radio, stopped) = match &inner.transport {
            InnerTransport::Ble { chars, stopped, .. } => {
                (chars.borrow().from_radio.clone(), stopped.get())
            }
            InnerTransport::Serial { .. } => {
                unreachable!("ble_read_loop called on serial transport")
            }
        };
        if stopped {
            return Ok(());
        }
        match ble::read_from_radio(&from_radio).await {
            Ok(Some(bytes)) => {
                process_payload(&inner, &bytes, &on_event, &on_voice);
                // Burst: keep draining while there's data, only
                // sleeping once the radio reports empty.
                continue;
            }
            Ok(None) => {
                sleep_ms(250).await;
            }
            Err(e) => {
                log(&format!("ble read error: {e:?}"));
                return Err(e);
            }
        }
    }
}

