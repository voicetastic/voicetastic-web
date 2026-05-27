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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use prost::Message as _;
use voicetastic_core::proto::ToRadio;
use voicetastic_core::protocol::{self, InboundCtx, InboundEvent, ProtocolState};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, future_to_promise};

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
}

/// Connect to a user-selected Meshtastic radio over Web Serial and start
/// driving `voicetastic_core`'s protocol core. `on_event` is invoked with a
/// short string for every decoded inbound event. Resolves once connected (the
/// read loop continues in the background).
///
/// Must be called from a user gesture (the Web Serial port picker requires it).
#[wasm_bindgen]
pub async fn connect(on_event: js_sys::Function) -> Result<WebClient, JsValue> {
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
    });

    // Background inbound loop: read → deframe → core decode → core state.
    let rx = inner.clone();
    let cb = on_event;
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = read_loop(reader, rx, cb).await {
            log(&format!("serial read loop ended: {e:?}"));
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
                        let summary = event_summary(&ev, &inner.state.borrow());
                        let _ = on_event.call1(&JsValue::NULL, &JsValue::from_str(&summary));
                    }
                }
                Err(e) => log(&format!("decode FromRadio failed: {e}")),
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
        InboundEvent::IncomingText(t) => format!("💬 text from 0x{:x}: {}", t.from, t.text),
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
