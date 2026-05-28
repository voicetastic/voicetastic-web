//! Settings IO for the browser client — `WebClient.snapshot()` for reading
//! the current device config + identity, and per-section write methods
//! (`writeOwner`, `writeChannel`, `writeConfig*`, `setFixedPosition`) that
//! build admin messages via core's `protocol::admin_packet`.
//!
//! DTOs mirror the proto types' editable fields. We treat the *current*
//! ProtocolState as the base for each write: the DTO is overlaid on top, so
//! fields the UI doesn't expose stay at the device's current value rather
//! than reverting to proto defaults.

use serde::{Deserialize, Serialize};

use voicetastic_core::proto::{
    Channel, ChannelSettings, Config, Position, User, admin_message, config,
};
use voicetastic_core::protocol::ProtocolState;

// ---------- read-side snapshot ----------

#[derive(Serialize, Default)]
pub(crate) struct Snapshot {
    pub my_node_num: Option<u32>,
    pub fw: Option<String>,
    pub owner: Option<OwnerDto>,
    pub lora: Option<LoraDto>,
    pub device: Option<DeviceDto>,
    pub position: Option<PositionDto>,
    pub power: Option<PowerDto>,
    pub network: Option<NetworkDto>,
    pub display: Option<DisplayDto>,
    pub bluetooth: Option<BluetoothDto>,
    pub channels: Vec<ChannelDto>,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct OwnerDto {
    pub long_name: String,
    pub short_name: String,
    pub is_licensed: bool,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct LoraDto {
    pub use_preset: bool,
    pub modem_preset: i32,
    pub region: i32,
    pub hop_limit: u32,
    pub tx_power: i32,
    pub tx_enabled: bool,
    pub ignore_mqtt: bool,
    pub channel_num: u32,
    pub bandwidth: u32,
    pub spread_factor: u32,
    pub coding_rate: u32,
    pub frequency_offset: f32,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct DeviceDto {
    pub role: i32,
    pub rebroadcast_mode: i32,
    pub node_info_broadcast_secs: u32,
    pub double_tap_as_button_press: bool,
    pub disable_triple_click: bool,
    pub button_gpio: u32,
    pub buzzer_gpio: u32,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct PositionDto {
    pub position_broadcast_secs: u32,
    pub position_broadcast_smart_enabled: bool,
    pub fixed_position: bool,
    pub gps_enabled: bool,
    pub gps_update_interval: u32,
    pub broadcast_smart_minimum_distance: u32,
    pub broadcast_smart_minimum_interval_secs: u32,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct PowerDto {
    pub is_power_saving: bool,
    pub on_battery_shutdown_after_secs: u32,
    pub wait_bluetooth_secs: u32,
    pub sds_secs: u32,
    pub ls_secs: u32,
    pub min_wake_secs: u32,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct NetworkDto {
    pub wifi_enabled: bool,
    pub wifi_ssid: String,
    pub wifi_psk: String,
    pub eth_enabled: bool,
    pub address_mode: i32,
    pub ntp_server: String,
    pub rsyslog_server: String,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct DisplayDto {
    pub screen_on_secs: u32,
    pub auto_screen_carousel_secs: u32,
    pub units: i32,
    pub oled: i32,
    pub displaymode: i32,
    pub flip_screen: bool,
    pub heading_bold: bool,
    pub wake_on_tap_or_motion: bool,
    pub use_12h_clock: bool,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct BluetoothDto {
    pub enabled: bool,
    pub mode: i32,
    pub fixed_pin: u32,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct ChannelDto {
    pub index: i32,
    pub role: i32,
    pub name: String,
    pub uplink_enabled: bool,
    pub downlink_enabled: bool,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct FixedPositionDto {
    pub latitude_i: i32,
    pub longitude_i: i32,
    pub altitude: i32,
}

// ---------- builders: ProtocolState -> Snapshot ----------

pub(crate) fn build_snapshot(state: &ProtocolState) -> Snapshot {
    Snapshot {
        my_node_num: state.my_info.as_ref().map(|i| i.my_node_num),
        fw: state.metadata.as_ref().map(|m| m.firmware_version.clone()),
        owner: state.owner.as_ref().map(owner_to_dto),
        lora: state.lora.as_ref().map(lora_to_dto),
        device: state.device.as_ref().map(device_to_dto),
        position: state.position.as_ref().map(position_to_dto),
        power: state.power.as_ref().map(power_to_dto),
        network: state.network.as_ref().map(network_to_dto),
        display: state.display.as_ref().map(display_to_dto),
        bluetooth: state.bluetooth.as_ref().map(bluetooth_to_dto),
        channels: state.channels.iter().map(channel_to_dto).collect(),
    }
}

fn owner_to_dto(u: &User) -> OwnerDto {
    OwnerDto {
        long_name: u.long_name.clone(),
        short_name: u.short_name.clone(),
        is_licensed: u.is_licensed,
    }
}

fn lora_to_dto(c: &config::LoRaConfig) -> LoraDto {
    LoraDto {
        use_preset: c.use_preset,
        modem_preset: c.modem_preset,
        region: c.region,
        hop_limit: c.hop_limit,
        tx_power: c.tx_power,
        tx_enabled: c.tx_enabled,
        ignore_mqtt: c.ignore_mqtt,
        channel_num: c.channel_num,
        bandwidth: c.bandwidth,
        spread_factor: c.spread_factor,
        coding_rate: c.coding_rate,
        frequency_offset: c.frequency_offset,
    }
}

fn device_to_dto(c: &config::DeviceConfig) -> DeviceDto {
    DeviceDto {
        role: c.role,
        rebroadcast_mode: c.rebroadcast_mode,
        node_info_broadcast_secs: c.node_info_broadcast_secs,
        double_tap_as_button_press: c.double_tap_as_button_press,
        disable_triple_click: c.disable_triple_click,
        button_gpio: c.button_gpio,
        buzzer_gpio: c.buzzer_gpio,
    }
}

fn position_to_dto(c: &config::PositionConfig) -> PositionDto {
    // `gps_enabled` (bool) was superseded by `gps_mode` (enum: Disabled,
    // Enabled, NotPresent) in firmware. The DTO surface still exposes a
    // bool for JS simplicity — true iff gps_mode == Enabled.
    let gps_enabled = c.gps_mode == config::position_config::GpsMode::Enabled as i32;
    PositionDto {
        position_broadcast_secs: c.position_broadcast_secs,
        position_broadcast_smart_enabled: c.position_broadcast_smart_enabled,
        fixed_position: c.fixed_position,
        gps_enabled,
        gps_update_interval: c.gps_update_interval,
        broadcast_smart_minimum_distance: c.broadcast_smart_minimum_distance,
        broadcast_smart_minimum_interval_secs: c.broadcast_smart_minimum_interval_secs,
    }
}

fn power_to_dto(c: &config::PowerConfig) -> PowerDto {
    PowerDto {
        is_power_saving: c.is_power_saving,
        on_battery_shutdown_after_secs: c.on_battery_shutdown_after_secs,
        wait_bluetooth_secs: c.wait_bluetooth_secs,
        sds_secs: c.sds_secs,
        ls_secs: c.ls_secs,
        min_wake_secs: c.min_wake_secs,
    }
}

fn network_to_dto(c: &config::NetworkConfig) -> NetworkDto {
    NetworkDto {
        wifi_enabled: c.wifi_enabled,
        wifi_ssid: c.wifi_ssid.clone(),
        // Don't surface the PSK on read — the radio reports it but the UI
        // doesn't need to echo it back. The write path takes a new value or
        // leaves it as the device currently has it (we overlay on top of the
        // existing config).
        wifi_psk: String::new(),
        eth_enabled: c.eth_enabled,
        address_mode: c.address_mode,
        ntp_server: c.ntp_server.clone(),
        rsyslog_server: c.rsyslog_server.clone(),
    }
}

fn display_to_dto(c: &config::DisplayConfig) -> DisplayDto {
    DisplayDto {
        screen_on_secs: c.screen_on_secs,
        auto_screen_carousel_secs: c.auto_screen_carousel_secs,
        units: c.units,
        oled: c.oled,
        displaymode: c.displaymode,
        flip_screen: c.flip_screen,
        heading_bold: c.heading_bold,
        wake_on_tap_or_motion: c.wake_on_tap_or_motion,
        use_12h_clock: c.use_12h_clock,
    }
}

fn bluetooth_to_dto(c: &config::BluetoothConfig) -> BluetoothDto {
    BluetoothDto {
        enabled: c.enabled,
        mode: c.mode,
        fixed_pin: c.fixed_pin,
    }
}

fn channel_to_dto(c: &Channel) -> ChannelDto {
    let s = c.settings.as_ref();
    ChannelDto {
        index: c.index,
        role: c.role,
        name: s.map(|s| s.name.clone()).unwrap_or_default(),
        uplink_enabled: s.map(|s| s.uplink_enabled).unwrap_or(false),
        downlink_enabled: s.map(|s| s.downlink_enabled).unwrap_or(false),
    }
}

// ---------- write-side admin payloads ----------
//
// Each helper builds the admin_message::PayloadVariant the web driver will
// pass to `protocol::admin_packet`. The "overlay on current" pattern (start
// from the live ProtocolState, copy fields from the DTO) means unedited
// fields of each config section keep their device-reported value rather than
// reverting to proto defaults — the same effect as desktop's dirty-tracking
// for the editable subset.

pub(crate) fn owner_payload(state: &ProtocolState, dto: OwnerDto) -> admin_message::PayloadVariant {
    let base = state.owner.clone().unwrap_or_default();
    admin_message::PayloadVariant::SetOwner(User {
        long_name: dto.long_name,
        short_name: dto.short_name,
        is_licensed: dto.is_licensed,
        ..base
    })
}

pub(crate) fn lora_payload(state: &ProtocolState, dto: LoraDto) -> admin_message::PayloadVariant {
    let base = state.lora.clone().unwrap_or_default();
    let updated = config::LoRaConfig {
        use_preset: dto.use_preset,
        modem_preset: dto.modem_preset,
        region: dto.region,
        hop_limit: dto.hop_limit,
        tx_power: dto.tx_power,
        tx_enabled: dto.tx_enabled,
        ignore_mqtt: dto.ignore_mqtt,
        channel_num: dto.channel_num,
        bandwidth: dto.bandwidth,
        spread_factor: dto.spread_factor,
        coding_rate: dto.coding_rate,
        frequency_offset: dto.frequency_offset,
        ..base
    };
    admin_message::PayloadVariant::SetConfig(Config {
        payload_variant: Some(config::PayloadVariant::Lora(updated)),
    })
}

pub(crate) fn device_payload(
    state: &ProtocolState,
    dto: DeviceDto,
) -> admin_message::PayloadVariant {
    let base = state.device.clone().unwrap_or_default();
    let updated = config::DeviceConfig {
        role: dto.role,
        rebroadcast_mode: dto.rebroadcast_mode,
        node_info_broadcast_secs: dto.node_info_broadcast_secs,
        double_tap_as_button_press: dto.double_tap_as_button_press,
        disable_triple_click: dto.disable_triple_click,
        button_gpio: dto.button_gpio,
        buzzer_gpio: dto.buzzer_gpio,
        ..base
    };
    admin_message::PayloadVariant::SetConfig(Config {
        payload_variant: Some(config::PayloadVariant::Device(updated)),
    })
}

pub(crate) fn position_payload(
    state: &ProtocolState,
    dto: PositionDto,
) -> admin_message::PayloadVariant {
    let base = state.position.unwrap_or_default();
    // Translate the DTO's bool back to gps_mode. The legacy `gps_enabled`
    // bool isn't set explicitly — `..base` carries whatever the radio
    // last reported, and firmware reads gps_mode regardless.
    let gps_mode = if dto.gps_enabled {
        config::position_config::GpsMode::Enabled as i32
    } else {
        config::position_config::GpsMode::Disabled as i32
    };
    let updated = config::PositionConfig {
        position_broadcast_secs: dto.position_broadcast_secs,
        position_broadcast_smart_enabled: dto.position_broadcast_smart_enabled,
        fixed_position: dto.fixed_position,
        gps_mode,
        gps_update_interval: dto.gps_update_interval,
        broadcast_smart_minimum_distance: dto.broadcast_smart_minimum_distance,
        broadcast_smart_minimum_interval_secs: dto.broadcast_smart_minimum_interval_secs,
        ..base
    };
    admin_message::PayloadVariant::SetConfig(Config {
        payload_variant: Some(config::PayloadVariant::Position(updated)),
    })
}

pub(crate) fn power_payload(state: &ProtocolState, dto: PowerDto) -> admin_message::PayloadVariant {
    let base = state.power.unwrap_or_default();
    let updated = config::PowerConfig {
        is_power_saving: dto.is_power_saving,
        on_battery_shutdown_after_secs: dto.on_battery_shutdown_after_secs,
        wait_bluetooth_secs: dto.wait_bluetooth_secs,
        sds_secs: dto.sds_secs,
        ls_secs: dto.ls_secs,
        min_wake_secs: dto.min_wake_secs,
        ..base
    };
    admin_message::PayloadVariant::SetConfig(Config {
        payload_variant: Some(config::PayloadVariant::Power(updated)),
    })
}

pub(crate) fn network_payload(
    state: &ProtocolState,
    dto: NetworkDto,
) -> admin_message::PayloadVariant {
    let base = state.network.clone().unwrap_or_default();
    // Empty PSK in the DTO means "keep current" rather than "clear PSK".
    let wifi_psk = if dto.wifi_psk.is_empty() {
        base.wifi_psk.clone()
    } else {
        dto.wifi_psk
    };
    let updated = config::NetworkConfig {
        wifi_enabled: dto.wifi_enabled,
        wifi_ssid: dto.wifi_ssid,
        wifi_psk,
        eth_enabled: dto.eth_enabled,
        address_mode: dto.address_mode,
        ntp_server: dto.ntp_server,
        rsyslog_server: dto.rsyslog_server,
        ..base
    };
    admin_message::PayloadVariant::SetConfig(Config {
        payload_variant: Some(config::PayloadVariant::Network(updated)),
    })
}

pub(crate) fn display_payload(
    state: &ProtocolState,
    dto: DisplayDto,
) -> admin_message::PayloadVariant {
    let base = state.display.unwrap_or_default();
    let updated = config::DisplayConfig {
        screen_on_secs: dto.screen_on_secs,
        auto_screen_carousel_secs: dto.auto_screen_carousel_secs,
        units: dto.units,
        oled: dto.oled,
        displaymode: dto.displaymode,
        flip_screen: dto.flip_screen,
        heading_bold: dto.heading_bold,
        wake_on_tap_or_motion: dto.wake_on_tap_or_motion,
        use_12h_clock: dto.use_12h_clock,
        ..base
    };
    admin_message::PayloadVariant::SetConfig(Config {
        payload_variant: Some(config::PayloadVariant::Display(updated)),
    })
}

pub(crate) fn bluetooth_payload(
    _state: &ProtocolState,
    dto: BluetoothDto,
) -> admin_message::PayloadVariant {
    // BluetoothConfig has only the three fields in the DTO; no need to overlay
    // anything from the current config.
    let updated = config::BluetoothConfig {
        enabled: dto.enabled,
        mode: dto.mode,
        fixed_pin: dto.fixed_pin,
    };
    admin_message::PayloadVariant::SetConfig(Config {
        payload_variant: Some(config::PayloadVariant::Bluetooth(updated)),
    })
}

pub(crate) fn channel_payload(state: &ProtocolState, dto: ChannelDto) -> admin_message::PayloadVariant {
    // Find the existing channel at that index so we preserve PSK + module
    // settings + any other fields the UI doesn't expose.
    let existing = state.channels.iter().find(|c| c.index == dto.index).cloned();
    let base_settings = existing
        .and_then(|c| c.settings)
        .unwrap_or_default();
    let updated = Channel {
        index: dto.index,
        role: dto.role,
        settings: Some(ChannelSettings {
            name: dto.name,
            uplink_enabled: dto.uplink_enabled,
            downlink_enabled: dto.downlink_enabled,
            ..base_settings
        }),
    };
    admin_message::PayloadVariant::SetChannel(updated)
}

pub(crate) fn fixed_position_payload(dto: FixedPositionDto) -> admin_message::PayloadVariant {
    admin_message::PayloadVariant::SetFixedPosition(Position {
        latitude_i: Some(dto.latitude_i),
        longitude_i: Some(dto.longitude_i),
        altitude: Some(dto.altitude),
        ..Default::default()
    })
}

// =============================================================================
// WebClient wasm-bindgen surface — read snapshot + 9 per-section write methods.
//
// Each writer is a thin Promise that:
//   1. deserialises the JS-side DTO via `serde_wasm_bindgen`
//   2. hands it to the matching `*_payload` builder above (overlays it on the
//      current `ProtocolState` so fields the DTO doesn't carry keep their
//      device-reported value — the same effect desktop's dirty-tracking gives)
//   3. ships the resulting admin message via `Inner::send_admin`
//
// The shape is identical across sections — eight of the nine writes go through
// the `write_config!` macro to make that uniformity legible. `setFixedPosition`
// is the odd one out (no state overlay; it's a one-shot location set) so it
// stays a literal definition.
// =============================================================================

use crate::WebClient;
use crate::util::err;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

/// One-section setter: deserialise DTO, build the payload, send_admin.
/// Expands to a full `impl WebClient { ... }` block — multiple impl blocks
/// for the same type are fine, and Rust doesn't allow macros directly
/// inside an `impl` body.
macro_rules! write_config {
    ($rust:ident, $js:ident, $dto:ident, $payload:path, $tag:literal) => {
        #[wasm_bindgen]
        impl WebClient {
            #[wasm_bindgen(js_name = $js)]
            pub fn $rust(&self, dto: JsValue) -> js_sys::Promise {
                let inner = self.inner.clone();
                future_to_promise(async move {
                    let dto: $dto = serde_wasm_bindgen::from_value(dto)
                        .map_err(|e| err(&format!(concat!($tag, " dto: {}"), e)))?;
                    let payload = $payload(&inner.state.borrow(), dto);
                    inner.send_admin(payload).await?;
                    Ok(JsValue::UNDEFINED)
                })
            }
        }
    };
}

write_config!(write_owner,            writeOwner,            OwnerDto,     owner_payload,     "owner");
write_config!(write_lora_config,      writeLoraConfig,       LoraDto,      lora_payload,      "lora");
write_config!(write_device_config,    writeDeviceConfig,     DeviceDto,    device_payload,    "device");
write_config!(write_position_config,  writePositionConfig,   PositionDto,  position_payload,  "position");
write_config!(write_power_config,     writePowerConfig,      PowerDto,     power_payload,     "power");
write_config!(write_network_config,   writeNetworkConfig,    NetworkDto,   network_payload,   "network");
write_config!(write_display_config,   writeDisplayConfig,    DisplayDto,   display_payload,   "display");
write_config!(write_bluetooth_config, writeBluetoothConfig,  BluetoothDto, bluetooth_payload, "bluetooth");
write_config!(write_channel,          writeChannel,          ChannelDto,   channel_payload,   "channel");

#[wasm_bindgen]
impl WebClient {
    /// Snapshot of the device identity + the eight config sections + channels,
    /// as a plain JS object (via serde-wasm-bindgen). Fields the radio hasn't
    /// reported yet are `null`. Mirrors what `MeshtasticService::watch_*` give
    /// the desktop GUI.
    #[wasm_bindgen(js_name = snapshot)]
    pub fn snapshot(&self) -> Result<JsValue, JsValue> {
        let snap = build_snapshot(&self.inner.state.borrow());
        serde_wasm_bindgen::to_value(&snap).map_err(|e| err(&format!("snapshot: {e}")))
    }

    /// One-shot fixed-position write — no state overlay because the device
    /// only stores the lat/lon/alt triple (not a richer config).
    #[wasm_bindgen(js_name = setFixedPosition)]
    pub fn set_fixed_position(&self, dto: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let dto: FixedPositionDto = serde_wasm_bindgen::from_value(dto)
                .map_err(|e| err(&format!("fixed position dto: {e}")))?;
            let payload = fixed_position_payload(dto);
            inner.send_admin(payload).await?;
            Ok(JsValue::UNDEFINED)
        })
    }
}
