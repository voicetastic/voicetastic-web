//! Small wasm/JS helpers used across the crate. None of these hold state,
//! none of them touch protocol or codec internals — they're the glue we'd
//! otherwise reinvent in every module.

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// `console.log(s)` from Rust. `web_sys::console::log_1` wants a `JsValue`,
/// so this is the one-liner everyone wants.
pub fn log(s: &str) {
    web_sys::console::log_1(&JsValue::from_str(s));
}

/// Wrap a string in a `JsValue` for use as a Promise rejection reason.
pub fn err(s: &str) -> JsValue {
    JsValue::from_str(s)
}

/// Cryptographically-random `u32` via the platform RNG (browser `crypto.getRandomValues`).
pub fn rand_u32() -> u32 {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    u32::from_le_bytes(b)
}

/// Await `ms` milliseconds via `setTimeout` (no tokio on wasm).
pub async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        if let Some(win) = web_sys::window() {
            let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = JsFuture::from(promise).await;
}
