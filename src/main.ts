import { renderHero, setHeroMuted } from "./components/hero";
import { api, onStateChanged, onLevel, onRecording, type AppState, type DeviceInfo, type Mode } from "./api";
import { renderMeter } from "./components/meter";
import { initRecorder, renderRecorder } from "./components/hotkey-recorder";

/**
 * Task 13 built the shell; this file wires it to the backend (amendment §1).
 *
 * The frontend stores no application state of its own — every render below
 * is a wholesale re-draw from the latest `state-changed` payload (§6). Two
 * exceptions, both transient UI state:
 *  - the settings sheet's open/closed flag, unchanged from Task 13, pure
 *    presentation, no IPC;
 *  - `recordingCombo` / `devices`, neither of which lives in `AppState` in
 *    the first place — the live combo only exists while `recording` is
 *    true, and the device list is its own command (§1/§5), not part of the
 *    hot snapshot.
 */

const hero = document.querySelector<HTMLElement>("#hero");
if (hero) renderHero(hero);

const cog = document.querySelector<HTMLButtonElement>("#settings-cog");
const panel = document.querySelector<HTMLElement>("#settings-panel");

// Already wired here by Task 13 — pure DOM, no IPC. Do not add a second
// handler (amendment §3): it would toggle the sheet twice per click.
cog?.addEventListener("click", () => {
  if (!panel) return;
  const open = panel.hidden;
  panel.hidden = !open;
  cog.setAttribute("aria-expanded", String(open));
});

// Escape closes the sheet — it has no close button of its own, and the cog is
// the only other way out.
window.addEventListener("keydown", (e) => {
  if (e.key !== "Escape" || !panel || panel.hidden) return;
  panel.hidden = true;
  cog?.setAttribute("aria-expanded", "false");
});

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

/** One line's worth of `#error-banner` at its fixed width/font (measured:
 *  ~68 chars fit the 390px content box at 12px Segoe UI) — Task 13's
 *  original single-line budget for this element. Truncated in JS rather
 *  than left to CSS alone: real COM error strings run far longer than the
 *  placeholder text this card was sized around, and an unbounded string
 *  pushes `#settings-cog` — the only way out of the settings sheet — off
 *  the bottom of the fixed 700px window (amendment §4; CSS `overflow`
 *  below stays as a backstop). */
const ERROR_MAX_CHARS = 65;
const truncate = (s: string, max: number) =>
  s.length > max ? `${s.slice(0, max - 1).trimEnd()}…` : s;

const els = {
  hero: $("hero"),
  error: $("error-banner"),
  device: $<HTMLSelectElement>("device"),
  meter: $("meter"),
  meterValue: $("meter-value"),
  volume: $<HTMLInputElement>("volume"),
  muteSwitch: $<HTMLInputElement>("mute-switch"),
  mode: $<HTMLSelectElement>("mode"),
  hotkeyDisplay: $("hotkey-display"),
  hotkeyRecord: $<HTMLButtonElement>("hotkey-record"),
  hotkeyClear: $<HTMLButtonElement>("hotkey-clear"),
  hotkeyWarning: $("hotkey-warning"),
  toast: $<HTMLInputElement>("toast-switch"),
  sound: $<HTMLInputElement>("sound-switch"),
  autostart: $<HTMLInputElement>("autostart-switch"),
};

/** Last `list_devices` result. Refreshed by its own call site (window open,
 *  after `set_device`), not by `state-changed` — see `refreshDevices`. */
let devices: DeviceInfo[] = [];

/** The combo shown live while `recording` is true. Discarded once the
 *  recording commits or cancels; `AppState.hotkey_display` takes back over. */
let recordingCombo: string | null = null;

function renderDeviceOptions(selected: string | null) {
  const known = devices.map(
    (d) => `<option value="${d.id}">${d.name}${d.is_default ? " (default)" : ""}</option>`,
  );
  // §5: a saved device absent from the enumerated list (e.g. unplugged)
  // must stay visible rather than the dropdown silently snapping to
  // "System default" and losing the user's setting.
  const known_ids = new Set(devices.map((d) => d.id));
  const missing =
    selected !== null && !known_ids.has(selected)
      ? `<option value="${selected}">${selected} (not connected)</option>`
      : "";
  els.device.innerHTML = `<option value="">System default</option>${missing}${known.join("")}`;
  els.device.value = selected ?? "";
}

/** Structured as its own call site (window open, post-`set_device`) so a
 *  future `devices-changed` listener (Task 9b hotplug) can call it too
 *  without restructuring anything else — amendment §5. */
function refreshDevices(selected: string | null) {
  api.listDevices().then((list) => {
    devices = list;
    renderDeviceOptions(selected);
  });
}

function render(s: AppState) {
  setHeroMuted(els.hero, s.muted);

  renderDeviceOptions(s.selected_device);

  els.volume.value = String(Math.round(s.volume * 100));
  els.mode.value = s.mode;

  // §6: manual mute controls are disabled in Push to Talk — PTT owns mute.
  els.muteSwitch.checked = s.muted;
  els.muteSwitch.disabled = !s.manual_controls_enabled;

  renderRecorder(els.hotkeyDisplay, els.hotkeyRecord, els.hotkeyWarning, {
    recording: s.recording,
    combo: s.recording ? recordingCombo : s.hotkey_display,
    bare: s.hotkey_is_bare_printable && !s.recording,
  });

  // "The last thing that went wrong", not "something is wrong right now" —
  // no user-dismiss path, stays pinned until a later fallible call succeeds.
  els.error.hidden = s.last_error === null;
  if (s.last_error !== null) els.error.textContent = truncate(s.last_error, ERROR_MAX_CHARS);

  els.toast.checked = s.notifications.toast;
  els.sound.checked = s.notifications.sound;
  els.autostart.checked = s.autostart;
}

els.device.addEventListener("change", () => {
  const id = els.device.value || null;
  api.setDevice(id).then(() => refreshDevices(id));
});
els.mode.addEventListener("change", () => api.setMode(els.mode.value as Mode));
els.volume.addEventListener("input", () => api.setVolume(Number(els.volume.value) / 100));
els.muteSwitch.addEventListener("change", () => api.toggleMute());
const prefs = () => api.setNotificationPrefs(els.toast.checked, els.sound.checked);
els.toast.addEventListener("change", prefs);
els.sound.addEventListener("change", prefs);
els.autostart.addEventListener("change", () => api.setAutostart(els.autostart.checked));

initRecorder(els.hotkeyDisplay, els.hotkeyRecord, els.hotkeyClear);

onStateChanged(render);
// 30Hz. Touches only the meter bar and its readout — never re-renders the
// whole state on a level tick (amendment §7).
onLevel((db) => renderMeter(els.meter, els.meterValue, db));
onRecording((_active, combo) => {
  recordingCombo = combo;
  renderRecorder(els.hotkeyDisplay, els.hotkeyRecord, els.hotkeyWarning, {
    recording: true,
    combo,
    bare: false,
  });
});

api.getState().then((s) => {
  render(s);
  refreshDevices(s.selected_device);
});
