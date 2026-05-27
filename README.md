# voicetastic-web

Browser (WASM) client for [Voicetastic](https://git.cha-sam.re/voicetastic) — async
voice messages over Meshtastic, with **no install**: the radio plugs into the user's
own machine and the page talks to it directly over **Web Serial**.

This reuses the desktop engine (`voicetastic-core`) compiled to `wasm32`, rather than
reimplementing the protocol. See the architecture notes below.

## Status: connectivity gate

This repo currently contains a **single proof-of-connectivity gate**, not the full
client. `connect_and_read_my_node_info()`:

1. opens a user-selected serial port (Web Serial),
2. sends a `WantConfigId` `ToRadio`, framed with the `0x94 0xc3` Meshtastic serial header,
3. reads + deframes the inbound byte stream,
4. decodes each `FromRadio` and resolves with the node number once `MyNodeInfo` arrives.

It reuses `voicetastic-core`'s protobuf types (`proto::ToRadio` / `proto::FromRadio`)
for encode/decode. It deliberately does **not** yet use core's `Transport` trait or
`connect_with_transport` — those require `Send` (browser JS handles are `!Send`) and a
*driven* tokio runtime. Isolating the Web Serial + wire-framing + protobuf risk first
keeps the gate trustworthy; the core-`Transport` integration is the next step (below).

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
