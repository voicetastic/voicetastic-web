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

mod amrnb;
mod settings;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use prost::Message as _;
use voicetastic_core::codec::{Denoiser, codec2_decode, codec2_encode, opus_decode};
use voicetastic_core::node::NodeId;
use voicetastic_core::ports::PRIVATE_APP;
use voicetastic_core::proto::ToRadio;
use voicetastic_core::protocol::{self, InboundCtx, InboundEvent, ProtocolState};
use voicetastic_core::service::modem_preset_from_proto;
use voicetastic_core::settings::api::VoiceFecMode;
use voicetastic_core::voice::{
    AssemblerConfig, AssemblyEvent, BuildConfig, MAX_BODY_SIZE, ModemPreset, OutgoingVoiceRegistry,
    VoiceAssembler, VoiceCodec, build_message, random_message_id, tx_policy,
};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, future_to_promise};

/// Default Codec2 mode for outgoing voice. Mode 0 = 3200 bps, the highest-
/// quality Codec2 mode. Modes 0..=5 progress 3200→2400→1600→1400→1300→1200 bps;
/// lower bps saves airtime but sounds more robotic. Runtime-settable via
/// `WebClient.setCodec2Mode` — stored on `Inner.codec_param`.
const DEFAULT_CODEC2_MODE: u8 = 0;
/// Inter-frame pacing fallback before the radio's LoRa config is known.
const DEFAULT_PACING_MS: u64 = 250;

/// Magic bytes that begin every serial-framed packet (see core's serial.rs).
const START1: u8 = 0x94;
const START2: u8 = 0xc3;
/// Maximum protobuf payload length accepted by the device.
const MAX_PAYLOAD: usize = 512;
/// Default baud for Meshtastic USB-serial.
const BAUD: u32 = 115_200;

fn log(s: &str) {
    web_sys::console::log_1(&JsValue::from_str(s));
}

fn err(s: &str) -> JsValue {
    JsValue::from_str(s)
}

fn rand_u32() -> u32 {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    u32::from_le_bytes(b)
}

/// Shared per-connection state. `!Send` (holds JS handles), which is fine on
/// wasm's single thread.
struct Inner {
    /// Kept alive so the serial connection isn't dropped.
    _port: web_sys::SerialPort,
    writer: web_sys::WritableStreamDefaultWriter,
    /// The canonical protocol snapshot — core's `ProtocolState`, exactly as the
    /// native driver uses it.
    state: RefCell<ProtocolState>,
    /// Outbound packet-id counter (the runtime-owned bit the core leaves to the
    /// driver). Seeded from the RNG like the native service.
    next_id: Cell<u32>,
    /// RX-side voice reassembly — core's sans-IO `VoiceAssembler`.
    assembler: VoiceAssembler,
    /// TX-side retransmit registry — core's sync `OutgoingVoiceRegistry`.
    /// Tracks every message we've sent so we can service incoming NACKs by
    /// reshipping the exact missing chunks (with cooldown + dedup, all in core).
    registry: OutgoingVoiceRegistry,
    /// Latest firmware queue depth (from `QueueStatus`); gates voice TX so we
    /// don't overflow the radio. `u32::MAX` until the first report.
    queue_free: Cell<u32>,
    /// Run captured audio through core's RNNoise denoiser before encoding.
    /// On by default; runtime-toggleable via `WebClient::setDenoiseEnabled`.
    /// Requires 48 kHz input — skipped if the AudioContext is at another rate.
    denoise_enabled: Cell<bool>,
    /// Codec2 mode (0..=5) for outgoing voice. Runtime-settable via
    /// `WebClient::setCodec2Mode`.
    codec_param: Cell<u8>,
    /// Which codec to use for outgoing voice. Numeric so it crosses the
    /// wasm-bindgen boundary directly: 0 = Codec2 (default), 1 = AMR-NB.
    /// (Opus encode isn't available in the browser, see UI for details.)
    send_codec: Cell<u8>,
    /// AMR-NB mode (0..=7) for outgoing voice when send_codec == AMR-NB.
    /// Default 5 = MR795 (7.95 kbps), matching the desktop GUI's default.
    amrnb_mode: Cell<u8>,
}

impl Inner {
    /// Reserve the next non-zero packet id.
    fn alloc_id(&self) -> u32 {
        let mut id = self.next_id.get().wrapping_add(1);
        if id == 0 {
            id = 1;
        }
        self.next_id.set(id);
        id
    }

    /// Encode a `ToRadio` payload, frame it, and write it to the port.
    async fn write_payload(
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
    async fn send_admin(
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
    fn pacing(&self) -> std::time::Duration {
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

    /// Encode + frame + paced-send a voice clip. Mirrors the native voice TX
    /// worker: Codec2 encode (core `codec2_encode`) → core `build_message` → per-frame
    /// pacing (`tx_policy`) + queue backpressure → PRIVATE_APP data packets.
    async fn send_voice(&self, pcm: &[f32], in_rate: u32, channel: u32, to: Option<u32>) -> Result<(), JsValue> {
        // RNNoise (core's `Denoiser`) runs on 48 kHz mono normalised f32. Skip
        // when the input is at a different rate or the user disabled it; the
        // raw PCM goes straight to the encoder in that case.
        let cleaned: Vec<f32>;
        let pcm_for_encode: &[f32] = if self.denoise_enabled.get() && in_rate == 48_000 {
            let mut d = Denoiser::new();
            let mut out = Vec::with_capacity(pcm.len());
            d.process(pcm, &mut out);
            d.flush(&mut out);
            cleaned = out;
            log(&format!(
                "voice: denoised {} → {} samples @ 48 kHz",
                pcm.len(),
                cleaned.len()
            ));
            &cleaned
        } else {
            pcm
        };
        // Dispatch by the chosen send codec. Codec2 stays the LoRa-optimal
        // default; AMR-NB is offered for interop with desktop/Android senders
        // that prefer it. Both paths feed the same `build_message` framing.
        let (payload, codec, codec_param) = match self.send_codec.get() {
            1 => {
                let mode = self.amrnb_mode.get();
                let bytes = amrnb::amrnb_encode(pcm_for_encode, in_rate, mode)
                    .await
                    .map_err(|e| err(&format!("amrnb encode: {e:?}")))?;
                (bytes, VoiceCodec::AmrNb, mode)
            }
            _ => {
                let mode = self.codec_param.get();
                let bytes = codec2_encode(pcm_for_encode, in_rate, mode)
                    .map_err(|e| err(&e.to_string()))?;
                (bytes, VoiceCodec::Codec2, mode)
            }
        };
        // chunk_size + FEC parity match the desktop's policy — both are derived
        // from the radio's LoRa modem preset + destination via core helpers, so
        // a clip sent from the browser is wire-equivalent to one sent from the
        // GUI on the same radio. `VoiceFecMode::Auto` is desktop's default.
        let preset = self
            .state
            .borrow()
            .lora
            .as_ref()
            .and_then(|l| modem_preset_from_proto(l.modem_preset));
        let chunk_size = preset
            .map(ModemPreset::recommended_chunk_size)
            .unwrap_or(MAX_BODY_SIZE);
        let total_data = payload.len().div_ceil(chunk_size).max(1);
        let broadcast = to.is_none();
        let parity_count = VoiceFecMode::Auto.resolve(broadcast, preset, total_data);
        let cfg = BuildConfig {
            message_id: random_message_id().map_err(|e| err(&e.to_string()))?,
            stream_seq: 0,
            codec,
            codec_param,
            chunk_size,
            parity_count,
            last_in_stream: true,
        };
        let msg = build_message(&payload, &cfg).map_err(|e| err(&format!("build_message: {e}")))?;
        let total = msg.frames.len();
        let total_data = msg.total_data;
        let message_id = cfg.message_id;
        // Register before sending so an early NACK lands in the registry
        // (its `pending_chunks` is seeded to {0..total_data} so overlapping
        // NACKs are dedup'd until each chunk's `mark_chunk_sent` fires).
        self.registry.register(message_id, &msg, channel, to);
        log(&format!(
            "voice: sending {total} frames ({total_data} data + {} parity, {} codec bytes)",
            msg.parity_count,
            payload.len()
        ));
        let frames: Vec<(u8, Vec<u8>)> = msg
            .frames
            .into_iter()
            .enumerate()
            .map(|(i, f)| (i as u8, f))
            .collect();
        self.send_voice_frames(message_id, total_data, frames, channel, to)
            .await?;
        log(&format!("voice: sent {total} frames"));
        Ok(())
    }

    /// Paced, queue-backpressured send of a list of pre-built voice frames.
    /// Used both by the initial burst and by NACK-driven retransmits, so the
    /// pacing/backpressure policy applies identically. After each DATA chunk
    /// (`chunk_index < total_data`) is written, `registry.mark_chunk_sent`
    /// is called so a later NACK can request that chunk again if it's still
    /// missing. Parity frames aren't tracked (the receiver NACKs by data
    /// index only). The `Vec<u8>` payloads are owned bodies handed to the
    /// transport's `data_packet` builder.
    async fn send_voice_frames(
        &self,
        message_id: u32,
        total_data: u8,
        frames: Vec<(u8, Vec<u8>)>,
        channel: u32,
        to: Option<u32>,
    ) -> Result<(), JsValue> {
        let pacing = self.pacing();
        let want_ack = to.is_some();
        let mut last: Option<f64> = None;
        for (chunk_index, frame) in frames {
            // Pace: wait the remaining gap since the previous frame.
            let elapsed = last
                .map(|t| std::time::Duration::from_secs_f64((now_ms() - t).max(0.0) / 1000.0));
            let wait = tx_policy::pacing_delay(elapsed, pacing);
            if !wait.is_zero() {
                sleep_ms(wait.as_millis() as i32).await;
            }
            // Backpressure: don't push into a full firmware queue.
            let bp_start = now_ms();
            while !tx_policy::queue_has_room(self.queue_free.get()) {
                if now_ms() - bp_start
                    > tx_policy::RADIO_QUEUE_WAIT_TIMEOUT.as_millis() as f64
                {
                    break; // safety valve — proceed anyway
                }
                sleep_ms(60).await;
            }
            let id = self.alloc_id();
            let pv = protocol::data_packet(
                id,
                PRIVATE_APP as i32,
                frame,
                channel,
                to,
                want_ack,
                false,
            );
            self.write_payload(pv).await?;
            if chunk_index < total_data {
                self.registry.mark_chunk_sent(message_id, chunk_index);
            }
            last = Some(now_ms());
        }
        Ok(())
    }
}

/// Handle to a connected radio. Returned by [`connect`]; lives as long as JS
/// holds it. The inbound read loop runs in the background via `spawn_local`.
#[wasm_bindgen]
pub struct WebClient {
    inner: Rc<Inner>,
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

    /// Toggle the RNNoise denoiser. On by default. Takes effect on the next
    /// `sendVoice` (in-flight clips are unaffected).
    #[wasm_bindgen(js_name = setDenoiseEnabled)]
    pub fn set_denoise_enabled(&self, enabled: bool) {
        self.inner.denoise_enabled.set(enabled);
    }

    /// Set the Codec2 mode (0..=5; 0 = 3200 bps, 5 = 1200 bps). Takes effect
    /// on the next `sendVoice`. Out-of-range values are clamped.
    #[wasm_bindgen(js_name = setCodec2Mode)]
    pub fn set_codec2_mode(&self, mode: u8) {
        self.inner.codec_param.set(mode.min(5));
    }

    /// Pick the codec for outgoing voice: `"codec2"` (default, LoRa-optimal)
    /// or `"amrnb"` (Adaptive Multi-Rate Narrowband, for interop). Any other
    /// value is ignored. Decode of inbound voice always works for all
    /// supported codecs regardless of this setting.
    #[wasm_bindgen(js_name = setSendCodec)]
    pub fn set_send_codec(&self, codec: &str) {
        let id: u8 = match codec {
            "amrnb" | "amr-nb" | "AMR-NB" => 1,
            _ => 0,
        };
        self.inner.send_codec.set(id);
    }

    /// AMR-NB mode (0..=7) when sending in AMR-NB. Mode → bitrate:
    /// 0=4.75, 1=5.15, 2=5.9, 3=6.7, 4=7.4, 5=7.95 (default), 6=10.2, 7=12.2 kbps.
    /// Takes effect on the next `sendVoice`. Out-of-range values are clamped.
    #[wasm_bindgen(js_name = setAmrnbMode)]
    pub fn set_amrnb_mode(&self, mode: u8) {
        self.inner.amrnb_mode.set(mode.min(7));
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

    /// Send a voice clip: mono f32 PCM at `in_rate` Hz, encoded with Codec2 and
    /// sent as paced PRIVATE_APP frames. `to` undefined = broadcast.
    #[wasm_bindgen(js_name = sendVoice)]
    pub fn send_voice(
        &self,
        pcm: Vec<f32>,
        in_rate: f32,
        channel: u32,
        to: Option<u32>,
    ) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            inner.send_voice(&pcm, in_rate as u32, channel, to).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    // ---------- Settings: read ----------

    /// Snapshot of the device identity + the eight config sections + channels,
    /// as a plain JS object (via serde-wasm-bindgen). Fields the radio hasn't
    /// reported yet are `null`. Mirrors what `MeshtasticService::watch_*` give
    /// the desktop GUI.
    #[wasm_bindgen(js_name = snapshot)]
    pub fn snapshot(&self) -> Result<JsValue, JsValue> {
        let snap = settings::build_snapshot(&self.inner.state.borrow());
        serde_wasm_bindgen::to_value(&snap).map_err(|e| err(&format!("snapshot: {e}")))
    }

    // ---------- Settings: write ----------
    //
    // Each setter overlays the DTO on the current config from ProtocolState so
    // fields not in the DTO keep their device-reported value (the same effect
    // desktop's dirty-tracking gives on the editable subset). All go through
    // core's `protocol::admin_packet`, addressed to our own node.

    #[wasm_bindgen(js_name = writeOwner)]
    pub fn write_owner(&self, dto: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let dto: settings::OwnerDto = serde_wasm_bindgen::from_value(dto)
                .map_err(|e| err(&format!("owner dto: {e}")))?;
            let payload = settings::owner_payload(&inner.state.borrow(), dto);
            inner.send_admin(payload).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    #[wasm_bindgen(js_name = writeLoraConfig)]
    pub fn write_lora_config(&self, dto: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let dto: settings::LoraDto = serde_wasm_bindgen::from_value(dto)
                .map_err(|e| err(&format!("lora dto: {e}")))?;
            let payload = settings::lora_payload(&inner.state.borrow(), dto);
            inner.send_admin(payload).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    #[wasm_bindgen(js_name = writeDeviceConfig)]
    pub fn write_device_config(&self, dto: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let dto: settings::DeviceDto = serde_wasm_bindgen::from_value(dto)
                .map_err(|e| err(&format!("device dto: {e}")))?;
            let payload = settings::device_payload(&inner.state.borrow(), dto);
            inner.send_admin(payload).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    #[wasm_bindgen(js_name = writePositionConfig)]
    pub fn write_position_config(&self, dto: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let dto: settings::PositionDto = serde_wasm_bindgen::from_value(dto)
                .map_err(|e| err(&format!("position dto: {e}")))?;
            let payload = settings::position_payload(&inner.state.borrow(), dto);
            inner.send_admin(payload).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    #[wasm_bindgen(js_name = writePowerConfig)]
    pub fn write_power_config(&self, dto: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let dto: settings::PowerDto = serde_wasm_bindgen::from_value(dto)
                .map_err(|e| err(&format!("power dto: {e}")))?;
            let payload = settings::power_payload(&inner.state.borrow(), dto);
            inner.send_admin(payload).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    #[wasm_bindgen(js_name = writeNetworkConfig)]
    pub fn write_network_config(&self, dto: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let dto: settings::NetworkDto = serde_wasm_bindgen::from_value(dto)
                .map_err(|e| err(&format!("network dto: {e}")))?;
            let payload = settings::network_payload(&inner.state.borrow(), dto);
            inner.send_admin(payload).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    #[wasm_bindgen(js_name = writeDisplayConfig)]
    pub fn write_display_config(&self, dto: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let dto: settings::DisplayDto = serde_wasm_bindgen::from_value(dto)
                .map_err(|e| err(&format!("display dto: {e}")))?;
            let payload = settings::display_payload(&inner.state.borrow(), dto);
            inner.send_admin(payload).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    #[wasm_bindgen(js_name = writeBluetoothConfig)]
    pub fn write_bluetooth_config(&self, dto: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let dto: settings::BluetoothDto = serde_wasm_bindgen::from_value(dto)
                .map_err(|e| err(&format!("bluetooth dto: {e}")))?;
            let payload = settings::bluetooth_payload(&inner.state.borrow(), dto);
            inner.send_admin(payload).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    #[wasm_bindgen(js_name = writeChannel)]
    pub fn write_channel(&self, dto: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let dto: settings::ChannelDto = serde_wasm_bindgen::from_value(dto)
                .map_err(|e| err(&format!("channel dto: {e}")))?;
            let payload = settings::channel_payload(&inner.state.borrow(), dto);
            inner.send_admin(payload).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    #[wasm_bindgen(js_name = setFixedPosition)]
    pub fn set_fixed_position(&self, dto: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let dto: settings::FixedPositionDto = serde_wasm_bindgen::from_value(dto)
                .map_err(|e| err(&format!("fixed position dto: {e}")))?;
            let payload = settings::fixed_position_payload(dto);
            inner.send_admin(payload).await?;
            Ok(JsValue::UNDEFINED)
        })
    }
}

/// Milliseconds since the epoch (monotonic enough for pacing).
fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// Await `ms` milliseconds via `setTimeout` (no tokio on wasm).
async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        if let Some(win) = web_sys::window() {
            let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = JsFuture::from(promise).await;
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
        port.readable().get_reader().unchecked_into();

    let inner = Rc::new(Inner {
        _port: port,
        writer,
        state: RefCell::new(ProtocolState::default()),
        next_id: Cell::new(rand_u32()),
        assembler: VoiceAssembler::new(AssemblerConfig::default()),
        registry: OutgoingVoiceRegistry::default(),
        queue_free: Cell::new(u32::MAX),
        denoise_enabled: Cell::new(true),
        codec_param: Cell::new(DEFAULT_CODEC2_MODE),
        send_codec: Cell::new(0), // 0 = Codec2
        amrnb_mode: Cell::new(5), // MR795
    });

    // Background inbound loop: read → deframe → core decode → core state/voice.
    let rx = inner.clone();
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = read_loop(reader, rx, on_event, on_voice).await {
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

    // Hand the vendored AMR-NB wasm to its JS shim so the first voice
    // operation doesn't pay the WebAssembly.instantiate latency. Errors are
    // logged but non-fatal — Codec2 paths still work without it.
    wasm_bindgen_futures::spawn_local(async {
        if let Err(e) = amrnb::init().await {
            log(&format!("amrnb shim init failed: {e:?}"));
        }
    });

    // Kick off the config handshake using the core builder.
    let nonce = rand_u32();
    inner.write_payload(protocol::want_config(nonce)).await?;
    log(&format!("serial: sent WantConfigId nonce={nonce}"));

    Ok(WebClient { inner })
}

/// Read frames off the port forever, feeding each through the core decoder and
/// applying snapshot events to the shared `ProtocolState`.
async fn read_loop(
    reader: web_sys::ReadableStreamDefaultReader,
    inner: Rc<Inner>,
    on_event: js_sys::Function,
    on_voice: js_sys::Function,
) -> Result<(), JsValue> {
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
            // Snapshot the one bit the decoder needs from current state.
            let ctx = InboundCtx {
                my_node_num: inner.state.borrow().my_info.as_ref().map(|i| i.my_node_num),
            };
            match protocol::decode_inbound(&payload, &ctx) {
                Ok(events) => {
                    for ev in events {
                        if ev.is_snapshot() {
                            inner.state.borrow_mut().apply(&ev);
                        }
                        match &ev {
                            // Track queue depth for voice TX backpressure.
                            InboundEvent::QueueStatus(qs) => {
                                inner.queue_free.set(qs.free);
                                emit(&on_event, &format!("queue free={}", qs.free));
                            }
                            // Voice frames go through core's reassembler; a
                            // completed message is decoded and handed to JS.
                            InboundEvent::Voice(vd) => {
                                handle_voice(&inner, vd, &on_event, &on_voice);
                            }
                            _ => emit(&on_event, &event_summary(&ev, &inner.state.borrow())),
                        }
                    }
                }
                Err(e) => log(&format!("decode FromRadio failed: {e}")),
            }
        }
    }
}

/// Feed one voice frame to the assembler; on completion decode + play.
fn handle_voice(
    inner: &Rc<Inner>,
    vd: &voicetastic_core::radio_service::VoiceData,
    on_event: &js_sys::Function,
    on_voice: &js_sys::Function,
) {
    let from = vd.from.to_string();
    match inner.assembler.accept(&from, vd.to, vd.channel, &vd.payload) {
        AssemblyEvent::Complete(msg) => {
            emit(
                on_event,
                &format!(
                    "🎙️ voice complete from {from} to {} ch{} ({} chunks)",
                    voice_dest_str(&msg.to),
                    msg.channel,
                    msg.total_data
                ),
            );
            // Dispatch by codec. Codec2/Opus have pure-Rust decoders in core;
            // AMR-NB runs through the vendored emscripten wasm via a JS shim,
            // which is async — so its branch spawns a local future and feeds
            // the resulting PCM back to `on_voice` when it lands.
            match msg.codec {
                VoiceCodec::Codec2 => play_or_log(&codec2_decode(&msg.audio, msg.codec_param), on_voice, &from),
                VoiceCodec::Opus => play_or_log(&opus_decode(&msg.audio, msg.codec_param), on_voice, &from),
                VoiceCodec::AmrNb => {
                    let payload = msg.audio.clone();
                    let on_voice = on_voice.clone();
                    let from = from.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        match amrnb::amrnb_decode(&payload).await {
                            Ok((pcm, rate)) => {
                                let arr = js_sys::Float32Array::from(pcm.as_slice());
                                let _ = on_voice.call3(
                                    &JsValue::NULL,
                                    &arr,
                                    &JsValue::from_f64(rate as f64),
                                    &JsValue::from_str(&from),
                                );
                            }
                            Err(e) => log(&format!("AMR-NB decode failed: {e:?}")),
                        }
                    });
                }
                other => log(&format!("voice: codec {other:?} not playable in browser")),
            }
        }
        AssemblyEvent::Pending {
            received_data,
            total_data,
            ..
        } => emit(on_event, &format!("🎙️ voice {received_data}/{total_data} from {from}")),
        AssemblyEvent::Rejected(e) => log(&format!("voice rejected: {e}")),
        AssemblyEvent::Nack(nack) => {
            // The peer asked us to retransmit `nack.missing` chunks of one of
            // our outgoing messages. core's `OutgoingVoiceRegistry` does the
            // budget/cooldown/dedup; we just send the frames it hands back.
            let pacing = inner.pacing();
            let Some((channel, dest, total_data)) = inner.registry.meta(nack.message_id) else {
                log(&format!(
                    "nack for msg 0x{:x}: no registry entry (GC'd or never sent here)",
                    nack.message_id
                ));
                return;
            };
            match inner.registry.take_retransmit(nack.message_id, &nack.missing, pacing) {
                Ok(rt_frames) => {
                    let n = rt_frames.len();
                    let frames: Vec<(u8, Vec<u8>)> = rt_frames
                        .into_iter()
                        .map(|(idx, b)| (idx, b.to_vec()))
                        .collect();
                    emit(
                        on_event,
                        &format!(
                            "  ⟶ retransmitting {n} chunk(s) for msg 0x{:x} (NACK from {from})",
                            nack.message_id
                        ),
                    );
                    // Send paced on the background event loop so the read loop
                    // doesn't block on the burst.
                    let retx_inner = inner.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Err(e) = retx_inner
                            .send_voice_frames(nack.message_id, total_data, frames, channel, dest)
                            .await
                        {
                            log(&format!("retransmit send failed: {e:?}"));
                        }
                    });
                }
                Err(reason) => log(&format!(
                    "retransmit skipped for msg 0x{:x}: {reason:?}",
                    nack.message_id
                )),
            }
        }
        _ => {}
    }
}

fn emit(cb: &js_sys::Function, line: &str) {
    let _ = cb.call1(&JsValue::NULL, &JsValue::from_str(line));
}

/// Hand a freshly-decoded PCM block to the JS playback callback, or log the
/// codec error if decode failed. Shared by the Codec2 and Opus decode arms.
fn play_or_log(
    decoded: &Result<(Vec<f32>, u32), voicetastic_core::codec::CodecError>,
    on_voice: &js_sys::Function,
    from: &str,
) {
    match decoded {
        Ok((pcm, rate)) => {
            let arr = js_sys::Float32Array::from(pcm.as_slice());
            let _ = on_voice.call3(
                &JsValue::NULL,
                &arr,
                &JsValue::from_f64(*rate as f64),
                &JsValue::from_str(from),
            );
        }
        Err(e) => log(&format!("voice decode failed: {e}")),
    }
}

/// Render a `VoiceDestination` as the same `0x...`-hex format we use for
/// node IDs in event lines, with `0xffffffff` standing in for broadcast — so
/// the JS chat router can apply the exact same broadcast-vs-DM rule it uses
/// for IncomingText.
fn voice_dest_str(dest: &voicetastic_core::voice::VoiceDestination) -> String {
    use voicetastic_core::voice::VoiceDestination::*;
    match dest {
        Broadcast => "0xffffffff".to_string(),
        Node(id) => format!("0x{:x}", id.as_u32()),
    }
}

/// Drive `VoiceAssembler::tick()` periodically and emit any returned NACK
/// frames back to the original sender. Mirrors the desktop GUI's RX-side
/// reliability: the framing + retry-round bookkeeping lives in core, this
/// loop just does the timer and the writes. Exits when only this task holds
/// `inner` (the `WebClient` handle and the read loop have both dropped).
async fn nack_tick_loop(inner: Rc<Inner>) {
    const TICK_MS: i32 = 500;
    loop {
        sleep_ms(TICK_MS).await;
        if Rc::strong_count(&inner) <= 1 {
            return;
        }
        let out = inner.assembler.tick();
        for nack in out.nacks {
            let Ok(node) = nack.from.parse::<NodeId>() else {
                log(&format!("nack: malformed sender id {:?}", nack.from));
                continue;
            };
            let id = inner.alloc_id();
            let pv = protocol::data_packet(
                id,
                PRIVATE_APP as i32,
                nack.frame,
                nack.channel,
                Some(node.as_u32()),
                false, // want_ack — NACK is a hint; if lost, the next round retries
                false, // want_response
            );
            if let Err(e) = inner.write_payload(pv).await {
                log(&format!("nack send failed: {e:?}"));
                continue;
            }
            log(&format!(
                "  ⟶ NACK round {} for msg 0x{:x} ({} missing) to {}",
                nack.round, nack.message_id, nack.missing_count, nack.from
            ));
        }
        for msg in out.finalized {
            if !msg.is_complete {
                log(&format!(
                    "voice partial finalize from {} ({}/{} chunks)",
                    msg.from, msg.received_data, msg.total_data
                ));
            }
        }
    }
}

/// One-line, JS-friendly description of an inbound event (for the demo UI).
fn event_summary(ev: &InboundEvent, state: &ProtocolState) -> String {
    use voicetastic_core::proto::config::PayloadVariant as Cfg;
    match ev {
        InboundEvent::MyInfo(i) => format!("MyNodeInfo node_num=0x{:x}", i.my_node_num),
        InboundEvent::NodeInfo(ni) => {
            let name = ni
                .user
                .as_ref()
                .map(|u| u.long_name.as_str())
                .unwrap_or("?");
            format!("NodeInfo 0x{:x} \"{name}\" (known nodes: {})", ni.num, state.nodes.len())
        }
        InboundEvent::Owner(u) => format!("Owner \"{}\"", u.long_name),
        InboundEvent::Config(v) => {
            let which = match v {
                Cfg::Lora(_) => "lora",
                Cfg::Device(_) => "device",
                Cfg::Position(_) => "position",
                Cfg::Power(_) => "power",
                Cfg::Network(_) => "network",
                Cfg::Display(_) => "display",
                Cfg::Bluetooth(_) => "bluetooth",
                _ => "other",
            };
            format!("Config: {which}")
        }
        InboundEvent::Channel(ch) => format!("Channel[{}] (total: {})", ch.index, state.channels.len()),
        InboundEvent::Metadata(m) => format!("Metadata fw={}", m.firmware_version),
        InboundEvent::ConfigComplete(n) => format!(
            "✅ ConfigComplete nonce={n} — READY (nodes={}, channels={}, fw={})",
            state.nodes.len(),
            state.channels.len(),
            state
                .metadata
                .as_ref()
                .map(|m| m.firmware_version.as_str())
                .unwrap_or("?")
        ),
        InboundEvent::IncomingText(t) => format!(
            "💬 text from 0x{:x} to 0x{:x} ch{}: {}",
            t.from, t.to, t.channel, t.text
        ),
        InboundEvent::IncomingData(d) => {
            format!("data port={} from=0x{:x} ({} bytes)", d.portnum, d.from, d.payload.len())
        }
        InboundEvent::Voice(vd) => format!("🎙️ voice from {:?} ({} bytes)", vd.from, vd.payload.len()),
        InboundEvent::QueueStatus(qs) => format!("queue free={}", qs.free),
    }
}

/// Prepend the 4-byte Meshtastic serial header (`0x94 0xc3 len_hi len_lo`).
fn frame_serial(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u16;
    let mut v = Vec::with_capacity(payload.len() + 4);
    v.extend_from_slice(&[START1, START2, (len >> 8) as u8, (len & 0xff) as u8]);
    v.extend_from_slice(payload);
    v
}

/// Extract one framed payload from the front of `buf`, scanning past console
/// noise. Returns `(payload, bytes_consumed)`, an empty payload as a resync
/// marker on a bad length, or `None` when more bytes are needed.
fn next_frame(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == START1 && buf[i + 1] == START2 {
            break;
        }
        i += 1;
    }
    if i + 1 >= buf.len() {
        return None;
    }
    if i + 4 > buf.len() {
        return None;
    }
    let len = ((buf[i + 2] as usize) << 8) | (buf[i + 3] as usize);
    if len == 0 || len > MAX_PAYLOAD {
        return Some((Vec::new(), i + 1));
    }
    let start = i + 4;
    if start + len > buf.len() {
        return None;
    }
    Some((buf[start..start + len].to_vec(), start + len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let f = frame_serial(b"hi");
        assert_eq!(&f[..4], &[0x94, 0xc3, 0x00, 0x02]);
        let (p, n) = next_frame(&f).unwrap();
        assert_eq!(p, b"hi");
        assert_eq!(n, f.len());
    }

    #[test]
    fn skips_leading_noise() {
        let mut data = b"debug log\n".to_vec();
        data.extend(frame_serial(b"ok"));
        assert_eq!(next_frame(&data).unwrap().0, b"ok");
    }
}
