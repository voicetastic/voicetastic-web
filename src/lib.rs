//! Connectivity gate for the Voicetastic browser client.
//!
//! Proves the browser-side unknowns end-to-end against a real radio over
//! Web Serial, reusing `voicetastic-core`'s Meshtastic proto types:
//!   1. open a user-selected serial port (Web Serial),
//!   2. send a `WantConfigId` `ToRadio`, framed with the `0x94 0xc3` serial header,
//!   3. read + deframe the inbound byte stream,
//!   4. decode each `FromRadio` and resolve when `MyNodeInfo` arrives.
//!
//! It deliberately does NOT use core's `Transport` trait /
//! `connect_with_transport`: those require `Send` (browser JS handles are
//! `!Send`) and a *driven* tokio runtime. Wiring those into core (a `?Send`
//! path on wasm32 + a `spawn_local` driver) is the next step — this gate
//! isolates the Web Serial + wire-framing + protobuf risk first, the same way
//! the compile gate isolated `mio`/`getrandom`.

use prost::Message;
use voicetastic_core::proto;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

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

/// Connect to a user-selected Meshtastic radio over Web Serial, request its
/// config, and resolve with the node number from `MyNodeInfo`.
///
/// Must be called from a user gesture (e.g. a click handler) — the Web Serial
/// port picker requires it.
#[wasm_bindgen]
pub async fn connect_and_read_my_node_info() -> Result<JsValue, JsValue> {
    let window = web_sys::window().ok_or_else(|| err("no window"))?;
    let serial = window.navigator().serial();

    // 1. Ask the user to pick a port, then open it.
    let port: web_sys::SerialPort = JsFuture::from(serial.request_port()).await?.dyn_into()?;
    let opts = web_sys::SerialOptions::new(BAUD);
    JsFuture::from(port.open(&opts)).await?;
    log(&format!("serial: port open @{BAUD}"));

    // 2. Build + frame a WantConfigId ToRadio (reusing core's proto types).
    let nonce = rand_u32();
    let to = proto::ToRadio {
        payload_variant: Some(proto::to_radio::PayloadVariant::WantConfigId(nonce)),
    };
    let mut payload = Vec::new();
    to.encode(&mut payload)
        .map_err(|e| err(&format!("encode ToRadio: {e}")))?;
    let frame = frame_serial(&payload);

    // 3. Write it to the port.
    let writer = port
        .writable()
        .get_writer()
        .map_err(|e| err(&format!("get_writer: {e:?}")))?;
    let chunk = js_sys::Uint8Array::from(frame.as_slice());
    JsFuture::from(writer.write_with_chunk(chunk.as_ref())).await?;
    writer.release_lock();
    log(&format!("serial: sent WantConfigId nonce={nonce}"));

    // 4. Read + deframe until a FromRadio carries MyNodeInfo.
    let reader: web_sys::ReadableStreamDefaultReader = port.readable().get_reader().unchecked_into();
    let mut buf: Vec<u8> = Vec::new();

    loop {
        let result = JsFuture::from(reader.read()).await?;
        let done = js_sys::Reflect::get(&result, &JsValue::from_str("done"))?
            .as_bool()
            .unwrap_or(false);
        if done {
            return Err(err("serial stream closed before MyNodeInfo arrived"));
        }
        let value = js_sys::Reflect::get(&result, &JsValue::from_str("value"))?;
        let arr = js_sys::Uint8Array::new(&value);
        let mut chunk = vec![0u8; arr.length() as usize];
        arr.copy_to(&mut chunk);
        buf.extend_from_slice(&chunk);

        // Drain every complete frame currently buffered.
        while let Some((payload, consumed)) = next_frame(&buf) {
            buf.drain(..consumed);
            if payload.is_empty() {
                continue; // resync marker — skipped noise
            }
            match proto::FromRadio::decode(payload.as_slice()) {
                Ok(fr) => {
                    if let Some(proto::from_radio::PayloadVariant::MyInfo(info)) = fr.payload_variant
                    {
                        log(&format!("serial: MyNodeInfo node_num={}", info.my_node_num));
                        let _ = reader.cancel();
                        return Ok(JsValue::from_f64(f64::from(info.my_node_num)));
                    }
                }
                Err(e) => log(&format!("decode FromRadio failed (skipping frame): {e}")),
            }
        }
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

/// Try to extract one framed payload from the front of `buf`.
///
/// Returns `(payload, bytes_consumed)`. Scans past leading console-log noise
/// like core's `read_frame`. On an invalid length it consumes the bytes up to
/// and including the false `START1` and returns an empty payload as a resync
/// marker. Returns `None` when more bytes are still needed.
fn next_frame(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == START1 && buf[i + 1] == START2 {
            break;
        }
        i += 1;
    }
    if i + 1 >= buf.len() {
        return None; // header not seen yet
    }
    if i + 4 > buf.len() {
        return None; // header incomplete
    }
    let len = ((buf[i + 2] as usize) << 8) | (buf[i + 3] as usize);
    if len == 0 || len > MAX_PAYLOAD {
        return Some((Vec::new(), i + 1)); // resync past the false START1
    }
    let start = i + 4;
    if start + len > buf.len() {
        return None; // payload incomplete
    }
    Some((buf[start..start + len].to_vec(), start + len))
}

fn rand_u32() -> u32 {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    u32::from_le_bytes(b)
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
        let (p, _) = next_frame(&data).unwrap();
        assert_eq!(p, b"ok");
    }

    #[test]
    fn incomplete_payload_waits() {
        let mut data = vec![START1, START2, 0x00, 0x0a];
        data.extend_from_slice(b"abc"); // only 3 of 10 bytes
        assert!(next_frame(&data).is_none());
    }
}
