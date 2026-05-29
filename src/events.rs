//! JS-facing event objects. The wasm side ships structured `{ type, text,
//! ...fields }` objects to `on_event` so the JS can `switch (ev.type)`
//! instead of regex-parsing a human-readable string. `text` is always set
//! to a one-line summary suitable for the user-visible event log.

use voicetastic_core::protocol::{InboundEvent, ProtocolState};
use wasm_bindgen::prelude::*;

// ---------- low-level JS object helpers ----------

fn obj() -> js_sys::Object {
    js_sys::Object::new()
}
fn set(o: &js_sys::Object, k: &str, v: &JsValue) {
    let _ = js_sys::Reflect::set(o, &JsValue::from_str(k), v);
}
fn set_str(o: &js_sys::Object, k: &str, v: &str) {
    set(o, k, &JsValue::from_str(v));
}
fn set_u32(o: &js_sys::Object, k: &str, v: u32) {
    set(o, k, &JsValue::from_f64(v as f64));
}
fn set_i32(o: &js_sys::Object, k: &str, v: i32) {
    set(o, k, &JsValue::from_f64(v as f64));
}

// ---------- emit helpers ----------

/// Fire one structured event object at the JS callback.
pub fn emit(cb: &js_sys::Function, ev: &JsValue) {
    let _ = cb.call1(&JsValue::NULL, ev);
}

/// Fire a log-only event — `{ type: 'log', text }`. For informational
/// strings that don't carry structured fields (voice progress, NACK
/// retransmit notices, etc.).
pub fn emit_log(cb: &js_sys::Function, text: &str) {
    let o = obj();
    set_str(&o, "type", "log");
    set_str(&o, "text", text);
    emit(cb, &o.into());
}

// ---------- structured event builder ----------

/// Convert a core `InboundEvent` into a JS object the harness can switch
/// on. Shape per variant (always carries `type` + `text`):
///
/// - `my_info`        { node_num }
/// - `node_info`      { node_num, long_name }
/// - `owner`          { long_name }
/// - `config`         { section: "lora"|"device"|… }
/// - `channel`        { index }
/// - `metadata`       { firmware_version }
/// - `config_complete`{ nonce }
/// - `incoming_text`  { from, to, channel, body }
/// - `incoming_data`  { port, from, len }
/// - `queue_status`   { free }
/// - `log`            (Voice — the real voice goes through `on_voice`)
pub fn build_event(ev: &InboundEvent, state: &ProtocolState) -> JsValue {
    use voicetastic_core::proto::config::PayloadVariant as Cfg;
    let o = obj();
    match ev {
        InboundEvent::MyInfo(i) => {
            set_str(&o, "type", "my_info");
            set_u32(&o, "node_num", i.my_node_num);
            set_str(&o, "text", &format!("MyNodeInfo node_num=0x{:x}", i.my_node_num));
        }
        InboundEvent::NodeInfo(ni) => {
            let name = ni
                .user
                .as_ref()
                .map(|u| u.long_name.as_str())
                .unwrap_or("?");
            set_str(&o, "type", "node_info");
            set_u32(&o, "node_num", ni.num);
            set_str(&o, "long_name", name);
            set_str(
                &o,
                "text",
                &format!(
                    "NodeInfo 0x{:x} \"{name}\" (known nodes: {})",
                    ni.num,
                    state.nodes.len()
                ),
            );
        }
        InboundEvent::Owner(u) => {
            set_str(&o, "type", "owner");
            set_str(&o, "long_name", &u.long_name);
            set_str(&o, "text", &format!("Owner \"{}\"", u.long_name));
        }
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
            set_str(&o, "type", "config");
            set_str(&o, "section", which);
            set_str(&o, "text", &format!("Config: {which}"));
        }
        InboundEvent::Channel(ch) => {
            set_str(&o, "type", "channel");
            set_i32(&o, "index", ch.index);
            set_str(
                &o,
                "text",
                &format!("Channel[{}] (total: {})", ch.index, state.channels.len()),
            );
        }
        InboundEvent::Metadata(m) => {
            set_str(&o, "type", "metadata");
            set_str(&o, "firmware_version", &m.firmware_version);
            set_str(&o, "text", &format!("Metadata fw={}", m.firmware_version));
        }
        InboundEvent::ConfigComplete(n) => {
            set_str(&o, "type", "config_complete");
            set_u32(&o, "nonce", *n);
            set_str(
                &o,
                "text",
                &format!(
                    "✅ ConfigComplete nonce={n} — READY (nodes={}, channels={}, fw={})",
                    state.nodes.len(),
                    state.channels.len(),
                    state
                        .metadata
                        .as_ref()
                        .map(|m| m.firmware_version.as_str())
                        .unwrap_or("?")
                ),
            );
        }
        InboundEvent::IncomingText(t) => {
            set_str(&o, "type", "incoming_text");
            set_u32(&o, "from", t.from);
            set_u32(&o, "to", t.to);
            set_u32(&o, "channel", t.channel);
            set_str(&o, "body", &t.text);
            set_str(
                &o,
                "text",
                &format!(
                    "💬 text from 0x{:x} to 0x{:x} ch{}: {}",
                    t.from, t.to, t.channel, t.text
                ),
            );
        }
        InboundEvent::IncomingData(d) => {
            set_str(&o, "type", "incoming_data");
            set_i32(&o, "port", d.portnum);
            set_u32(&o, "from", d.from);
            set_u32(&o, "len", d.payload.len() as u32);
            set_str(
                &o,
                "text",
                &format!(
                    "data port={} from=0x{:x} ({} bytes)",
                    d.portnum,
                    d.from,
                    d.payload.len()
                ),
            );
        }
        InboundEvent::Voice(vd) => {
            // The real voice flow goes through `on_voice`; this is just a
            // log line so the user can see the frame scroll past.
            set_str(&o, "type", "log");
            set_str(
                &o,
                "text",
                &format!("🎙️ voice from {:?} ({} bytes)", vd.from, vd.payload.len()),
            );
        }
        InboundEvent::QueueStatus(qs) => {
            set_str(&o, "type", "queue_status");
            set_u32(&o, "free", qs.free);
            set_str(&o, "text", &format!("queue free={}", qs.free));
        }
    }
    o.into()
}
