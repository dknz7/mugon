import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export interface DeviceInfo {
  id: string;
  name: string;
  is_default: boolean;
}

export type Mode = "MuteToggle" | "PushToTalk";

/** One `<optgroup>` in the key dropdown, straight from `hotkey::keys`. */
export interface KeyGroup {
  label: string;
  keys: string[];
}

/**
 * A binding as the picker's controls hold it. `key` is a **name** from
 * `list_bindable_keys` — never a raw VK — so the value the dropdown shows is
 * the value `set_hotkey` takes back.
 */
export interface HotkeyParts {
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  win: boolean;
  key: string;
}

/**
 * The `HOTKEY STATUS` line. `label` is rendered verbatim; `kind` is what the
 * styling switches on.
 *
 * They travel together so the frontend never has to infer one from the other.
 * Deriving the accent colour by prefix-matching `label` would make it an
 * undeclared enum — a copy edit on the Rust side would silently stop the
 * colouring, with every test on both sides still passing.
 */
export type HotkeyStatusKind = "Inactive" | "NotSet" | "Bound" | "Confirmed";

export interface HotkeyStatus {
  kind: HotkeyStatusKind;
  label: string;
}

/**
 * Mirrors `state::AppState`. There is **no** `devices` field: device
 * enumeration is its own command (`list_devices`), kept off the hot
 * snapshot that fires on every push-to-talk keypress. See
 * task-14-amendment.md §1 and src-tauri/src/state.rs's doc comment on
 * `AppState`.
 *
 * The frontend stores none of this — it re-renders wholesale on every
 * `state-changed`. See DESIGN.md §3.
 */
export interface AppState {
  selected_device: string | null;
  mode: Mode;
  muted: boolean;
  volume: number;
  hotkey_is_bare_printable: boolean;
  /** The binding split into what the four chips and the dropdown bind to. */
  hotkey: HotkeyParts | null;
  /**
   * The `HOTKEY STATUS` line. Render `label` as given — do not parse it and do
   * not rebuild it here (task-14-amendment §2); switch on `kind` instead.
   */
  hotkey_status: HotkeyStatus;
  manual_controls_enabled: boolean;
  notifications: { toast: boolean; sound: boolean };
  autostart: boolean;
  last_error: string | null;
  /**
   * Why the keyboard hook is not running, or `null` while it is.
   *
   * Separate from `last_error` on purpose: that one clears on the next
   * successful audio call, and a dead hook is a standing condition — the
   * hotkey does nothing until the app is restarted. Rendered in the same
   * banner, with precedence.
   */
  hook_error: string | null;
}

export const api = {
  getState: () => invoke<AppState>("get_state"),
  listDevices: () => invoke<DeviceInfo[]>("list_devices"),
  setDevice: (id: string | null) => invoke<void>("set_device", { id }),
  setMode: (mode: Mode) => invoke<void>("set_mode", { mode }),
  setVolume: (level: number) => invoke<void>("set_volume", { level }),
  toggleMute: () => invoke<void>("toggle_mute"),
  listBindableKeys: () => invoke<KeyGroup[]>("list_bindable_keys"),
  /** `key: null` clears the binding. */
  setHotkey: (ctrl: boolean, alt: boolean, shift: boolean, win: boolean, key: string | null) =>
    invoke<void>("set_hotkey", { ctrl, alt, shift, win, key }),
  setNotificationPrefs: (toast: boolean, sound: boolean) =>
    invoke<void>("set_notification_prefs", { toast, sound }),
  setAutostart: (enabled: boolean) => invoke<void>("set_autostart", { enabled }),
};

export const onStateChanged = (cb: (s: AppState) => void) =>
  listen<AppState>("state-changed", (e) => cb(e.payload));

export const onLevel = (cb: (peakDb: number) => void) =>
  listen<{ peak_db: number }>("level", (e) => cb(e.payload.peak_db));

/**
 * A capture device was added, removed, or became the new default.
 *
 * **No payload** — deliberately, superseding DESIGN.md §3's `DeviceInfo[]`.
 * The event is raised from a COM notification thread that must not block, and
 * enumerating devices means a round trip through the audio worker. So this is
 * a bare signal: call `list_devices` in response. See
 * `src-tauri/src/audio/hotplug.rs` and task-9b-brief.md §4.
 */
export const onDevicesChanged = (cb: () => void) =>
  listen<null>("devices-changed", () => cb());

