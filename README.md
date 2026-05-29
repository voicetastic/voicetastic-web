# voicetastic-web

Browser (WASM) client for [Voicetastic](https://github.com/voicetastic) — async
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

Voice messaging works, reusing core's voice pipeline — and now has reliability parity
with the desktop client:

- **Codecs from core.** Codec2 (pure-Rust `codec2` crate, the LoRa-optimal default),
  AMR-NB, and Opus all run as `voicetastic_core::codec::*` — one codec implementation
  shared with desktop/Android, no JS codec modules, no codec code in this crate.
- **TX** (`WebClient.sendVoice`): mic PCM → optional RNNoise denoise (core's `Denoiser`)
  → selected codec encode → core `build_message` → per-frame pacing via `voice::tx_policy`
  + firmware queue backpressure → PRIVATE_APP frames. Modem-preset FEC parity is applied
  via `VoiceFecMode::Auto` (same policy as desktop).
- **RX**: PRIVATE_APP frames → core's sans-IO `VoiceAssembler` → on completion, decoded
  with the matching codec → PCM handed to the JS playback callback.
- **NACK loop**: an in-browser tick driver polls `VoiceAssembler::tick()` every 500 ms
  to emit RX-side NACKs, and inbound NACKs are serviced by core's `OutgoingVoiceRegistry`
  for paced retransmits — wire-compatible with the desktop driver.
- Mic capture + playback are Web Audio (the only JS-side audio glue); everything else is
  core's Rust.

Remaining limit: a single `sendVoice` call still ships one Meshtastic message (~13 s at
1200 bps Codec2; less at higher bitrates / Opus). Clips that exceed one message need to
be split — the multi-message sender is not in core yet.

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

The big architectural moves are done: core was refactored sans-IO, so the browser is a
real driver (not a bridge) over the same `protocol` / `voice` modules the desktop and
Android clients use. Open work:

- **Persistent ports.** `navigator.serial.getPorts()` returns previously-granted ports
  without a picker — wire auto-reconnect for sessions where the user already approved a
  device.
- **AudioWorklet capture.** Mic capture currently uses the deprecated
  `ScriptProcessorNode` (main-thread audio). Move to `AudioWorkletNode` for the same
  latency story the rest of the audio stack already has.
- **Multi-message voice clips.** Lift the one-message cap so longer recordings split
  cleanly into a sequence the receiver reassembles in order. Belongs in core
  (`voice::*`), shared with desktop/Android.
- **Deploy story.** Switch the `voicetastic-core` path dep to a git dep so CI and a
  hosted static build don't need the sibling `voicetastic-desktop` checkout.
