//! Web Bluetooth transport for Meshtastic radios.
//!
//! Mirrors the serial path in `crate::serial` + the read loop in
//! `crate::lib`: opens a GATT connection, grabs the three Meshtastic
//! characteristics, and provides a polling read loop that drains
//! `fromRadio` to feed into `protocol::decode_inbound`.
//!
//! **Polling, not notifications.** The Meshtastic GATT spec defines a
//! `fromNum` characteristic that notifies when new data is available,
//! but threading a `Closure` callback through wasm-bindgen and back into
//! async-Rust requires extra crates (futures-channel) and lifetime
//! gymnastics for the listener. Polling `fromRadio` at ~100 ms is the
//! simpler v1 — the firmware returns an empty `FromRadio` when there's
//! nothing pending so the cost is one cheap read per tick. Notification
//! mode can land later without changing the public API.
//!
//! **Web Bluetooth caveats.** Chromium-only today (Chrome, Edge, Opera).
//! Firefox + Safari fall back to Web Serial. The browser also requires
//! a user gesture for `navigator.bluetooth.requestDevice` (same as Web
//! Serial), so `connect_ble` must be called from a click handler.
//!
//! **Framing.** Unlike the serial transport, BLE writes are complete
//! protobuf-encoded `ToRadio` messages — no `0x94 0xc3 + length` prefix
//! is added on this path. Each GATT write maps to one `ToRadio`; each
//! GATT read maps to (at most) one `FromRadio`.

use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use crate::util::err;

/// Meshtastic primary GATT service UUID. Mirrors the constant in the
/// firmware (see `MeshService.cpp`).
pub const MESHTASTIC_SERVICE_UUID: &str = "6ba1b218-15a8-461f-9fa8-5dcae273eafd";

/// `toRadio` — client → device, one `ToRadio` proto per write. Write
/// without response is what the firmware expects; we use plain
/// `write_value_with_*` which the browser will pick the best mode for.
pub const TO_RADIO_CHARACTERISTIC: &str = "f75c76d2-129e-4dad-a1dd-7866124401e7";

/// `fromRadio` — device → client. Each read returns at most one
/// complete `FromRadio` proto; an empty buffer means "nothing
/// pending, try later."
pub const FROM_RADIO_CHARACTERISTIC: &str = "2c55e69e-4993-11ed-b878-0242ac120002";

/// `fromNum` — increments every time `fromRadio` has new data. We
/// keep the UUID here for the future notification-based read loop;
/// the v1 polling driver doesn't subscribe to it.
#[allow(dead_code)]
pub const FROM_NUM_CHARACTERISTIC: &str = "ed9da18c-a800-4f66-a670-aa7547e34453";

/// One-stop "open the GATT connection" helper. Pops the browser's
/// device picker (filtered to the Meshtastic service UUID), connects
/// GATT, and returns the three handles the rest of the driver needs.
///
/// Must be called from a user gesture (the picker requires it). The
/// `device` handle is returned so the caller can register a
/// `gattserverdisconnected` listener or call `disconnect()` later.
pub async fn open() -> Result<BleHandles, JsValue> {
    let window = web_sys::window().ok_or_else(|| err("no window"))?;
    let bt = window.navigator().bluetooth().ok_or_else(|| {
        err(
            "Web Bluetooth not available — use Chrome/Edge/Opera on a \
             machine with BLE, or fall back to USB serial",
        )
    })?;

    // Filter the picker to devices advertising the Meshtastic service.
    let filters = js_sys::Array::new();
    let filter = js_sys::Object::new();
    let services = js_sys::Array::new();
    services.push(&JsValue::from_str(MESHTASTIC_SERVICE_UUID));
    js_sys::Reflect::set(&filter, &"services".into(), &services).unwrap();
    filters.push(&filter);
    let opts = js_sys::Object::new();
    js_sys::Reflect::set(&opts, &"filters".into(), &filters).unwrap();
    // optionalServices is required when filters don't include all
    // services we plan to access, but here filters already cover it;
    // keep the field explicit so future characteristics can be added
    // without re-prompting.
    js_sys::Reflect::set(&opts, &"optionalServices".into(), &services).unwrap();
    let opts: web_sys::RequestDeviceOptions = opts.unchecked_into();

    let device: web_sys::BluetoothDevice = JsFuture::from(bt.request_device(&opts))
        .await?
        .dyn_into()?;

    let chars = discover_chars(&device).await?;
    Ok(BleHandles {
        device,
        from_radio: chars.from_radio,
        to_radio: chars.to_radio,
        from_num: chars.from_num,
    })
}

/// Connect (or reconnect) to an already-known device, re-discover the
/// Meshtastic service, and return fresh characteristic handles. Used
/// by the initial `open()` path and by the reconnect campaign after
/// `gattserverdisconnected`. Doesn't pop the picker — the caller
/// must already have a `BluetoothDevice` reference (which means a
/// previous successful `open()` in the same tab session, or the
/// browser remembered the permission for this origin).
pub async fn discover_chars(
    device: &web_sys::BluetoothDevice,
) -> Result<BleChars, JsValue> {
    let gatt = device
        .gatt()
        .ok_or_else(|| err("device has no GATT server"))?;
    let server: web_sys::BluetoothRemoteGattServer =
        JsFuture::from(gatt.connect()).await?.dyn_into()?;
    let service: web_sys::BluetoothRemoteGattService = JsFuture::from(
        server.get_primary_service_with_str(MESHTASTIC_SERVICE_UUID),
    )
    .await?
    .dyn_into()?;
    let from_radio: web_sys::BluetoothRemoteGattCharacteristic = JsFuture::from(
        service.get_characteristic_with_str(FROM_RADIO_CHARACTERISTIC),
    )
    .await?
    .dyn_into()?;
    let to_radio: web_sys::BluetoothRemoteGattCharacteristic = JsFuture::from(
        service.get_characteristic_with_str(TO_RADIO_CHARACTERISTIC),
    )
    .await?
    .dyn_into()?;
    // `fromNum` increments each time `fromRadio` has new data; subscribing
    // to its `characteristicvaluechanged` event lets the driver run on
    // notifications instead of polling. The characteristic itself is
    // optional on older firmware — keep the connect path going either way
    // and the caller can detect `from_num.is_none()` to fall back to polling.
    let from_num: Option<web_sys::BluetoothRemoteGattCharacteristic> = JsFuture::from(
        service.get_characteristic_with_str(FROM_NUM_CHARACTERISTIC),
    )
    .await
    .ok()
    .and_then(|v| v.dyn_into().ok());

    Ok(BleChars {
        from_radio,
        to_radio,
        from_num,
    })
}

/// The three characteristic handles obtained by service discovery.
/// Re-acquired on each reconnect because Web Bluetooth invalidates
/// `BluetoothRemoteGattCharacteristic` instances after a disconnect.
pub struct BleChars {
    pub from_radio: web_sys::BluetoothRemoteGattCharacteristic,
    pub to_radio: web_sys::BluetoothRemoteGattCharacteristic,
    pub from_num: Option<web_sys::BluetoothRemoteGattCharacteristic>,
}

/// Handles to the three Meshtastic GATT endpoints. Held on `Inner` so
/// the connection survives across `await` points (dropping the
/// characteristic objects would not close the GATT link, but the JS
/// proxies they reference would be GC'd).
pub struct BleHandles {
    pub device: web_sys::BluetoothDevice,
    pub from_radio: web_sys::BluetoothRemoteGattCharacteristic,
    pub to_radio: web_sys::BluetoothRemoteGattCharacteristic,
    /// `fromNum` notify characteristic — present on every Meshtastic
    /// firmware that knows about Web Bluetooth, but optional in this
    /// API in case some hand-rolled firmware lacks it. When `None` the
    /// driver falls back to polling `fromRadio`.
    pub from_num: Option<web_sys::BluetoothRemoteGattCharacteristic>,
}

/// Read one `FromRadio` chunk via the GATT `fromRadio` characteristic.
/// Returns `Ok(None)` when the firmware reported "nothing pending"
/// (empty buffer); `Ok(Some(bytes))` otherwise. The caller is
/// responsible for back-off / polling cadence.
pub async fn read_from_radio(
    from_radio: &web_sys::BluetoothRemoteGattCharacteristic,
) -> Result<Option<Vec<u8>>, JsValue> {
    let view = JsFuture::from(from_radio.read_value()).await?;
    // `read_value()` resolves to a `DataView`. Pull the underlying
    // buffer + byteOffset + byteLength out manually because web-sys
    // doesn't have a typed accessor.
    let buffer = js_sys::Reflect::get(&view, &"buffer".into())?;
    let byte_offset = js_sys::Reflect::get(&view, &"byteOffset".into())?
        .as_f64()
        .unwrap_or(0.0) as u32;
    let byte_length = js_sys::Reflect::get(&view, &"byteLength".into())?
        .as_f64()
        .unwrap_or(0.0) as u32;
    if byte_length == 0 {
        return Ok(None);
    }
    let arr = js_sys::Uint8Array::new_with_byte_offset_and_length(&buffer, byte_offset, byte_length);
    let mut out = vec![0u8; byte_length as usize];
    arr.copy_to(&mut out);
    Ok(Some(out))
}

/// Write one already-encoded `ToRadio` proto via the GATT
/// `toRadio` characteristic. The firmware tolerates either write-mode
/// (with-response or without); we use the default which the browser
/// picks based on the characteristic's properties.
pub async fn write_to_radio(
    to_radio: &web_sys::BluetoothRemoteGattCharacteristic,
    bytes: &[u8],
) -> Result<(), JsValue> {
    let arr = js_sys::Uint8Array::from(bytes);
    JsFuture::from(to_radio.write_value_with_buffer_source(arr.as_ref())?).await?;
    Ok(())
}
