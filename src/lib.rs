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
pub(crate) struct Inner {
    /// The serial port handle. Kept alive so the connection persists, and
    /// closed explicitly by [`WebClient::disconnect`] for a graceful teardown.
    pub(crate) port: web_sys::SerialPort,
    pub(crate) writer: web_sys::WritableStreamDefaultWriter,
    /// Inbound stream reader. Held on `Inner` (rather than as a local in
    /// `read_loop`) so `disconnect()` can cancel it, which causes the loop's
    /// pending `read()` to resolve with `done: true` and exit cleanly.
    pub(crate) reader: web_sys::ReadableStreamDefaultReader,
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

    /// Encode a `ToRadio` payload, frame it, and write it to the port.
    pub(crate) async fn write_payload(
        &self,
        payload: voicetastic_core::proto::to_radio::PayloadVariant,
    ) -> Result<(), JsValue> {
        let msg = ToRadio {
            payload_variant: Some(payload),
        };
        let mut buf = Vec::with_capacity(msg.encoded_len());
        msg.encode(&mut buf).map_err(|e| err(&format!("encode: {e}")))?;
        let frame = frame_serial(&buf);
        let chunk = js_sys::Uint8Array::from(frame.as_slice());
        JsFuture::from(self.writer.write_with_chunk(chunk.as_ref())).await?;
        Ok(())
    }

    async fn send_text(&self, text: &str, channel: u32, to: Option<u32>) -> Result<(), JsValue> {
        let id = self.alloc_id();
        let payload = protocol::text_packet(id, text, channel, to)
            .map_err(|e| err(&format!("build text: {e}")))?;
        self.write_payload(payload).await?;
        log(&format!("sent text id={id}"));
        Ok(())
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
            inner.send_text(&text, channel, to).await?;
            Ok(JsValue::UNDEFINED)
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
    #[wasm_bindgen(js_name = disconnect)]
    pub fn disconnect(self) -> js_sys::Promise {
        future_to_promise(async move {
            let _ = JsFuture::from(self.inner.reader.cancel()).await;
            let _ = JsFuture::from(self.inner.writer.close()).await;
            let _ = JsFuture::from(self.inner.port.close()).await;
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
        port,
        writer,
        reader,
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
    });

    // Background inbound loop: read → deframe → core decode → core state/voice.
    // The reader itself lives on `Inner` so `disconnect()` can cancel it.
    let rx = inner.clone();
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = read_loop(rx, on_event, on_voice).await {
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

/// Read frames off the port forever, feeding each through the core decoder and
/// applying snapshot events to the shared `ProtocolState`. Exits with
/// `Ok(())` when the reader is cancelled (graceful disconnect) or with `Err`
/// on transport failure (e.g. the cable was unplugged).
async fn read_loop(
    inner: Rc<Inner>,
    on_event: js_sys::Function,
    on_voice: js_sys::Function,
) -> Result<(), JsValue> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let result = JsFuture::from(inner.reader.read()).await?;
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
                };
                protocol::decode_inbound(&payload, &ctx)
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
                                emit(&on_event, &build_event(&ev, &inner.state.borrow()));
                            }
                            // Voice frames go through core's reassembler; a
                            // completed message is decoded and handed to JS.
                            InboundEvent::Voice(vd) => {
                                handle_voice(&inner, vd, &on_event, &on_voice);
                            }
                            _ => emit(&on_event, &build_event(&ev, &inner.state.borrow())),
                        }
                    }
                }
                Err(e) => log(&format!("decode FromRadio failed: {e}")),
            }
        }
    }
}

