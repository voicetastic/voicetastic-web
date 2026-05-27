# voicetastic-web

Browser (WASM) client for [Voicetastic](https://git.cha-sam.re/voicetastic) — async
voice messages over Meshtastic, with **no install**: the radio plugs into the user's
own machine and the page talks to it directly over **Web Serial**.

This reuses the desktop engine (`voicetastic-core`) compiled to `wasm32`, rather than
reimplementing the protocol. See the architecture notes below.

## Status: browser driver over the sans-IO core

This is a real driver for `voicetastic-core`'s sans-IO protocol core — the wasm sibling
of the desktop's native `MeshtasticService`. It runs the **same** protocol logic the
desktop and Android clients use, with the browser supplying only the platform glue.

`connect(onEvent)`:

1. opens a user-selected serial port (Web Serial),
2. sends a `WantConfigId` built by `voicetastic_core::protocol::want_config`,
3. spawns a background read loop (`spawn_local`) that deframes the `0x94 0xc3` stream and
   feeds each frame through `protocol::decode_inbound`, applying snapshot events to
   `protocol::ProtocolState` and surfacing a summary of every event to `onEvent`,
4. returns a `WebClient` whose `sendText(...)` builds packets with
   `protocol::text_packet` and writes them back.

No Meshtastic decode/build/state logic lives in this crate — only Web Serial, the serial
framing, and ferrying events to JS. That's the payoff of the sans-IO refactor in
`voicetastic-core` (the `protocol` module): one protocol implementation, two drivers.

### Voice

Voice messaging works, reusing core's voice pipeline:

- **Codec**: Codec2, **from core** — `voicetastic_core::codec::codec2_encode/decode`,
  enabled via core's wasm-safe `codec2` feature (the pure-Rust `codec2` crate; no
  emscripten, no JS codec module, no codec code in this crate). The codec stays a single
  implementation in core, shared with desktop/Android.
- **TX** (`WebClient.sendVoice`): mic PCM → Codec2 encode → core `build_message` →
  per-frame pacing via `voice::tx_policy` + firmware queue backpressure → PRIVATE_APP
  frames.
- **RX**: PRIVATE_APP frames → core's sans-IO `VoiceAssembler` → on completion, Codec2
  decode → PCM handed to the JS playback callback.
- Mic capture + playback are Web Audio (the only JS-side audio glue); everything else is
  core's Rust.

v1 limits: one message per clip (~13 s at 1200 bps), no Reed-Solomon parity, and NACK
retransmit stays native-only — so voice is best-effort over good links for now. Codec2
only on the playback path.

## Build

Prerequisites: Rust 1.95+, the `wasm32-unknown-unknown` target, `wasm-pack`, and
`protoc` (the sibling `voicetastic-desktop/.../voicetastic-core` build script needs it).

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-pack          # if not already installed
wasm-pack build --target web --out-dir pkg --dev
```

The `wasm32` build cfgs (`getrandom_backend`, `web_sys_unstable_apis`) live in
[.cargo/config.toml](.cargo/config.toml).

> **Path dependency.** `voicetastic-core` is referenced by relative path
> (`../voicetastic-desktop/crates/voicetastic-core`), so this repo must sit beside a
> checkout of `voicetastic-desktop`. Switch to a git dependency for CI.

## Run the gate against a radio

Web Serial needs a secure context; `localhost` qualifies, so a plain static server works:

```sh
python3 -m http.server 8080
```

Open <http://localhost:8080>, plug in a Meshtastic radio, click **Connect & read node
info**, and pick the port. On success the page shows the node number; the browser
console logs each step. Needs Chrome/Edge or Firefox 151+.

## Next steps

- Make core's `Transport` trait `?Send` on `wasm32` and add a `spawn_local` runtime
  driver, so `connect_with_transport` (and the full config/message machinery) runs in
  the browser. This replaces the hand-rolled read loop here.
- Audio I/O via Web Audio / AudioWorklet.
- Codec2 as a JS-side WASM module (PCM crosses the boundary).
- The SPA shell (Vite) on top of these bindings.
