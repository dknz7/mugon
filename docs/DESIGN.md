# mugon — Design Spec

**Date:** 2026-08-10
**Status:** Approved, pre-implementation

A Windows tray utility for muting and push-to-talking the microphone via a fully
customizable global hotkey.

---

## 1. Purpose

Control the system microphone from anywhere without alt-tabbing into whatever app
is holding it. The mute happens at the Windows audio endpoint, so it applies to
every application at once — Discord, Teams, OBS, browser calls — and is reflected
in Windows' own sound settings.

The app lives in the system tray. The window is a settings panel you open rarely.

### Non-goals

- No audio recording, routing, effects, or processing.
- No per-application mute. Endpoint-level only.
- No cross-platform support. Windows 10 1809+ / Windows 11 only.
- No cloud, accounts, telemetry, or update checks.
- **No mouse-button bindings.** Keyboard only — one `WH_KEYBOARD_LL` hook, no
  `WH_MOUSE_LL`. Considered and explicitly rejected for scope; `F13`–`F24` already
  solve the "key nothing else uses" problem (§4.4).

---

## 2. Tech stack

| Layer | Choice | Why |
|---|---|---|
| Core | Rust + `windows` crate | Everything hard here is a raw Win32 / Core Audio call. Direct bindings, no marshalling layer. |
| UI | Tauri v2 + WebView2 | WebView2 ships with Windows. Pixel-accurate control over the target design; the hero animation is CSS. |
| Frontend | Vanilla TypeScript + CSS | ~6 interactive controls. A framework would be more build config than application. |
| Notifications | `tauri-plugin-notification` | Wraps WinRT toasts, handles the AppUserModelID plumbing. |
| Autostart | `tauri-plugin-autostart` | `HKCU\...\CurrentVersion\Run`, no elevation needed. |

### Prerequisites (not currently installed on the dev machine)

- Rust toolchain via `rustup` (`stable-x86_64-pc-windows-msvc`)
- `cargo-tauri` CLI
- MSVC build tools (Visual Studio Build Tools, Desktop C++ workload)

Node 26.4.0 and WebView2 151 are already present.

---

## 3. Architecture

Single process. **The Rust core owns all state.** The webview is a pure view — it
renders what it is told and dispatches commands back. No state is duplicated in
JavaScript.

### Modules (`src-tauri/src/`)

| Module | Responsibility | Depends on |
|---|---|---|
| `main.rs` | Tauri setup, app lifecycle, wiring | all |
| `audio.rs` | Core Audio: enumerate endpoints, get/set mute, get/set volume, peak metering, hotplug notifications | `windows` |
| `hotkey.rs` | `WH_KEYBOARD_LL` hook, key-state tracking, combo matching, the bindable-key table | `windows` |
| `modes.rs` | Mode state machine. The sole authority on what the mute state should be. | `audio` (trait) |
| `notify.rs` | Toasts and the optional audible beep | tauri plugin, `windows` |
| `config.rs` | Settings serialization and persistence | `serde` |
| `tray.rs` | Tray icon, context menu, window show/create | tauri |

**Isolation rule:** `audio.rs` exposes a `MicControl` trait. `modes.rs` depends on
the trait, never on Core Audio directly, so the entire mode state machine is
testable against a fake device.

### Data flow

```
  [WH_KEYBOARD_LL hook thread]
              |  key down / key up
              v
        modes.rs (state machine)
              |  desired mute state
              v
        audio.rs (IAudioEndpointVolume::SetMute)
              |
              +--> notify.rs (toast + beep)
              +--> tray.rs (icon swap)
              +--> emit "state-changed" --> webview
```

### Command surface (frontend → Rust)

| Command | Signature |
|---|---|
| `get_state` | `() -> AppState` |
| `list_devices` | `() -> Vec<DeviceInfo>` |
| `set_device` | `(id: Option<String>)` — `None` means follow system default |
| `set_mode` | `(mode: Mode)` |
| `set_volume` | `(level: f32)` — 0.0–1.0 |
| `toggle_mute` | `()` — for the UI toggle switch |
| `list_bindable_keys` | `() -> Vec<KeyGroup>` — the picker's dropdown, static for the process's life |
| `set_hotkey` | `(ctrl, alt, shift, win, key: Option<String>) -> Result<(), String>` — `None` clears |
| `set_notification_prefs` | `(toast: bool, sound: bool)` |
| `set_autostart` | `(enabled: bool)` |

### Event surface (Rust → frontend)

| Event | Payload | Rate |
|---|---|---|
| `state-changed` | `AppState` | On change |
| `level` | `{ peak_db: f32 }` | ~30 Hz, only while window exists |
| `devices-changed` | `Vec<DeviceInfo>` | On hotplug |

`AppState` is small enough that one fat `state-changed` event is simpler and
cheaper than a family of granular ones.

---

## 4. Behaviour

### 4.1 Modes

**Mute Toggle** — hotkey press flips endpoint mute. Second press flips it back.
Key release does nothing. This is the default mode on first run.

**Push to Talk** — true PTT semantics:

- Selecting this mode **immediately mutes** the device.
- Key **down** → unmute (you are live).
- Key **up** → mute.
- Key auto-repeat is ignored; only state transitions are acted on.

**Manual mute controls are disabled in Push to Talk mode.** The UI toggle switch
and the tray's *Toggle Mute* item are both greyed out while PTT is active, because
the mode state machine is the sole authority on mute state (§3) and a manual
override would immediately be undone by the next key event. The UI shows PTT's
live/muted state as read-only.

### 4.2 Mode and lifecycle transitions

| Transition | Effect on mic |
|---|---|
| Enter Push to Talk | Mute immediately |
| Leave Push to Talk → Mute Toggle | Unmute |
| App quits (any mode, any means) | **Unmute** |
| App crashes | Best-effort unmute via panic hook; not guaranteed |

Leaving the user's microphone silently dead after the app closes is unacceptable.
Restore-on-exit is a hard requirement, wired to both the tray Quit path and
`WM_QUERYENDSESSION` (shutdown / logoff).

### 4.3 Stuck-key safety

If the hook misses a key-up — UAC prompt stealing focus, `Win+L`, an elevated
window taking the foreground — push-to-talk could latch open, leaving the mic live
without the user knowing.

Mitigations, all required:

1. While PTT is held, a watchdog polls `GetAsyncKeyState` at 250ms. If the key
   reads as physically up, force the release path.
2. ~~Subscribe to `WM_WTSSESSION_CHANGE`; session lock forces the release
   path.~~ **Resolved at Task 15: not implemented, and not needed.** The
   documented failure modes of `GetAsyncKeyState` are what make mitigation 1
   already cover this. It "returns zero if the call fails", and the listed
   reasons for failure are precisely the three hazards in this section:

   > - The current desktop is not the active desktop.
   > - UI Privilege Isolation (UIPI) prevents the calling thread from accessing
   >   the foreground thread.
   > - The foreground thread belongs to another process and the calling thread
   >   does not have `DESKTOP_HOOKCONTROL` or `DESKTOP_JOURNALRECORD` access to
   >   its desktop.

   A locked workstation and a UAC prompt both switch to a different desktop, so
   the poll returns zero, the key reads as up, and the release fires within
   250ms — sooner than a session-change message would arrive, and without
   needing an HWND that the app does not have while the settings window is
   destroyed (§4.10).

   The watchdog therefore treats "cannot tell" as "physically up". That is the
   deliberate direction: an unreadable key state re-mutes, which is at worst an
   interrupted sentence, where the alternative is a live microphone behind a
   lock screen.
3. Window focus loss does **not** trigger release — that would break legitimate
   use, since PTT is used precisely while other apps are focused. Enforced
   structurally: `modes::KeyObservation`, the watchdog's only input, has no
   focus field to consult.

### 4.4 Hotkey

Implemented as a **low-level keyboard hook** (`SetWindowsHookEx` / `WH_KEYBOARD_LL`),
not `RegisterHotKey`. `RegisterHotKey` only delivers `WM_HOTKEY` on key-down, which
makes push-to-talk impossible, and it cannot bind arbitrary keys.

```rust
struct Hotkey { ctrl: bool, alt: bool, shift: bool, win: bool, vk: u32 }
```

Serialized with a human-readable key name rather than a bare virtual-key code.

**Binding workflow — the user *picks*, they do not record.**

1. Four modifier chips (`Ctrl` / `Alt` / `Shift` / `Win`) and one grouped key
   dropdown. Any change writes immediately; there is no Save control.
2. The dropdown's first entry is `None`, which clears the binding
   (`hotkey: null`, no hotkey active).
3. A `HOTKEY STATUS` line reports one of four states — see below.

> **Superseded 2026-08-11 (Task 17).** This was originally a **Record** button
> that captured the next keypress. It was replaced because capture required
> mugon to observe a keystroke while its own window held focus, and that never
> worked in real use; three rounds of diagnosis reached no root cause. Picking
> observes no keystrokes at all, so it does not depend on the behaviour that
> failed. The fault itself is **unexplained, not fixed** —
> `.superpowers/sdd/2026-08-10-mugon/HANDOFF-task16.md` §6f is the evidence.
>
> Note the mechanism did **not** change: still `WH_KEYBOARD_LL` for firing.
> Utilities that offer a dropdown *and* force a two-key combo are typically
> using `RegisterHotKey`, which is the thing paragraph one rejects.

**Confirmation.** Recording had one virtue a dropdown lacks: pressing the key
proved it reached mugon. That matters most for `F13`–`F24`, which are not
physical keys on a standard board and arrive via a remapper that may simply not
be running. So a binding is not trusted until it has been *seen*:

| Status | Condition | Line |
|---|---|---|
| Inactive | the hook failed to install | `HOTKEY STATUS: Inactive — keyboard hook blocked` |
| Not set | no binding | `HOTKEY STATUS: Not set` |
| Bound | set, never observed | `HOTKEY STATUS: Bound — press {combo} to confirm` |
| Confirmed | observed firing at least once | `HOTKEY STATUS: Confirmed` |

`Inactive` outranks the rest: with no hook, no binding can ever be confirmed,
so prompting for a press would be an instruction that cannot succeed. The state
persists (`config.hotkey_confirmed`) and resets whenever the binding changes.
Confirmation hangs off **key-down** — matching is exact-modifier-set, and users
routinely release `Ctrl` before the key, so the key-up event often carries no
modifiers and matches nothing.

**Rules:**

- Bare single keys are permitted — `F13`, `ScrollLock` and `V` are all legitimate
  PTT bindings.
- The bound combo is **shared, not exclusive**: mugon fires on it, and the
  keystroke still reaches whatever application has focus. mugon never
  consumes the event to keep it from other applications.
- If the user binds a bare printable key, the picker shows a non-blocking
  warning: in Push-to-Talk, holding it now repeats that character into
  whatever is focused — hold `V` to talk and you type `vvvvvvv`. It does not
  prevent the binding. Printable covers letters, digits **and punctuation**.
- `Esc` cannot be bound — it is the universal way out, and is absent from the
  offered list. `Ctrl+Alt+Del` cannot be intercepted by any userspace process
  and is silently unbindable.
- A modifier can never be the *bound key*, or the binding would fire on every
  shortcut the user pressed all day. Enforced at both entrances to that field:
  `Core::set_hotkey` for the picker, `Hotkey`'s `Deserialize` for the config
  file.

#### Key coverage — explicit requirements

The offered list must cover **every** virtual-key code the hook can usefully
deliver, not a curated subset. It is generated from `hotkey::keys::NAMED` plus
the arithmetic letter/digit ranges — **the list the user is offered and the list
`set_hotkey` accepts are the same object.** Two lists is how `:` came to be
displayable nowhere and bindable never. Specifically required:

| Group | Range | Notes |
|---|---|---|
| Standard function keys | `VK_F1`–`VK_F12` | — |
| **Extended function keys** | **`VK_F13`–`VK_F24`** | **Hard requirement.** Immediately follows `F12` in the VK space. |
| Alphanumerics | `A`–`Z`, `0`–`9` | Triggers the bare-printable-key warning |
| Numpad | `VK_NUMPAD0`–`9`, operators | Distinct from the number row |
| Lock keys | `CapsLock`, `ScrollLock`, `NumLock` | Bindable. Pressing them toggles the lock, same as any other bound key toggling anything else it's normally bound to. |
| Navigation / editing | Arrows, `Home`/`End`/`PgUp`/`PgDn`, `Ins`/`Del` | — |
| Media & browser keys | `VK_MEDIA_*`, `VK_BROWSER_*`, `VK_VOLUME_*` | Delivered by the keyboard hook |
| Punctuation | `VK_OEM_1`–`VK_OEM_7`, `VK_OEM_PLUS/COMMA/MINUS/PERIOD` | Named for the unshifted US keycap character. Triggers the bare-printable warning. Added in Task 17 — their absence made `:` permanently unbindable. |

**Two implementation constraints, both non-obvious and both load-bearing:**

1. **Key names come from a hardcoded VK→string table.** Do *not* use
   `GetKeyNameTextW`. It derives its label from the scancode rather than the VK,
   and `F13`–`F24` have non-standard scancodes — a remapper emitting a synthetic
   or zeroed scancode makes it return an empty string. The key would capture
   correctly and then render as blank in the UI.

2. **Injected keystrokes must NOT be filtered.** `F13`–`F24` do not exist on
   standard keyboards; they are produced by remapping tools (PowerToys Keyboard
   Manager, AutoHotkey, gaming peripheral software), which deliver them via
   `SendInput` with the `LLKHF_INJECTED` flag set on the hook event. Discarding
   injected input — a common defensive instinct when writing a keyboard hook —
   would break extended-F-key support for every software-remapped setup while
   appearing to work fine on firmware-remapped hardware. This is a deliberate
   design decision, not an oversight.

VK constants are taken from the `windows` crate rather than hardcoded numerically,
so the values are compiler-checked against the Win32 headers.

**Known interaction:** with a *software* remapper, low-level hooks fire in
installation order. If the remapper's hook is installed after mugon's, mugon sees
the pre-remap key rather than the remapped one. Firmware-level remapping
(QMK/VIA, onboard peripheral memory) is unaffected, since the hardware genuinely
emits the extended scancode. Documented, not worked around.

### 4.5 Device selection

Dropdown lists all active capture endpoints via `IMMDeviceEnumerator::EnumAudioEndpoints`,
plus a **"System default"** entry at the top, which is the first-run default.

An `IMMNotificationClient` watches for device add/remove/default-change and emits
`devices-changed`.

### 4.6 Volume slider

Sets the selected endpoint's master capture volume via
`IAudioEndpointVolume::SetMasterVolumeLevelScalar` (0.0–1.0). This is a system
property of the device, read live on window open and on device change — mugon
does **not** persist it to config, because Windows already owns it and writing a
second source of truth would mean fighting the OS on every external change.

Unlike mute, volume is not touched by mode transitions or by app exit.

### 4.7 Audio level meter — hybrid strategy

Reads peak amplitude and displays it in dBFS.

- **Window closed:** passive metering only, via `IAudioMeterInformation::GetPeakValue`.
  Zero CPU, no audio stream, mugon does not appear in Windows' microphone privacy
  list. Reads zero unless another application is actively capturing.
- **Window open:** additionally opens a short-lived shared-mode WASAPI capture
  stream so the meter is always live. Windows shows the mic-in-use indicator
  **only while the settings window is open**, and the stream is torn down when the
  window closes.

**Conversion:** `db = 20 * log10(peak)`, clamped to a `-60 dBFS` floor. A peak of
zero maps to `-60`.

**Display:** horizontal bar plus numeric dBFS. Green below `-12`, amber `-12` to
`-3`, red above `-3`.

**Expected behaviour, not a bug:** the meter reads post-mute, so it goes flat when
muted. The UI labels this state explicitly rather than showing a dead meter.

### 4.8 Notifications

**Toasts** fire on mute state change in **Mute Toggle mode only**. Push-to-talk
deliberately does not toast — it would fire on every utterance.

Requires an AppUserModelID (`com.byron.mugon`) with a matching Start Menu
shortcut. Without the shortcut, Windows silently discards toasts. The NSIS
installer creates it; a first-run check creates it if missing.

**Beep** — two short embedded WAVs (mute / unmute), played via `PlaySound` with
`SND_MEMORY | SND_ASYNC`. Fires in both modes when enabled. Off by default.

Toast and beep are independently toggleable.

### 4.9 Tray

- **Icon** reflects live state: microphone (live) vs. struck-through microphone
  (muted). 16px and 32px ICO variants embedded.
- **Left click** → show/create the settings window.
- **Right click menu:** Open mugon · Toggle Mute · Mode ▸ (Mute Toggle /
  Push to Talk) · Quit.

  *Open mugon* is first because it is the item most often wanted, and it is the
  only fallback for reaching the window if the left-click handler misses —
  `show_menu_on_left_click(false)` means there is no other path in. It was
  originally labelled *Settings*, which promised the settings panel and
  delivered the main window; the panel is the cog flyout one click inside it.

### 4.10 Window lifecycle

The close button **destroys the webview** rather than hiding it. Reopening costs
a cold start, which is irrelevant for a panel opened this rarely.

**Measured at Task 15** against the installed 0.1.0 release build, whole process
tree, replacing the original "~10MB idle" estimate:

| State | Processes | Working set | Private bytes |
|---|---|---|---|
| Tray only, window never created (the autostart path) | 1 | 15.8 MB | 3.0 MB |
| Settings window open | 7 (6 × `msedgewebview2.exe`) | 365.5 MB | 189.8 MB |
| Window destroyed — the idle state | 1 | 28.1 MB | 7.1 MB |

The "~10MB" figure was optimistic on working set and about right on private
bytes. What matters is the shape, and it holds emphatically: closing the window
retires all six WebView2 processes and drops the tree from 365 MB to 28 MB.
Working set does not return all the way to the never-opened figure because
Windows does not trim a quiet process unless it needs the pages; private bytes,
the number that reflects what the app is actually holding, goes back to 7.1 MB.

Consequences: the `level` event stream and the WASAPI metering stream both start
on window create and stop on window destroy, which falls out of this design for
free.

`Quit` is only available from the tray menu.

### 4.11 Autostart

`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` via `tauri-plugin-autostart`.
No elevation required. When launched by autostart the app passes `--minimized` and
never creates a window — it goes straight to tray.

---

## 5. Visual design

Derived from the reference screenshot, with the purple palette replaced.

- **Base:** asphalt greys (near-black surface, elevated card, subtle borders)
- **Text:** off-white primary, muted grey secondary
- **Accent:** a single highlight colour for live/active states, used sparingly
- **Layout:** hero microphone → device dropdown → level meter → volume slider →
  mute toggle → mode dropdown → notification toggles → hotkey picker → settings
  cog link at the bottom

**Hero animation:** an SVG microphone with a looping pulse. Live state shows a slow
breathing glow ring in the accent colour; muted state collapses the ring,
desaturates the mic and overlays the slash. Pure CSS keyframes. Honours
`prefers-reduced-motion` by falling back to a static state.

The settings panel is reached via a **cog icon linked at the bottom** of the main
view.

---

## 6. Configuration

`%APPDATA%\mugon\config.json`

```json
{
  "version": 1,
  "device_id": null,
  "mode": "MuteToggle",
  "hotkey": { "ctrl": true, "alt": true, "shift": false, "win": false, "key": "M" },
  "hotkey_confirmed": false,
  "notifications": { "toast": true, "sound": false },
  "autostart": false
}
```

`device_id: null` means follow the system default capture device. `version` exists
so a future schema change can migrate rather than reset. Writes are atomic
(temp file + rename). A corrupt or unreadable config is backed up to
`config.json.bak` and replaced with defaults rather than crashing.

`hotkey_confirmed` (Task 17) records whether the hook has ever observed the bound
combo fire — see §4.4. It is `#[serde(default)]`, so **a config written before it
existed still loads**: it arrives `false`, which is honest, rather than tripping
the corrupt-file path and silently resetting the user's binding and device
choice. That fallback is the right behaviour for corruption and the wrong one for
an older schema, and the two are easy to confuse.

---

## 7. Error handling

| Condition | Handling |
|---|---|
| Selected device unplugged | Toast, fall back to system default, meter zeroes, UI updates |
| No capture devices at all | UI shows an empty state; hotkey is inert; no crash |
| Core Audio call fails | Log, surface a non-blocking UI error, keep last known state |
| Config corrupt or unparseable | Back up to `.bak`, load defaults |
| Toast fails (missing AUMID/shortcut) | Attempt shortcut creation once, then degrade silently to beep-only |
| Keyboard hook fails to install | Blocking error in the UI — the app's core function is dead without it |
| Another app swallows the hotkey first | Undetectable from userspace. Documented, not handled. |

---

## 8. Known limitations

Stated up front, not treated as bugs:

1. The keyboard hook does not fire while an **elevated window** holds focus,
   unless mugon itself runs elevated. This is a Windows security boundary and
   cannot be worked around from userspace.
1. **The hotkey does not fire while mugon's own window has focus.** Click any
   other window, or close mugon to the tray, and it works normally. The picker
   surfaces this as a hint under `HOTKEY STATUS`.

   **Root cause unknown.** Four rounds of instrumented diagnosis (Task 16) never
   explained it: nothing in mugon reads focus, the hook is global, and the
   evidence is in `.superpowers/sdd/2026-08-10-mugon/HANDOFF-task16.md` §6f. It
   was confirmed by the owner against a portable build on 2026-08-11 — the
   earlier note in §6f that a *bound* hotkey fires normally with mugon focused
   was based on a recollection he has since withdrawn.

   Accepted rather than fixed because it is deliberately unpursued, not because
   it is understood. The conventional fix is the second half of a standard
   pattern — global capture while unfocused, **local key events while focused** —
   which here means a WebView2 `keydown`/`keyup` path forwarding to the backend,
   with Chromium key identifiers mapped back to Windows VKs. That is a real
   piece of work, worst exactly at `F13`–`F24`, and Push-to-Talk needs the
   key-up edge too. Deferred, not dismissed.

   `RegisterHotKey` is **not** the answer here, for the reasons in §4.4's
   opening paragraph: key-down only, so no Push-to-Talk, and exclusive, so it
   would break the shared-not-consumed ruling.
2. Low-level keyboard hooks use the same API as keyloggers. Some antivirus
   heuristics may flag the binary. Code signing would reduce this; it is out of
   scope for v1.
3. `Ctrl+Alt+Del` and the secure desktop cannot be intercepted by any process.
4. Notifications require the Start Menu shortcut, so a fully portable
   single-`.exe` build cannot toast.

---

## 9. Testing

**Unit tested** (pure logic, no Win32):

- Hotkey combo matching, including modifier subset/superset cases
- Hotkey serialization round-trip
- VK→name table resolves a non-empty label for **every** bindable key, asserted
  across the full `F13`–`F24` range, numpad, lock and media keys
- Mode state machine against a fake `MicControl` — every transition in §4.2
- Key auto-repeat suppression
- Config round-trip, defaulting, and corrupt-file recovery
- Peak → dBFS conversion, including the zero and clamp cases

**Manual test checklist** (Win32 surfaces that cannot be meaningfully automated):

- Mute reflected in Windows Sound settings and in a live call
- Hotkey fires with the target app focused, and the keystroke still reaches it
- **`F13`–`F24` bind, display and fire correctly** — tested from both a
  software remapper (injected input) and firmware remapping if available.
  `HOTKEY STATUS` reaching `Confirmed` *is* the fire test.
- **The settings cog is reachable at 100%, 125% and 150% display scaling**, with
  the §4 worst case showing (bare printable key bound, error banner, long device
  name). `inner_size` is in *logical* pixels, so scaling multiplies the window
  height against a screen that does not grow: `tray::window_height` clamps to the
  monitor and `body { overflow-y: auto }` catches the remainder, and neither is
  exercised by any automated test.
- PTT holds and releases correctly under sustained hold
- Stuck-key watchdog recovers after a UAC prompt and after `Win+L`
- Device unplug/replug mid-session
- Toast appears; beep plays
- Autostart survives reboot and lands in tray with no window
- Tray icon reflects state; menu actions all work
- Mic is unmuted after quit, after logoff, and after shutdown

---

## 10. Build and distribution

`cargo tauri build` produces an **NSIS installer**. NSIS rather than
portable-exe specifically because the Start Menu shortcut is a hard requirement
for toasts (§4.8).

Release profile: `opt-level = "z"`, `lto = true`, `codegen-units = 1`,
`panic = "abort"`, stripped.

---

## 11. Open items

None. All design decisions resolved.
