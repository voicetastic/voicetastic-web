// Chat-page state + UI: per-thread message lists, the dropdown that
// picks the active thread, mic capture (AudioWorklet), and voice
// playback. Nothing in here decodes protocol — that's done on the wasm
// side and arrives via `routeIncoming` (text) or `onVoice` (voice).

import { state } from './state.js';
import { log, nodeAddr, nodeDisplay } from './ui.js';

// ---------- thread state ----------
//
// Each thread key is either `channel:<index>` (broadcast) or
// `node:!<hex>` (DM, where the hex is the peer's node id).
const threads = new Map();
threads.set('channel:0', { label: 'Broadcast — Channel 0', messages: [] });

let threadEl, chatEl, textEl, sendBtn, recBtn, recTimer;

/// Wire up the chat-page DOM. Called once at startup by app.js.
export function initChat() {
  threadEl = document.getElementById('chat-thread');
  chatEl = document.getElementById('chat-messages');
  textEl = document.getElementById('text');
  sendBtn = document.getElementById('send');
  recBtn = document.getElementById('record');
  recTimer = document.getElementById('rectimer');

  threadEl.addEventListener('change', renderChat);

  const doSend = async () => {
    const text = textEl.value.trim();
    if (!text || !state.client) return;
    const dest = parseThread(threadEl.value);
    try {
      // sendText now resolves with the mesh packet id; pass it through
      // so the message bubble can show its delivery-status icon.
      const id = await state.client.sendText(text, dest.channel, dest.to);
      log('  ⟶ sent: ' + text);
      pushMessage(threadEl.value, threads.get(threadEl.value).label, 'You', text, 'out', id);
      textEl.value = '';
    } catch (e) { log('❌ send failed: ' + e); }
  };
  sendBtn.onclick = doSend;
  textEl.addEventListener('keydown', (e) => { if (e.key === 'Enter') doSend(); });

  recBtn.onclick = async () => {
    try {
      if (recording) await stopRecording();
      else await startRecording();
    } catch (e) {
      log('❌ mic error: ' + e);
      recording = null;
      recBtn.textContent = '🎙 Record';
      recBtn.classList.remove('rec');
    }
  };
}

/// Enable/disable the chat-page inputs based on connection state.
/// Called by app.js's setConnectedUi.
export function setChatEnabled(on) {
  textEl.disabled = !on;
  sendBtn.disabled = !on;
  recBtn.disabled = !on;
}

/// Drop the per-thread message lists between connections so a new
/// radio doesn't show the previous session's chat. Keeps the broadcast
/// seed so the dropdown is never empty.
export function clearThreads() {
  threads.clear();
  threads.set('channel:0', { label: 'Broadcast — Channel 0', messages: [] });
  // Refresh the dropdown to match.
  threadEl.replaceChildren();
  const opt = document.createElement('option');
  opt.value = 'channel:0';
  opt.textContent = 'Broadcast — Channel 0';
  threadEl.appendChild(opt);
  renderChat();
}

/// Ensure a thread exists; create + add to the dropdown if not, and
/// rename if a better label is now known.
export function ensureThread(key, label) {
  if (!threads.has(key)) {
    threads.set(key, { label, messages: [] });
    const opt = document.createElement('option');
    opt.value = key;
    opt.textContent = label;
    threadEl.appendChild(opt);
  } else if (label && threads.get(key).label !== label) {
    threads.get(key).label = label;
    for (const opt of threadEl.options) {
      if (opt.value === key) opt.textContent = label;
    }
  }
}

/// Push a message into a thread + re-render if it's currently selected.
/// `packetId` (optional) is the mesh packet id returned by `sendText`;
/// when present, outgoing messages start with `status: 'pending'` and
/// flip to delivered/failed/timed_out via `setMessageStatus` when the
/// matching `ack_or_nak` event lands.
export function pushMessage(threadKey, threadLabel, who, body, kind, packetId) {
  ensureThread(threadKey, threadLabel);
  const msg = { who, body, kind };
  if (typeof packetId === 'number') {
    msg.packetId = packetId;
    msg.status = 'pending';
  }
  threads.get(threadKey).messages.push(msg);
  if (threadEl.value === threadKey) renderChat();
}

/// Flip an outgoing message's delivery-status icon. Called from
/// events.js when an `ack_or_nak` lands. No-op if no message in any
/// thread has that packet id (e.g. an ack arriving for a message we
/// sent before the page loaded).
export function setMessageStatus(packetId, status) {
  for (const t of threads.values()) {
    for (const m of t.messages) {
      if (m.packetId === packetId) {
        m.status = status;
        renderChat();
        return;
      }
    }
  }
}

/// Currently-expanded node detail row in the Chat-tab Nodes panel.
/// `null` collapses every row; otherwise the matching `num` row renders
/// an inline detail block underneath it.
let expandedNodeNum = null;

/// Re-render the Chat tab's Nodes panel from `client.listNodes()`. Called
/// on the relevant events (`node_info`, `config_complete`, `disconnect`)
/// so the list stays in sync without polling.
export function renderNodes() {
  const listEl = document.getElementById('nodes-list');
  const countEl = document.getElementById('nodes-count');
  if (!listEl) return;
  if (!state.client) {
    listEl.innerHTML = '<div class="placeholder muted">Connect a radio to load the node list.</div>';
    if (countEl) countEl.textContent = '(0)';
    return;
  }
  let rows;
  try { rows = state.client.listNodes(); }
  catch (e) {
    listEl.innerHTML = `<div class="placeholder muted">listNodes failed: ${e}</div>`;
    return;
  }
  if (countEl) countEl.textContent = `(${rows.length})`;
  if (rows.length === 0) {
    listEl.innerHTML = '<div class="placeholder muted">No peers heard yet.</div>';
    return;
  }
  const now = Math.floor(Date.now() / 1000);
  const fmtAge = (lh) => {
    if (!lh) return '—';
    const a = Math.max(0, now - lh);
    if (a < 60) return `${a}s ago`;
    if (a < 3600) return `${Math.floor(a / 60)}m ago`;
    if (a < 86400) return `${Math.floor(a / 3600)}h ago`;
    return `${Math.floor(a / 86400)}d ago`;
  };
  const fmtBat = (b) => b == null ? '—' : (b === 101 ? 'AC' : `${b}%`);
  const fmtUptime = (s) => {
    if (s == null) return null;
    const sec = s % 60, m = Math.floor(s / 60) % 60, h = Math.floor(s / 3600) % 24, d = Math.floor(s / 86400);
    if (d > 0) return `${d}d ${h}h`;
    if (h > 0) return `${h}h ${m}m`;
    if (m > 0) return `${m}m ${sec}s`;
    return `${sec}s`;
  };
  const escape = (s) => s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  const hexId = (n) => `!${n.toString(16).padStart(8, '0')}`;

  // Build a min-max-normalised SVG polyline of values inside a 160×24
  // rect, the same dimensions desktop uses. `nullable` = true skips
  // null/missing entries (battery is optional per-sample); otherwise
  // 0s are kept (snr).
  const sparkline = (values, lo, hi, color, nullable = false) => {
    const filtered = nullable ? values.filter((v) => v != null) : values;
    if (filtered.length < 2) return '';
    const W = 160, H = 24;
    const n = filtered.length;
    const span = hi - lo || 1;
    const pts = filtered.map((v, i) => {
      const norm = Math.max(0, Math.min(1, (v - lo) / span));
      const x = (W / (n - 1)) * i;
      const y = H - norm * H;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    }).join(' ');
    return `<svg class="spark" viewBox="0 0 ${W} ${H}" width="${W}" height="${H}">
      <rect x="0.5" y="0.5" width="${W - 1}" height="${H - 1}" fill="none" stroke="${color}" stroke-opacity="0.25" />
      <polyline fill="none" stroke="${color}" stroke-width="1.5" points="${pts}"/>
    </svg>`;
  };

  const detailRows = (n) => {
    const lines = [];
    lines.push(['Node id', `${hexId(n.num)} (${n.num})`]);
    if (n.long_name) lines.push(['Long name', escape(n.long_name)]);
    if (n.short_name) lines.push(['Short name', escape(n.short_name)]);
    lines.push(['HW model', String(n.hw_model)]);
    lines.push(['Role', String(n.role)]);
    if (n.is_licensed) lines.push(['HAM', 'yes']);
    lines.push(['Channel', String(n.channel)]);
    if (n.last_heard) lines.push(['Last heard', `${fmtAge(n.last_heard)} (${n.last_heard})`]);
    lines.push(['SNR', `${n.snr.toFixed(1)} dB`]);
    if (n.latitude_i != null && n.longitude_i != null) {
      const lat = (n.latitude_i / 1e7).toFixed(5);
      const lon = (n.longitude_i / 1e7).toFixed(5);
      lines.push(['Position', `${lat}, ${lon}`]);
    }
    if (n.altitude != null) lines.push(['Altitude', `${n.altitude} m`]);
    if (n.battery_level != null) lines.push(['Battery', fmtBat(n.battery_level)]);
    if (n.voltage != null) lines.push(['Voltage', `${n.voltage.toFixed(2)} V`]);
    if (n.channel_utilization != null) lines.push(['Ch util', `${n.channel_utilization.toFixed(1)}%`]);
    if (n.air_util_tx != null) lines.push(['Air util TX', `${n.air_util_tx.toFixed(1)}%`]);
    const up = fmtUptime(n.uptime_seconds);
    if (up) lines.push(['Uptime', up]);
    if (n.via_mqtt) lines.push(['Via MQTT', 'yes']);
    if (n.is_favorite) lines.push(['Favorite', 'yes']);
    // Telemetry sparklines, surfaced only when ≥ 2 samples have
    // landed for this peer (battery 0–100, SNR scaled to typical
    // mesh range of −20 dB..+20 dB).
    const samples = state.nodeHistory.get(n.num) || [];
    if (samples.length >= 2) {
      const batt = samples.map((s) => s.battery);
      const snr = samples.map((s) => s.snr);
      const battSvg = sparkline(batt, 0, 100, '#78c878', true);
      const snrSvg = sparkline(snr, -20, 20, '#78a0dc');
      if (battSvg) lines.push(['Battery trend', battSvg]);
      if (snrSvg) lines.push(['SNR trend', snrSvg]);
    }
    return lines.map(([k, v]) => `<dt>${k}</dt><dd>${v}</dd>`).join('');
  };

  const tbody = rows.map((n) => {
    const display = n.long_name || n.short_name || hexId(n.num);
    const isOpen = expandedNodeNum === n.num;
    const summaryRow = `<tr data-num="${n.num}" class="node-row${isOpen ? ' open' : ''}">
      <td>${isOpen ? '▾' : '▸'} ${escape(display)}</td>
      <td>${fmtAge(n.last_heard)}</td>
      <td>${n.snr.toFixed(1)} dB</td>
      <td>${fmtBat(n.battery_level)}</td>
      <td class="id">${hexId(n.num)}</td>
    </tr>`;
    if (!isOpen) return summaryRow;
    return summaryRow + `<tr class="node-detail-row"><td colspan="5"><dl class="node-detail">${detailRows(n)}</dl></td></tr>`;
  }).join('');
  listEl.innerHTML = `<table>
    <thead><tr><th>Node</th><th>Last heard</th><th>SNR</th><th>Battery</th><th>ID</th></tr></thead>
    <tbody>${tbody}</tbody>
  </table>`;

  // Wire click handlers per row (no event-delegation gymnastics; we
  // re-render on every node_info anyway so the live list isn't large).
  for (const tr of listEl.querySelectorAll('tr.node-row')) {
    tr.addEventListener('click', () => {
      const num = parseInt(tr.dataset.num, 10);
      expandedNodeNum = (expandedNodeNum === num) ? null : num;
      renderNodes();
    });
  }
}

/// Render the currently-selected thread into the messages pane.
export function renderChat() {
  const key = threadEl.value;
  const t = threads.get(key);
  chatEl.innerHTML = '';
  if (!t || t.messages.length === 0) {
    const ph = document.createElement('div');
    ph.className = 'placeholder';
    ph.textContent = state.client ? 'No messages yet in this thread.' : 'Connect a radio to start chatting.';
    chatEl.appendChild(ph);
    return;
  }
  for (const m of t.messages) {
    const row = document.createElement('div');
    row.className = 'msg' + (m.kind ? ' ' + m.kind : '');
    const who = document.createElement('span'); who.className = 'who'; who.textContent = m.who;
    row.appendChild(who);
    if (m.voice) {
      const player = document.createElement('span');
      player.className = 'body voice-player';
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'voice-play';
      btn.textContent = (currentVoice && currentVoice.id === m.voiceId) ? '⏹' : '▶';
      btn.onclick = () => playVoice(m, btn);
      if (currentVoice && currentVoice.id === m.voiceId) currentVoice.btnEl = btn;
      const meta = document.createElement('span');
      meta.className = 'voice-meta';
      meta.textContent = `${fmtDuration(m.duration_ms)} · ${m.codec}`;
      player.appendChild(btn); player.appendChild(meta);
      row.appendChild(player);
    } else {
      const body = document.createElement('span'); body.className = 'body'; body.textContent = m.body;
      row.appendChild(body);
    }
    if (m.status) {
      const status = document.createElement('span');
      status.className = 'msg-status';
      const map = {
        pending:   { icon: '⏳', hover: 'Waiting for delivery ack' },
        delivered: { icon: '✓',  hover: 'Delivered' },
        failed:    { icon: '❌', hover: 'Delivery failed' },
        timed_out: { icon: '⏱', hover: 'No ack within timeout; may still be in flight' },
        cancelled: { icon: '⊘', hover: 'Cancelled' },
      };
      const view = map[m.status] || { icon: '?', hover: m.status };
      status.textContent = view.icon;
      status.title = view.hover;
      row.appendChild(status);
    }
    chatEl.appendChild(row);
  }
  chatEl.scrollTop = chatEl.scrollHeight;
}

/// `channel:N` → `{ channel: N, to: undefined }` (broadcast),
/// `node:!HEX` → `{ channel: 0, to: u32 }` (DM).
function parseThread(key) {
  if (key.startsWith('channel:')) return { channel: parseInt(key.slice(8), 10), to: undefined };
  if (key.startsWith('node:')) return { channel: 0, to: parseInt(key.slice(6), 16) };
  return { channel: 0, to: undefined };
}

// Decide which thread an inbound text goes to — broadcast on `channel`
// if addressed to 0xffffffff, DM thread on the sender's id if addressed
// to us, drop otherwise (flood-routed traffic destined for another node).
const BROADCAST = 0xffffffff;
export function routeIncoming(fromNum, toNum, channel, body, kind) {
  const fromAddr = nodeAddr(fromNum);
  const fromName = nodeDisplay(fromNum);
  if (toNum === BROADCAST) {
    const key = `channel:${channel}`;
    const label = channel === 0 ? 'Broadcast — Channel 0' : `Channel ${channel}`;
    pushMessage(key, label, fromName, body, kind);
  } else if (state.myNodeNum != null && toNum === state.myNodeNum) {
    const key = `node:${fromAddr}`;
    const label = `DM — ${fromName}`;
    pushMessage(key, label, fromName, body, kind);
  }
}

// ---------- voice playback ----------
//
// Inbound voice clips no longer auto-play. The wasm side delivers a
// decoded PCM blob via `on_voice`; we drop it into the matching thread
// as a row with a ▶ button. One playback is active at a time — clicking
// a new ▶ stops the previous clip.
let playCtx = null;
let voiceSeq = 0;
let currentVoice = null; // { id, src, btnEl }

function stopCurrentVoice() {
  if (!currentVoice) return;
  try { currentVoice.src.stop(); } catch (_) {}
  if (currentVoice.btnEl) currentVoice.btnEl.textContent = '▶';
  currentVoice = null;
}

function playVoice(msg, btnEl) {
  if (currentVoice && currentVoice.id === msg.voiceId) {
    stopCurrentVoice();
    return;
  }
  stopCurrentVoice();
  try {
    if (!playCtx) playCtx = new AudioContext();
    const buf = playCtx.createBuffer(1, msg.pcm.length, msg.rate);
    buf.getChannelData(0).set(msg.pcm);
    const src = playCtx.createBufferSource();
    src.buffer = buf;
    src.connect(playCtx.destination);
    const id = msg.voiceId;
    src.onended = () => {
      if (currentVoice && currentVoice.id === id) {
        if (btnEl) btnEl.textContent = '▶';
        currentVoice = null;
      }
    };
    src.start();
    btnEl.textContent = '⏹';
    currentVoice = { id, src, btnEl };
  } catch (e) { log('playback error: ' + e); }
}

function fmtDuration(ms) {
  const s = Math.max(0, ms / 1000);
  if (s < 60) return s.toFixed(1) + 's';
  const m = Math.floor(s / 60);
  const r = Math.floor(s % 60);
  return `${m}:${r.toString().padStart(2, '0')}`;
}

/// `on_voice` callback handed to the wasm side. Receives one decoded
/// PCM block plus routing metadata, threads it into the matching chat
/// thread as a clickable voice row.
export function onVoice(detail) {
  const { pcm, rate, from, to, channel, codec, duration_ms } = detail;
  // `from` / `to` arrive as raw u32s from the wasm side; nodeAddr /
  // nodeDisplay format them once. No munging here.
  const fromAddr = nodeAddr(from);
  const fromName = nodeDisplay(from);
  const voiceId = `v${++voiceSeq}`;
  const voiceMsg = {
    who: fromName,
    kind: 'in voice',
    voice: true,
    voiceId,
    pcm,
    rate,
    duration_ms,
    codec,
  };
  if (to === BROADCAST) {
    const key = `channel:${channel}`;
    const label = channel === 0 ? 'Broadcast — Channel 0' : `Channel ${channel}`;
    ensureThread(key, label);
    threads.get(key).messages.push(voiceMsg);
    if (threadEl.value === key) renderChat();
  } else if (state.myNodeNum != null && to === state.myNodeNum) {
    const key = `node:${fromAddr}`;
    const label = `DM — ${fromName}`;
    ensureThread(key, label);
    threads.get(key).messages.push(voiceMsg);
    if (threadEl.value === key) renderChat();
  } else {
    log(`🎙️ voice from ${fromName} not addressed to us (to=${nodeAddr(to)})`);
  }
  log(`  ⏵ voice from ${fromName} ready (${fmtDuration(duration_ms)}, ${codec})`);
}

// ---------- mic capture ----------
//
// Audio runs off the main thread via AudioWorklet: a small processor is
// loaded from an inline blob URL, receives 128-sample blocks on the
// audio thread, and posts a copy of each block back to the main thread.
// ScriptProcessorNode (deprecated, main-thread) is no longer used. Mic
// preprocessing stays off — RNNoise runs on the wasm side.
const CAPTURE_WORKLET_SRC = `
class CaptureProcessor extends AudioWorkletProcessor {
  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    // process() reuses its input buffers, so copy before posting.
    if (ch && ch.length) this.port.postMessage(new Float32Array(ch));
    return true;
  }
}
registerProcessor('capture', CaptureProcessor);
`;
let captureWorkletUrl = null;
function getCaptureWorkletUrl() {
  if (!captureWorkletUrl) {
    captureWorkletUrl = URL.createObjectURL(
      new Blob([CAPTURE_WORKLET_SRC], { type: 'application/javascript' }),
    );
  }
  return captureWorkletUrl;
}

let recording = null;
async function startRecording() {
  const stream = await navigator.mediaDevices.getUserMedia({
    audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false },
  });
  const ctx = new AudioContext();
  await ctx.audioWorklet.addModule(getCaptureWorkletUrl());
  const source = ctx.createMediaStreamSource(stream);
  const node = new AudioWorkletNode(ctx, 'capture');
  // Silent sink keeps the graph pulled — without a downstream node some
  // engines won't call process() reliably.
  const silent = ctx.createGain(); silent.gain.value = 0;
  const chunks = [];
  node.port.onmessage = (e) => chunks.push(e.data);
  source.connect(node); node.connect(silent); silent.connect(ctx.destination);
  recording = { stream, ctx, source, node, silent, chunks, t0: performance.now() };
  recBtn.textContent = '⏹ Stop';
  recBtn.classList.add('rec');
  const tick = () => {
    if (!recording) return;
    recTimer.textContent = ((performance.now() - recording.t0) / 1000).toFixed(1) + 's';
    requestAnimationFrame(tick);
  };
  tick();
}

async function stopRecording() {
  const r = recording; recording = null;
  recBtn.textContent = '🎙 Record'; recBtn.classList.remove('rec'); recTimer.textContent = '';
  r.node.port.onmessage = null;
  r.node.disconnect(); r.source.disconnect(); r.silent.disconnect();
  r.stream.getTracks().forEach((t) => t.stop());
  const rate = r.ctx.sampleRate;
  await r.ctx.close();
  const total = r.chunks.reduce((n, a) => n + a.length, 0);
  if (total === 0) { log('recording empty'); return; }
  const pcm = new Float32Array(total);
  let o = 0; for (const a of r.chunks) { pcm.set(a, o); o += a.length; }
  const dest = parseThread(threadEl.value);
  log(`  ⟶ encoding + sending ${(total / rate).toFixed(1)}s of audio…`);
  pushMessage(
    threadEl.value,
    threads.get(threadEl.value).label,
    'You',
    `(voice, ${(total / rate).toFixed(1)}s)`,
    'out voice',
  );
  try { await state.client.sendVoice(pcm, rate, dest.channel, dest.to); }
  catch (e) { log('❌ voice send failed: ' + e); }
}
