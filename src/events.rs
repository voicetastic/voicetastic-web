//! JS-facing event log formatting + the small `emit` helper that forwards
//! one-line summaries to the JS callback. Pure formatting — no state
//! mutation, no I/O.

use voicetastic_core::protocol::{InboundEvent, ProtocolState};
use wasm_bindgen::prelude::*;

/// Call the JS-side `on_event(line)` callback with a one-line string.
pub fn emit(cb: &js_sys::Function, line: &str) {
    let _ = cb.call1(&JsValue::NULL, &JsValue::from_str(line));
}

/// One-line, JS-friendly description of an inbound event (for the demo UI).
pub fn event_summary(ev: &InboundEvent, state: &ProtocolState) -> String {
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
