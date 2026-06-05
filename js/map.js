// Map tab: Leaflet-backed OSM map with a marker per known peer that
// has reported a position. Markers are refreshed when the user
// navigates to /map; we don't push on every node_info update because
// the user only sees the map when it's the active route.

import { state } from './state.js';

let map = null;
const markers = new Map(); // node_num → L.Marker (peers only)
// A distinct icon for our own node so "you" stands out from peers,
// mirroring the Android self pin. Built lazily because Leaflet (`L`)
// is defer-loaded and may be undefined at module-eval time. The self
// pin is a single dedicated marker (not part of `markers`) because
// `listNodes()` deliberately omits the local node — our position
// comes from `snapshot().current_position` instead.
let selfIcon = null;
let selfMarker = null;

/// Our own node's [lat, lon] in decimal degrees, or null if we have no
/// radio, no reported position, or only the (0, 0) "unknown" sentinel.
/// `list_nodes()` excludes the local node, so the radio's own position
/// is read from `snapshot().current_position` (1e-7 fixed point).
function selfLatLng() {
  if (!state.client) return null;
  let snap;
  try { snap = state.client.snapshot(); }
  catch { return null; }
  const p = snap && snap.current_position;
  if (!p || p.latitude_i == null || p.longitude_i == null) return null;
  if (p.latitude_i === 0 && p.longitude_i === 0) return null;
  return [p.latitude_i / 1e7, p.longitude_i / 1e7];
}

/// Center the map on our own node. Shared by the open-on-/map auto
/// center and the "center on my node" button. Surfaces a hint when we
/// have no position to center on rather than silently doing nothing.
function centerOnSelf() {
  if (!map) return;
  const ll = selfLatLng();
  const hint = document.getElementById('map-hint');
  if (ll) {
    map.setView(ll, 16);
  } else if (hint) {
    hint.textContent = state.client
      ? 'Your node has not reported a position yet.'
      : 'Connect a radio to center on your node.';
  }
}

/// Lazily build the Leaflet map. Called on the first navigation to
/// the /map route — Leaflet's script tag is defer-loaded so it may
/// not be ready at page load, and the map needs a sized container
/// (the page is `display:none` until activated). Idempotent.
export function initMapIfReady() {
  if (map) return true;
  if (typeof L === 'undefined') return false;
  const container = document.getElementById('map');
  if (!container) return false;

  map = L.map('map', { worldCopyJump: true }).setView([20, 0], 2);
  L.tileLayer('https://tile.openstreetmap.org/{z}/{x}/{y}.png', {
    maxZoom: 19,
    attribution: '© <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a>',
  }).addTo(map);

  // The self pin: a green dot ringed in white (CSS in style.css). A
  // `divIcon` keeps this asset-free; passing our own className drops
  // Leaflet's default white-box styling so only our dot shows.
  selfIcon = L.divIcon({
    className: 'map-self-pin',
    html: '<span class="map-self-dot"></span>',
    iconSize: [18, 18],
    iconAnchor: [9, 9],
    popupAnchor: [0, -9],
  });

  // "Center on my node" button — the web counterpart of the Android
  // location FAB. Anchored bottom-right, over the map.
  const CenterControl = L.Control.extend({
    options: { position: 'bottomright' },
    onAdd() {
      const btn = L.DomUtil.create('button', 'map-center-btn');
      btn.type = 'button';
      btn.title = 'Center on my node';
      btn.setAttribute('aria-label', 'Center on my node');
      // Crosshair / "my location" glyph.
      btn.innerHTML =
        '<svg viewBox="0 0 24 24" width="20" height="20" aria-hidden="true">' +
        '<path fill="none" stroke="currentColor" stroke-width="2" ' +
        'd="M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8zM12 2v3M12 19v3M2 12h3M19 12h3"/>' +
        '</svg>';
      // Stop clicks/scrolls reaching the map, then center.
      L.DomEvent.disableClickPropagation(btn);
      L.DomEvent.on(btn, 'click', L.DomEvent.stop);
      L.DomEvent.on(btn, 'click', centerOnSelf);
      return btn;
    },
  });
  map.addControl(new CenterControl());
  return true;
}

/// Re-render the markers from `client.listNodes()`. Only peers with
/// both lat and lon set get a pin; others are silently skipped.
/// Auto-pans to fit if at least one new marker was added; respects
/// the user's current view otherwise.
export function renderMap() {
  // Bail out gracefully when Leaflet hasn't loaded or no radio is
  // connected — the placeholder hint stays visible.
  const hint = document.getElementById('map-hint');
  if (!initMapIfReady()) {
    if (hint) hint.textContent = 'Leaflet still loading…';
    return;
  }
  if (!state.client) {
    if (hint) hint.textContent = 'Connect a radio with peers reporting positions to see pins.';
    return;
  }

  let rows;
  try { rows = state.client.listNodes(); }
  catch (e) {
    if (hint) hint.textContent = `listNodes failed: ${e}`;
    return;
  }

  // Drop pins for peers that disappeared from the snapshot. Iterate
  // a copy so we can mutate `markers` during the loop.
  const seen = new Set();
  for (const n of rows) {
    if (n.latitude_i == null || n.longitude_i == null) continue;
    if (n.latitude_i === 0 && n.longitude_i === 0) continue; // unknown
    seen.add(n.num);
    const lat = n.latitude_i / 1e7;
    const lon = n.longitude_i / 1e7;
    const display = n.long_name || n.short_name || `!${n.num.toString(16).padStart(8, '0')}`;
    const battery = n.battery_level == null ? '—' : (n.battery_level === 101 ? 'AC' : `${n.battery_level}%`);
    const popup = `<b>${escapeHtml(display)}</b><br>` +
                  `!${n.num.toString(16).padStart(8, '0')}<br>` +
                  `SNR ${n.snr.toFixed(1)} dB · Battery ${battery}`;

    let marker = markers.get(n.num);
    if (marker) {
      marker.setLatLng([lat, lon]);
      marker.setPopupContent(popup);
    } else {
      marker = L.marker([lat, lon]).bindPopup(popup).addTo(map);
      markers.set(n.num, marker);
    }
  }
  for (const [num, marker] of markers) {
    if (!seen.has(num)) {
      map.removeLayer(marker);
      markers.delete(num);
    }
  }

  // Draw our own node as a dedicated "you" pin. It isn't in `markers`
  // (or `listNodes()`), so it's tracked separately and refreshed here.
  // zIndexOffset keeps it above peer pins when they overlap.
  const selfLL = selfLatLng();
  if (selfLL) {
    const name = (state.myNodeHex && state.knownNodes.get(state.myNodeHex)) || 'You';
    const popup = `<b>${escapeHtml(name)} (you)</b>` +
                  (state.myNodeHex ? `<br>${escapeHtml(state.myNodeHex)}` : '');
    if (selfMarker) {
      selfMarker.setLatLng(selfLL);
      selfMarker.setPopupContent(popup);
    } else {
      selfMarker = L.marker(selfLL, { icon: selfIcon, zIndexOffset: 1000 })
        .bindPopup(popup).addTo(map);
    }
  } else if (selfMarker) {
    map.removeLayer(selfMarker);
    selfMarker = null;
  }

  if (hint) {
    const total = rows.length;
    const plotted = markers.size;
    hint.textContent = `Plotted ${plotted} of ${total} known peer(s).`;
  }

  // Center on our own node when the map opens, mirroring the Android
  // app: your own position is almost always what you want framed first.
  // renderMap only runs on navigation to /map, so each call is a fresh
  // "open" — re-centering here matches Android's one-shot-per-screen-
  // entry behaviour (it re-frames you every time you return to the map,
  // rather than fighting a pan/zoom while you're already looking at it).
  // This takes priority over the peer-bounds auto-fit below.
  if (selfLL) {
    map.setView(selfLL, 16);
  } else if (markers.size >= 1 && map.getZoom() <= 2) {
    // No self position yet: fall back to framing all known peers.
    // First-render only — if the user has already zoomed in (zoom > 2)
    // we leave their view alone.
    const bounds = L.latLngBounds(Array.from(markers.values()).map((m) => m.getLatLng()));
    map.fitBounds(bounds.pad(0.2), { maxZoom: 13 });
  }
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}
