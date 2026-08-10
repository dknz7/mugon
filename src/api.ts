import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export interface DeviceInfo {
  id: string;
  name: string;
  is_default: boolean;
}

export type Mode = "MuteToggle" | "PushToTalk";

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
  hotkey_display: string | null;
  hotkey_is_bare_printable: boolean;
  manual_controls_enabled: boolean;
  notifications: { toast: boolean; sound: boolean };
  autostart: boolean;
  recording: boolean;
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
  beginRecording: () => invoke<void>("begin_hotkey_recording"),
  cancelRecording: () => invoke<void>("cancel_hotkey_recording"),
  clearHotkey: () => invoke<void>("clear_hotkey"),
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

export const onRecording = (cb: (active: boolean, combo: string | null) => void) =>
  listen<{ active: boolean; combo: string | null }>("hotkey-recording", (e) =>
    cb(e.payload.active, e.payload.combo),
  );
