// Map tab: Leaflet-backed OSM map with a marker per known peer that
// has reported a position. Markers are refreshed when the user
// navigates to /map; we don't push on every node_info update because
// the user only sees the map when it's the active route.

import { state } from './state.js';

let map = null;
const markers = new Map(); // node_num → L.Marker

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

  if (hint) {
    const total = rows.length;
    const plotted = markers.size;
    hint.textContent = `Plotted ${plotted} of ${total} known peer(s).`;
  }

  // First-render auto-fit: if the user hasn't zoomed yet (we're still
  // on the initial worldview) and we have ≥ 1 marker, fit to bounds.
  if (markers.size >= 1 && map.getZoom() <= 2) {
    const bounds = L.latLngBounds(Array.from(markers.values()).map((m) => m.getLatLng()));
    map.fitBounds(bounds.pad(0.2), { maxZoom: 13 });
  }
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}
