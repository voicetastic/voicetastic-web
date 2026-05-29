// Settings page: device config sections (Owner / LoRa / Device / …),
// per-channel settings, fixed-position coordinates, and the Audio
// category (codec / bitrate / denoise toggles, all client-side).
//
// Each Meshtastic section is data-driven via the SECTIONS array. The
// Audio section is hand-written HTML in index.html — this file just
// wires its change handlers to the wasm setters.

import { state } from './state.js';
import { log } from './ui.js';
import { waitForApplyConfirm } from './events.js';

// ---------- enum tables ----------
//
// Code numbers are the wire values the firmware expects; labels are
// what the user sees. Keep these mirroring the proto enums in
// `proto/meshtastic/config.proto`.

const MODEM_PRESETS = [
  [0, 'LongFast'], [1, 'LongSlow'], [2, 'VeryLongSlow'], [3, 'MediumSlow'],
  [4, 'MediumFast'], [5, 'ShortSlow'], [6, 'ShortFast'], [7, 'LongModerate'],
  [8, 'ShortTurbo'],
];
const LORA_REGIONS = [
  [0, 'Unset'], [1, 'US'], [2, 'EU 433 MHz'], [3, 'EU 868 MHz'],
  [4, 'China'], [5, 'Japan'], [6, 'Australia / NZ'], [7, 'Korea'],
  [8, 'Taiwan'], [9, 'Russia'], [10, 'India'], [11, 'New Zealand 865 MHz'],
  [12, 'Thailand'], [13, 'LoRa 2.4 GHz (WLAN)'], [14, 'Ukraine 433 MHz'],
  [15, 'Ukraine 868 MHz'], [16, 'Malaysia 433 MHz'], [17, 'Malaysia 919 MHz'],
  [18, 'Singapore 923 MHz'], [19, 'Philippines 433 MHz'],
  [20, 'Philippines 868 MHz'], [21, 'Philippines 915 MHz'],
  [22, 'Australia / NZ 433 MHz'], [23, 'Kazakhstan 433 MHz'],
  [24, 'Kazakhstan 863 MHz'], [25, 'Nepal 865 MHz'], [26, 'Brazil 902 MHz'],
  [27, 'ITU Region 1 2 m (144–146 MHz)'],
  [28, 'ITU Region 2/3 2 m (144–148 MHz)'],
  [29, 'EU 866 MHz (SRD)'], [30, 'EU 874 MHz (SRD)'],
  [31, 'EU 917 MHz (SRD)'], [32, 'EU 868 MHz (narrow presets)'],
];
const DEVICE_ROLES = [
  [0, 'Client'], [1, 'ClientMute'], [2, 'Router'], [4, 'RouterClient'],
  [5, 'Repeater'], [6, 'Tracker'], [7, 'Sensor'], [8, 'TAK'],
  [9, 'ClientHidden'], [10, 'LostAndFound'], [11, 'TAKTracker'],
];
const BT_MODES = [[0, 'RandomPin'], [1, 'FixedPin'], [2, 'NoPin']];
const DISPLAY_UNITS = [[0, 'Metric'], [1, 'Imperial']];
const ADDR_MODES = [[0, 'DHCP'], [1, 'Static']];

const SECTIONS = [
  { key: 'owner', title: '👤 Owner', writeFn: 'writeOwner', fields: [
    { name: 'long_name', label: 'Long name', type: 'text' },
    { name: 'short_name', label: 'Short name (≤4)', type: 'text', maxlength: 4 },
    { name: 'is_licensed', label: 'Licensed (HAM)', type: 'bool' },
  ]},
  { key: 'lora', title: '📡 LoRa / Radio', writeFn: 'writeLoraConfig', fields: [
    { name: 'use_preset', label: 'Use modem preset', type: 'bool' },
    { name: 'modem_preset', label: 'Modem preset', type: 'enum', options: MODEM_PRESETS },
    { name: 'region', label: 'Region', type: 'enum', options: LORA_REGIONS },
    { name: 'hop_limit', label: 'Hop limit', type: 'int', min: 1, max: 7 },
    { name: 'tx_power', label: 'TX power (dBm)', type: 'int' },
    { name: 'tx_enabled', label: 'TX enabled', type: 'bool' },
    { name: 'ignore_mqtt', label: 'Ignore MQTT', type: 'bool' },
    { name: 'channel_num', label: 'Channel num', type: 'int' },
    { name: 'bandwidth', label: 'Bandwidth (manual)', type: 'int' },
    { name: 'spread_factor', label: 'Spread factor (manual)', type: 'int' },
    { name: 'coding_rate', label: 'Coding rate (manual)', type: 'int' },
    { name: 'frequency_offset', label: 'Frequency offset (Hz)', type: 'float' },
  ]},
  { key: 'device', title: '📱 Device', writeFn: 'writeDeviceConfig', fields: [
    { name: 'role', label: 'Role', type: 'enum', options: DEVICE_ROLES },
    { name: 'rebroadcast_mode', label: 'Rebroadcast mode (int)', type: 'int' },
    { name: 'node_info_broadcast_secs', label: 'NodeInfo broadcast (s)', type: 'int' },
    { name: 'double_tap_as_button_press', label: 'Double-tap as button', type: 'bool' },
    { name: 'disable_triple_click', label: 'Disable triple-click', type: 'bool' },
    { name: 'button_gpio', label: 'Button GPIO', type: 'int' },
    { name: 'buzzer_gpio', label: 'Buzzer GPIO', type: 'int' },
  ]},
  { key: 'position', title: '📍 Position', writeFn: 'writePositionConfig', fields: [
    { name: 'gps_enabled', label: 'GPS enabled', type: 'bool' },
    { name: 'fixed_position', label: 'Fixed position', type: 'bool' },
    { name: 'position_broadcast_secs', label: 'Broadcast interval (s)', type: 'int' },
    { name: 'gps_update_interval', label: 'GPS update interval (s)', type: 'int' },
    { name: 'position_broadcast_smart_enabled', label: 'Smart broadcast', type: 'bool' },
    { name: 'broadcast_smart_minimum_distance', label: 'Smart min distance (m)', type: 'int' },
    { name: 'broadcast_smart_minimum_interval_secs', label: 'Smart min interval (s)', type: 'int' },
  ]},
  { key: 'power', title: '🔋 Power', writeFn: 'writePowerConfig', fields: [
    { name: 'is_power_saving', label: 'Power saving', type: 'bool' },
    { name: 'on_battery_shutdown_after_secs', label: 'Shutdown on battery (s)', type: 'int' },
    { name: 'wait_bluetooth_secs', label: 'Wait Bluetooth (s)', type: 'int' },
    { name: 'sds_secs', label: 'SDS (s)', type: 'int' },
    { name: 'ls_secs', label: 'LS (s)', type: 'int' },
    { name: 'min_wake_secs', label: 'Min wake (s)', type: 'int' },
  ]},
  { key: 'network', title: '🌐 Network', writeFn: 'writeNetworkConfig', fields: [
    { name: 'wifi_enabled', label: 'Wi-Fi enabled', type: 'bool' },
    { name: 'wifi_ssid', label: 'Wi-Fi SSID', type: 'text' },
    { name: 'wifi_psk', label: 'Wi-Fi PSK (blank = keep)', type: 'text' },
    { name: 'eth_enabled', label: 'Ethernet enabled', type: 'bool' },
    { name: 'address_mode', label: 'Address mode', type: 'enum', options: ADDR_MODES },
    { name: 'ntp_server', label: 'NTP server', type: 'text' },
    { name: 'rsyslog_server', label: 'rsyslog server', type: 'text' },
  ]},
  { key: 'display', title: '🖥 Display', writeFn: 'writeDisplayConfig', fields: [
    { name: 'screen_on_secs', label: 'Screen on (s)', type: 'int' },
    { name: 'auto_screen_carousel_secs', label: 'Auto carousel (s)', type: 'int' },
    { name: 'units', label: 'Units', type: 'enum', options: DISPLAY_UNITS },
    { name: 'oled', label: 'OLED type (int)', type: 'int' },
    { name: 'displaymode', label: 'Display mode (int)', type: 'int' },
    { name: 'flip_screen', label: 'Flip screen', type: 'bool' },
    { name: 'heading_bold', label: 'Heading bold', type: 'bool' },
    { name: 'wake_on_tap_or_motion', label: 'Wake on tap/motion', type: 'bool' },
    { name: 'use_12h_clock', label: '12-hour clock', type: 'bool' },
  ]},
  { key: 'bluetooth', title: '🔵 Bluetooth', writeFn: 'writeBluetoothConfig', fields: [
    { name: 'enabled', label: 'Enabled', type: 'bool' },
    { name: 'mode', label: 'Pairing mode', type: 'enum', options: BT_MODES },
    { name: 'fixed_pin', label: 'Fixed PIN', type: 'int' },
  ]},
];

// ---------- per-field input + read ----------

function inputForField(f, value) {
  if (f.type === 'bool') {
    const i = document.createElement('input');
    i.type = 'checkbox'; i.name = f.name; i.checked = !!value;
    return i;
  }
  if (f.type === 'enum') {
    const sel = document.createElement('select');
    sel.name = f.name;
    for (const [v, label] of f.options) {
      const opt = document.createElement('option');
      opt.value = String(v); opt.textContent = `${label} (${v})`;
      if (Number(value) === Number(v)) opt.selected = true;
      sel.appendChild(opt);
    }
    return sel;
  }
  const i = document.createElement('input');
  i.type = f.type === 'text' ? 'text' : 'number';
  if (f.type === 'float') i.step = 'any';
  if (f.maxlength) i.maxLength = f.maxlength;
  if (f.min != null) i.min = f.min;
  if (f.max != null) i.max = f.max;
  i.name = f.name;
  i.value = value == null ? '' : String(value);
  return i;
}

function readField(f, el) {
  if (f.type === 'bool') return el.checked;
  if (f.type === 'enum' || f.type === 'int') return parseInt(el.value || '0', 10);
  if (f.type === 'float') return parseFloat(el.value || '0');
  return el.value;
}

// ---------- section card render ----------

function renderSection(section, sectionValue, snap) {
  const card = document.createElement('details');
  card.className = 'setcard';
  card.dataset.section = section.key;
  const hdr = document.createElement('summary');
  hdr.innerHTML = `<h3>${section.title}</h3>`;
  if (!sectionValue) {
    const h = document.createElement('span');
    h.className = 'hint'; h.textContent = 'not received yet';
    hdr.appendChild(h);
    card.classList.add('disabled');
  }
  const apply = document.createElement('button');
  apply.className = 'apply'; apply.textContent = 'Apply';
  apply.addEventListener('click', (e) => e.stopPropagation());
  // Map a settings section to the protocol event we expect back from
  // the radio after a successful SetOwner / SetConfig.
  const confirmKey = section.key === 'owner' ? 'owner' : `config:${section.key}`;
  apply.onclick = async (e) => {
    e.stopPropagation();
    const dto = {};
    for (const f of section.fields) {
      const el = card.querySelector(`[name="${f.name}"]`);
      dto[f.name] = el ? readField(f, el) : (f.type === 'bool' ? false : f.type === 'text' ? '' : 0);
    }
    apply.disabled = true;
    const prev = apply.textContent;
    apply.textContent = 'Applying…';
    try {
      await state.client[section.writeFn](dto);
      apply.textContent = 'Awaiting confirm…';
      const confirmed = await waitForApplyConfirm(confirmKey);
      if (confirmed) {
        log(`  ⟶ applied ${section.key} config (confirmed by radio)`);
        apply.textContent = '✓ Confirmed';
      } else {
        log(`  ⟶ applied ${section.key} config (no confirmation in 3 s)`);
        apply.textContent = '⚠ No confirm';
      }
      setTimeout(() => { apply.textContent = prev; apply.disabled = false; }, 1800);
    } catch (e) {
      log(`❌ apply ${section.key}: ${e}`);
      apply.textContent = '✗ Failed';
      setTimeout(() => { apply.textContent = prev; apply.disabled = false; }, 2000);
    }
  };
  hdr.appendChild(apply);
  card.appendChild(hdr);

  const fields = document.createElement('div');
  fields.className = 'fields';
  const sv = sectionValue || {};
  for (const f of section.fields) {
    const lab = document.createElement('label');
    lab.textContent = f.label;
    fields.appendChild(lab);
    fields.appendChild(inputForField(f, sv[f.name]));
  }
  card.appendChild(fields);

  // Position section: mirror the fixed_position checkbox with a coords
  // subgroup that uses setFixedPosition (not a config write).
  if (section.key === 'position') {
    const coords = renderFixedCoordsBox(snap && snap.current_position);
    card.appendChild(coords);
    const checkbox = fields.querySelector('[name="fixed_position"]');
    const sync = () => { coords.hidden = !(checkbox && checkbox.checked); };
    sync();
    if (checkbox) checkbox.addEventListener('change', sync);
  }
  return card;
}

// ---------- fixed coordinates ----------
//
// Lat/lon are entered in decimal degrees and converted to the proto's
// int×1e7 at submit time. Pre-populated from the radio's current
// position when known (snapshot.current_position).

function renderFixedCoordsBox(current) {
  const box = document.createElement('div');
  box.className = 'coords';
  const header = document.createElement('div');
  header.className = 'coords-header';
  header.textContent = 'Coordinates (sent via SetFixedPosition)';
  box.append(header);

  const mkField = (labelText, opts) => {
    const lab = document.createElement('label');
    lab.textContent = labelText;
    const inp = document.createElement('input');
    inp.type = 'number';
    if (opts.step != null) inp.step = String(opts.step);
    if (opts.placeholder) inp.placeholder = opts.placeholder;
    if (opts.value != null) inp.value = String(opts.value);
    box.append(lab, inp);
    return inp;
  };
  const latInput = mkField('Latitude (°)', {
    step: 0.0000001, placeholder: 'e.g. 47.6062',
    value: current ? (current.latitude_i / 1e7).toFixed(7) : null,
  });
  const lonInput = mkField('Longitude (°)', {
    step: 0.0000001, placeholder: 'e.g. -122.3321',
    value: current ? (current.longitude_i / 1e7).toFixed(7) : null,
  });
  const altInput = mkField('Altitude (m)', {
    step: 1, placeholder: 'e.g. 50',
    value: current ? current.altitude : null,
  });

  const actions = document.createElement('div');
  actions.className = 'coords-actions';
  const setBtn = document.createElement('button');
  setBtn.className = 'apply';
  setBtn.type = 'button';
  setBtn.textContent = 'Set coordinates';
  actions.append(setBtn);
  box.append(actions);

  setBtn.onclick = async (e) => {
    e.stopPropagation();
    const lat = parseFloat(latInput.value);
    const lon = parseFloat(lonInput.value);
    const alt = parseInt(altInput.value || '0', 10);
    if (!Number.isFinite(lat) || !Number.isFinite(lon)) {
      log('❌ enter latitude and longitude in decimal degrees first');
      return;
    }
    const dto = {
      latitude_i: Math.round(lat * 1e7),
      longitude_i: Math.round(lon * 1e7),
      altitude: Number.isFinite(alt) ? alt : 0,
    };
    const prev = setBtn.textContent;
    setBtn.disabled = true;
    setBtn.textContent = 'Setting…';
    try {
      await state.client.setFixedPosition(dto);
      setBtn.textContent = 'Awaiting confirm…';
      // SetFixedPosition triggers a Config(position) re-emit on the
      // radio side, so the same watcher key the section Apply uses
      // also works here.
      const confirmed = await waitForApplyConfirm('config:position');
      if (confirmed) {
        log(`  ⟶ set fixed position ${lat.toFixed(7)}, ${lon.toFixed(7)} @ ${dto.altitude} m (confirmed)`);
        setBtn.textContent = '✓ Confirmed';
      } else {
        log(`  ⟶ set fixed position (no confirmation in 3 s)`);
        setBtn.textContent = '⚠ No confirm';
      }
      setTimeout(() => { setBtn.textContent = prev; setBtn.disabled = false; }, 1800);
    } catch (err) {
      log(`❌ set coordinates: ${err}`);
      setBtn.textContent = '✗ Failed';
      setTimeout(() => { setBtn.textContent = prev; setBtn.disabled = false; }, 2000);
    }
  };

  return box;
}

// ---------- channels ----------

function renderChannelsCard(channels) {
  const wrap = document.createElement('div');
  const card = document.createElement('details');
  card.className = 'setcard';
  const hdr = document.createElement('summary');
  hdr.innerHTML = `<h3>💬 Channels</h3><span class="hint">${(channels || []).length} configured</span>`;
  card.appendChild(hdr);
  const fields = document.createElement('div');
  fields.className = 'fields';
  fields.style.gridTemplateColumns = '1fr';
  if (!channels || channels.length === 0) {
    const note = document.createElement('div');
    note.className = 'full'; note.textContent = 'No channel info yet.';
    fields.appendChild(note);
  } else {
    for (const c of channels) {
      fields.appendChild(renderOneChannel(c));
    }
  }
  card.appendChild(fields);
  wrap.appendChild(card);
  return wrap;
}

function renderOneChannel(ch) {
  const row = document.createElement('div');
  row.style.display = 'grid';
  row.style.gridTemplateColumns = '4rem 1fr auto auto auto';
  row.style.gap = '0.5rem';
  row.style.alignItems = 'center';
  row.style.padding = '0.4rem 0';
  row.style.borderTop = '1px solid var(--border)';
  row.dataset.index = String(ch.index);
  const idx = document.createElement('span'); idx.className = 'muted'; idx.textContent = `#${ch.index}`;
  const name = document.createElement('input'); name.type = 'text'; name.placeholder = 'Channel name';
  name.value = ch.name || ''; name.dataset.field = 'name';
  const up = document.createElement('label'); up.style.fontSize = '0.85rem';
  const upChk = document.createElement('input'); upChk.type = 'checkbox'; upChk.checked = !!ch.uplink_enabled; upChk.dataset.field = 'uplink_enabled';
  up.appendChild(upChk); up.appendChild(document.createTextNode(' uplink'));
  const dn = document.createElement('label'); dn.style.fontSize = '0.85rem';
  const dnChk = document.createElement('input'); dnChk.type = 'checkbox'; dnChk.checked = !!ch.downlink_enabled; dnChk.dataset.field = 'downlink_enabled';
  dn.appendChild(dnChk); dn.appendChild(document.createTextNode(' downlink'));
  const apply = document.createElement('button');
  apply.className = 'apply'; apply.textContent = 'Apply';
  apply.onclick = async (e) => {
    e.stopPropagation();
    const dto = {
      index: ch.index, role: ch.role,
      name: name.value,
      uplink_enabled: upChk.checked,
      downlink_enabled: dnChk.checked,
    };
    apply.disabled = true;
    const prev = apply.textContent; apply.textContent = '…';
    try {
      await state.client.writeChannel(dto);
      const confirmed = await waitForApplyConfirm(`channel:${ch.index}`);
      if (confirmed) {
        log(`  ⟶ applied channel ${ch.index} (confirmed by radio)`);
        apply.textContent = '✓';
      } else {
        log(`  ⟶ applied channel ${ch.index} (no confirmation in 3 s)`);
        apply.textContent = '⚠';
      }
      setTimeout(() => { apply.textContent = prev; apply.disabled = false; }, 1800);
    } catch (e) {
      log(`❌ apply channel ${ch.index}: ${e}`);
      apply.textContent = '✗';
      setTimeout(() => { apply.textContent = prev; apply.disabled = false; }, 2000);
    }
  };
  row.appendChild(idx); row.appendChild(name); row.appendChild(up); row.appendChild(dn); row.appendChild(apply);
  return row;
}

// ---------- top-level render ----------

let settingsCardsEl, settingsRefreshBtn, settingsHintEl;
let denoiseEl, sendCodecEl, codecModeEl, amrnbModeEl, opusKbpsEl;
let fecModeEl, nackModeEl;

/// Wire up DOM refs + Audio category change handlers. Called once at
/// startup by app.js.
export function initSettings() {
  settingsCardsEl = document.getElementById('settings-cards');
  settingsRefreshBtn = document.getElementById('settings-refresh');
  settingsHintEl = document.getElementById('settings-hint');
  settingsRefreshBtn.onclick = renderSettings;

  // Audio settings live in static HTML; just wire the change handlers
  // through to the wasm setters. All are gated on `state.client`
  // because they're inert until a radio is attached.
  denoiseEl = document.getElementById('denoise');
  sendCodecEl = document.getElementById('send-codec');
  codecModeEl = document.getElementById('codec-mode');
  amrnbModeEl = document.getElementById('amrnb-mode');
  opusKbpsEl = document.getElementById('opus-kbps');
  fecModeEl = document.getElementById('fec-mode');
  nackModeEl = document.getElementById('nack-mode');

  denoiseEl.onchange = () => {
    if (!state.client) return;
    state.client.setDenoiseEnabled(denoiseEl.checked);
    log(`noise suppression ${denoiseEl.checked ? 'on' : 'off'}`);
  };
  codecModeEl.onchange = () => {
    if (!state.client) return;
    const mode = parseInt(codecModeEl.value, 10);
    state.client.setCodec2Mode(mode);
    log(`codec2 mode set to ${codecModeEl.options[codecModeEl.selectedIndex].text}`);
  };
  amrnbModeEl.onchange = () => {
    if (!state.client) return;
    const mode = parseInt(amrnbModeEl.value, 10);
    state.client.setAmrnbMode(mode);
    log(`AMR-NB mode set to ${amrnbModeEl.options[amrnbModeEl.selectedIndex].text}`);
  };
  opusKbpsEl.onchange = () => {
    if (!state.client) return;
    const kbps = parseInt(opusKbpsEl.value, 10);
    state.client.setOpusKbps(kbps);
    log(`Opus bitrate set to ${opusKbpsEl.options[opusKbpsEl.selectedIndex].text}`);
  };
  sendCodecEl.onchange = () => {
    if (!state.client) return;
    state.client.setSendCodec(sendCodecEl.value);
    log(`send codec set to ${sendCodecEl.value}`);
    refreshCodecRows();
  };
  fecModeEl.onchange = () => {
    if (!state.client) return;
    state.client.setFecMode(fecModeEl.value);
    log(`FEC parity set to ${fecModeEl.options[fecModeEl.selectedIndex].text}`);
  };
  nackModeEl.onchange = () => {
    if (!state.client) return;
    state.client.setNackMode(nackModeEl.value);
    log(`NACK policy set to ${nackModeEl.options[nackModeEl.selectedIndex].text}`);
  };
}

/// Show only the mode/bitrate dropdown for the currently-selected send
/// codec. Called from initSettings and after every connect.
export function refreshCodecRows() {
  const which = sendCodecEl.value;
  for (const el of document.querySelectorAll('[data-codec]')) {
    el.hidden = el.dataset.codec !== which;
  }
}

/// Enable/disable the Audio category controls in lockstep with the
/// connection. Called by app.js's setConnectedUi.
export function setAudioControlsEnabled(on) {
  denoiseEl.disabled = !on;
  sendCodecEl.disabled = !on;
  codecModeEl.disabled = !on;
  amrnbModeEl.disabled = !on;
  opusKbpsEl.disabled = !on;
  fecModeEl.disabled = !on;
  nackModeEl.disabled = !on;
  settingsRefreshBtn.disabled = !on;
  if (on) refreshCodecRows();
}

/// Re-render the Settings page from `client.snapshot()`. Called on
/// each ConfigComplete (so the cards are populated whenever the user
/// navigates to /settings) and from the Refresh button.
export function renderSettings() {
  if (!state.client) {
    settingsCardsEl.innerHTML = '';
    settingsHintEl.textContent = 'Connect a radio to load settings.';
    return;
  }
  let snap;
  try { snap = state.client.snapshot(); }
  catch (e) { settingsHintEl.textContent = 'snapshot failed: ' + e; return; }
  settingsCardsEl.innerHTML = '';
  for (const section of SECTIONS) {
    settingsCardsEl.appendChild(renderSection(section, snap[section.key], snap));
  }
  settingsCardsEl.appendChild(renderChannelsCard(snap.channels));
  const known = SECTIONS.filter((s) => snap[s.key]).length + (snap.channels?.length ? 1 : 0);
  settingsHintEl.textContent = `Loaded ${known} of ${SECTIONS.length + 1} section(s).`;
}
