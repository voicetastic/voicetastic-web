//! Voice TX/RX paths: encode/decode dispatch, paced send, NACK service,
//! and the JS-facing playback callback. Splits across two impl blocks
//! (`Inner` for the send pipeline, `WebClient` for the wasm-bindgen
//! setters + `sendVoice`) plus the free functions the read-loop and
//! background tick driver invoke.
//!
//! No serial / framing logic lives here — that's `crate::serial`. No
//! protocol decode either — that's `voicetastic_core::protocol`. This
//! module is just "everything codec/voice-shaped."

use std::rc::Rc;

use voicetastic_core::codec::{
    amrnb_decode, amrnb_encode, codec2_decode, codec2_encode, opus_encode, opus_wasm_decode,
    Denoiser,
};
use voicetastic_core::node::NodeId;
use voicetastic_core::ports::PRIVATE_APP;
use voicetastic_core::protocol;
use voicetastic_core::service::modem_preset_from_proto;
use voicetastic_core::settings::api::VoiceFecMode;
use voicetastic_core::voice::{
    AssemblyEvent, VoiceCodec, VoiceTx, VoiceTxAction, prepare_voice_send,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;
use web_time::Instant;

use crate::events::emit;
use crate::util::{err, log, sleep_ms};
use crate::{Inner, WebClient};

// =============================================================================
// Send path — methods on Inner that the WebClient `sendVoice` wraps.
// =============================================================================

impl Inner {
    /// Encode + frame + paced-send a voice clip. Mirrors the native voice TX
    /// worker: codec encode (core's `*_encode`) → core `build_message` →
    /// per-frame pacing (`tx_policy`) + queue backpressure → PRIVATE_APP
    /// data packets.
    pub(crate) async fn send_voice(
        &self,
        pcm: &[f32],
        in_rate: u32,
        channel: u32,
        to: Option<u32>,
    ) -> Result<(), JsValue> {
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
        // default; AMR-NB and Opus are offered for interop with desktop/
        // Android senders that prefer them. All three paths feed the same
        // `prepare_voice_send` framing.
        let (payload, codec, codec_param) = match self.send_codec.get() {
            1 => {
                let mode = self.amrnb_mode.get();
                let bytes = amrnb_encode(pcm_for_encode, in_rate, mode)
                    .await
                    .map_err(|e| err(&format!("amrnb encode: {e:?}")))?;
                (bytes, VoiceCodec::AmrNb, mode)
            }
            2 => {
                let kbps = self.opus_kbps.get();
                let bytes = opus_encode(pcm_for_encode, in_rate, kbps)
                    .await
                    .map_err(|e| err(&format!("opus encode: {e:?}")))?;
                (bytes, VoiceCodec::Opus, kbps)
            }
            _ => {
                let mode = self.codec_param.get();
                let bytes = codec2_encode(pcm_for_encode, in_rate, mode)
                    .map_err(|e| err(&e.to_string()))?;
                (bytes, VoiceCodec::Codec2, mode)
            }
        };
        // chunk_size + FEC parity match the desktop's policy — derived from
        // the radio's LoRa modem preset + destination via core helpers, so
        // a clip sent from the browser is wire-equivalent to one sent from
        // the GUI on the same radio.
        let preset = self
            .state
            .borrow()
            .lora
            .as_ref()
            .and_then(|l| modem_preset_from_proto(l.modem_preset));
        let payload_bytes = payload.len();
        let prep =
            prepare_voice_send(payload, codec, codec_param, preset, to.is_none(), VoiceFecMode::Auto)
                .map_err(|e| err(&format!("prepare voice: {e}")))?;
        // Register before sending so an early NACK lands in the registry
        // (its `pending_chunks` is seeded to {0..total_data} so overlapping
        // NACKs are dedup'd until each chunk's `mark_chunk_sent` fires).
        self.registry
            .register(prep.message_id, &prep.encoded, channel, to);
        log(&format!(
            "voice: sending {} frames ({} data + {} parity, {} codec bytes)",
            prep.frames.len(),
            prep.total_data,
            prep.parity_count,
            payload_bytes,
        ));
        self.send_voice_frames(prep.message_id, prep.total_data, prep.frames, channel, to)
            .await?;
        log("voice: sent");
        Ok(())
    }

    /// Paced, queue-backpressured send of a list of pre-built voice frames.
    /// Used both by the initial burst and by NACK-driven retransmits, so the
    /// pacing/backpressure policy applies identically. The loop is the
    /// thinnest possible wrapper around core's [`VoiceTx`] state machine:
    /// pacing math + the `RADIO_QUEUE_WAIT_TIMEOUT` safety valve live in
    /// core, this just sleeps and writes.
    pub(crate) async fn send_voice_frames(
        &self,
        message_id: u32,
        total_data: u8,
        frames: Vec<(u8, Vec<u8>)>,
        channel: u32,
        to: Option<u32>,
    ) -> Result<(), JsValue> {
        let mut tx = VoiceTx::new(total_data, frames, channel, to, self.pacing());
        loop {
            match tx.next_action(Instant::now(), self.queue_free.get()) {
                VoiceTxAction::Send {
                    chunk_index,
                    frame,
                    channel,
                    to,
                    want_ack,
                    is_data,
                } => {
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
                    if is_data {
                        self.registry.mark_chunk_sent(message_id, chunk_index);
                    }
                }
                VoiceTxAction::Wait(d) => sleep_ms(d.as_millis() as i32).await,
                VoiceTxAction::Done => break,
            }
        }
        Ok(())
    }
}

// =============================================================================
// WebClient JS API surface: voice config setters + `sendVoice`.
// =============================================================================

#[wasm_bindgen]
impl WebClient {
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

    /// Pick the codec for outgoing voice: `"codec2"` (default, LoRa-optimal),
    /// `"amrnb"` (Adaptive Multi-Rate Narrowband, telephony interop), or
    /// `"opus"` (best quality, higher airtime). Any other value is ignored.
    /// Decode of inbound voice always works for all supported codecs
    /// regardless of this setting.
    #[wasm_bindgen(js_name = setSendCodec)]
    pub fn set_send_codec(&self, codec: &str) {
        let id: u8 = match codec {
            "amrnb" | "amr-nb" | "AMR-NB" => 1,
            "opus" | "Opus" | "OPUS" => 2,
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

    /// Opus target bitrate in kbps (6..=128 per RFC 6716) when sending in
    /// Opus. Default 12 kbps matches desktop's `OPUS_BITRATE`. Takes effect
    /// on the next `sendVoice`. Out-of-range values are clamped.
    #[wasm_bindgen(js_name = setOpusKbps)]
    pub fn set_opus_kbps(&self, kbps: u8) {
        self.inner.opus_kbps.set(kbps.clamp(6, 128));
    }

    /// Send a voice clip: mono f32 PCM at `in_rate` Hz, encoded with the
    /// active send codec and sent as paced PRIVATE_APP frames.
    /// `to` undefined = broadcast.
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
}

// =============================================================================
// RX path — `handle_voice` is called by the read loop in lib.rs.
// =============================================================================

/// Feed one voice frame to the assembler; on completion decode + emit to JS.
pub(crate) fn handle_voice(
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
            // Dispatch by codec. Codec2 has a pure-Rust decoder in core;
            // AMR-NB and Opus go through their vendored emscripten wasms via
            // JS shims, which are async — those branches spawn local futures
            // and feed the resulting PCM back to `on_voice` when it lands.
            let to_str = voice_dest_str(&msg.to);
            let channel = msg.channel;
            let codec_id = msg.codec;
            match msg.codec {
                VoiceCodec::Codec2 => emit_voice_or_log(
                    &codec2_decode(&msg.audio, msg.codec_param),
                    on_voice,
                    &from,
                    &to_str,
                    channel,
                    codec_id,
                ),
                VoiceCodec::Opus => {
                    let payload = msg.audio.clone();
                    let codec_param = msg.codec_param;
                    let on_voice = on_voice.clone();
                    let from = from.clone();
                    let to_str = to_str.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        match opus_wasm_decode(&payload, codec_param).await {
                            Ok((pcm, rate)) => emit_voice(
                                &on_voice, &pcm, rate, &from, &to_str, channel, codec_id,
                            ),
                            Err(e) => log(&format!("Opus decode failed: {e:?}")),
                        }
                    });
                }
                VoiceCodec::AmrNb => {
                    let payload = msg.audio.clone();
                    let on_voice = on_voice.clone();
                    let from = from.clone();
                    let to_str = to_str.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        match amrnb_decode(&payload).await {
                            Ok((pcm, rate)) => emit_voice(
                                &on_voice, &pcm, rate, &from, &to_str, channel, codec_id,
                            ),
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
        } => emit(
            on_event,
            &format!("🎙️ voice {received_data}/{total_data} from {from}"),
        ),
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
            match inner
                .registry
                .take_retransmit(nack.message_id, &nack.missing, pacing)
            {
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

// =============================================================================
// JS playback callback — pack PCM + routing into one object and emit.
// =============================================================================

/// Hand a freshly-decoded PCM block to the JS playback callback. Packs all
/// routing/metadata into one JS object so the chat-router can put the clip
/// in the right thread:
///
/// ```ts
/// { pcm: Float32Array, rate: number, from: string, to: string,
///   channel: number, codec: string, duration_ms: number }
/// ```
///
/// `to` follows the same `0x...` / `0xffffffff` convention used for text
/// events — see [`voice_dest_str`].
pub(crate) fn emit_voice(
    on_voice: &js_sys::Function,
    pcm: &[f32],
    rate: u32,
    from: &str,
    to: &str,
    channel: u32,
    codec: VoiceCodec,
) {
    let arr = js_sys::Float32Array::from(pcm);
    let duration_ms = if rate > 0 {
        (pcm.len() as f64 * 1000.0) / rate as f64
    } else {
        0.0
    };
    let detail = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&detail, &"pcm".into(), &arr.into());
    let _ = js_sys::Reflect::set(&detail, &"rate".into(), &JsValue::from_f64(rate as f64));
    let _ = js_sys::Reflect::set(&detail, &"from".into(), &JsValue::from_str(from));
    let _ = js_sys::Reflect::set(&detail, &"to".into(), &JsValue::from_str(to));
    let _ = js_sys::Reflect::set(&detail, &"channel".into(), &JsValue::from_f64(channel as f64));
    let _ = js_sys::Reflect::set(&detail, &"codec".into(), &JsValue::from_str(codec_name(codec)));
    let _ = js_sys::Reflect::set(&detail, &"duration_ms".into(), &JsValue::from_f64(duration_ms));
    let _ = on_voice.call1(&JsValue::NULL, &detail);
}

/// Sync wrapper for codecs that decode without an await — Codec2 today.
fn emit_voice_or_log(
    decoded: &Result<(Vec<f32>, u32), voicetastic_core::codec::CodecError>,
    on_voice: &js_sys::Function,
    from: &str,
    to: &str,
    channel: u32,
    codec: VoiceCodec,
) {
    match decoded {
        Ok((pcm, rate)) => emit_voice(on_voice, pcm, *rate, from, to, channel, codec),
        Err(e) => log(&format!("voice decode failed: {e}")),
    }
}

fn codec_name(codec: VoiceCodec) -> &'static str {
    match codec {
        VoiceCodec::Codec2 => "codec2",
        VoiceCodec::Opus => "opus",
        VoiceCodec::AmrNb => "amrnb",
        _ => "other",
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

// =============================================================================
// Background NACK driver — ticks core's VoiceAssembler.
// =============================================================================

/// Drive `VoiceAssembler::tick()` periodically and emit any returned NACK
/// frames back to the original sender. Mirrors the desktop GUI's RX-side
/// reliability: the framing + retry-round bookkeeping lives in core, this
/// loop just does the timer and the writes. Exits when only this task holds
/// `inner` (the `WebClient` handle and the read loop have both dropped).
pub(crate) async fn nack_tick_loop(inner: Rc<Inner>) {
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
