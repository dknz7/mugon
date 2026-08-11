import { renderHero, setHeroMuted } from "./components/hero";
import {
  api,
  onStateChanged,
  onLevel,
  onDevicesChanged,
  type AppState,
  type DeviceInfo,
  type Mode,
} from "./api";
import { renderMeter } from "./components/meter";
import {
  initPicker,
  renderKeyOptions,
  renderPicker,
  type PickerElements,
} from "./components/hotkey-picker";

/**
 * Task 13 built the shell; this file wires it to the backend (amendment §1).
 *
 * The frontend stores no application state of its own — every render below
 * is a wholesale re-draw from the latest `state-changed` payload (§6). Two
 * exceptions, both transient UI state:
 *  - the settings sheet's open/closed flag, unchanged from Task 13, pure
 *    presentation, no IPC;
 *  - `devices`, which does not live in `AppState` in the first place — the
 *    device list is its own command (§1/§5), not part of the hot snapshot.
 *
 * Task 17 replaced the hotkey recorder with a picker. The bindable key list is
 * the same shape of exception: its own command, fetched once on window open,
 * because it never changes for the life of the process.
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
  toast: $<HTMLInputElement>("toast-switch"),
  sound: $<HTMLInputElement>("sound-switch"),
  autostart: $<HTMLInputElement>("autostart-switch"),
};

const picker: PickerElements = {
  ctrl: $<HTMLInputElement>("mod-ctrl"),
  alt: $<HTMLInputElement>("mod-alt"),
  shift: $<HTMLInputElement>("mod-shift"),
  win: $<HTMLInputElement>("mod-win"),
  key: $<HTMLSelectElement>("hotkey-key"),
  clear: $<HTMLButtonElement>("hotkey-clear"),
  status: $("hotkey-status"),
  hint: $("hotkey-hint"),
  warning: $("hotkey-warning"),
};

/** Last `list_devices` result. Refreshed by its own call site (window open,
 *  after `set_device`), not by `state-changed` — see `refreshDevices`. */
let devices: DeviceInfo[] = [];

/** Device names and ids are attacker-adjacent data in the mundane sense:
 *  Windows lets anyone rename a capture device to anything at all in Sound
 *  settings. Interpolated into `innerHTML`, a name containing `&` renders
 *  wrong and one containing `<` silently swallows the rest of the dropdown.
 *  Built as elements with `textContent` instead, so the string is never
 *  parsed as markup. */
function option(value: string, label: string): HTMLOptionElement {
  const el = document.createElement("option");
  el.value = value;
  el.textContent = label;
  return el;
}

function renderDeviceOptions(selected: string | null) {
  const options = [option("", "System default")];

  // §5: a saved device absent from the enumerated list (e.g. unplugged)
  // must stay visible rather than the dropdown silently snapping to
  // "System default" and losing the user's setting.
  const known_ids = new Set(devices.map((d) => d.id));
  if (selected !== null && !known_ids.has(selected)) {
    options.push(option(selected, `${selected} (not connected)`));
  }
  for (const d of devices) {
    options.push(option(d.id, `${d.name}${d.is_default ? " (default)" : ""}`));
  }

  els.device.replaceChildren(...options);
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

  renderPicker(picker, {
    hotkey: s.hotkey,
    status: s.hotkey_status,
    bare: s.hotkey_is_bare_printable,
  });

  // `hook_error` wins over `last_error`: a device error is "the last thing
  // that went wrong" and clears on the next successful call, whereas a hook
  // that would not install means the hotkey does nothing for the rest of the
  // session (§7). Showing the transient one on top of the permanent one would
  // bury the more important message seconds after it appeared.
  //
  // The full text goes in `title` because the banner is a single fixed-width
  // line — see ERROR_MAX_CHARS — and the hook message carries the underlying
  // Win32 error at the end, which is worth keeping recoverable on hover.
  const banner = s.hook_error ?? s.last_error;
  els.error.hidden = banner === null;
  // The label is a CSS ::before, so the variant is a class rather than text.
  els.error.classList.toggle("banner--hook", s.hook_error !== null);
  if (banner !== null) {
    els.error.textContent = truncate(banner, ERROR_MAX_CHARS);
    els.error.title = banner;
  }

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

initPicker(picker);

onStateChanged(render);
// 30Hz. Touches only the meter bar and its readout — never re-renders the
// whole state on a level tick (amendment §7).
onLevel((db) => renderMeter(els.meter, els.meterValue, db));
// Hotplug (Task 9b). `refreshDevices` was left factored for exactly this
// (§5) — nothing else here changes. The current selection comes from the
// select element rather than a cached `AppState`: `renderDeviceOptions` has
// already written it there, including the synthetic "(not connected)" option
// for a saved-but-absent device, so reading it back preserves the user's
// choice instead of snapping the dropdown to "System default".
onDevicesChanged(() => refreshDevices(els.device.value || null));

// The key list must be in the DOM before the first `render`, or `renderPicker`
// finds no matching <option> for a saved binding and treats it as unavailable.
//
// The `catch` keeps that ordering from also coupling the two *failures*: without
// it, a rejected `list_bindable_keys` would leave the whole window on its
// hardcoded placeholders — no device list, no mode, no mute state, no error
// banner — with the status line still reading "Not set", which by then is a lie.
// A failure here should cost the dropdown and nothing else.
api
  .listBindableKeys()
  .catch(() => [])
  .then((groups) => {
    renderKeyOptions(picker.key, groups);
    return api.getState().then((s) => {
      render(s);
      refreshDevices(s.selected_device);
    });
  });
