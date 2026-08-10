# mugon

A Windows system-tray microphone mute and push-to-talk utility. It sits in the
tray, binds one hotkey, and controls your **actual microphone** at the Windows
endpoint level — so every application sees the mute, not just the one you were
in when you pressed it.

Two modes:

- **Mute Toggle** — press the hotkey to mute, press again to unmute. Rests
  unmuted.
- **Push to Talk** — the microphone is muted at rest and live only while the
  hotkey is held.

Plus a device picker, an input-volume slider, a live level meter, optional
toast and beep feedback, and run-at-login.

---

## Install

Download `mugon_0.1.0_x64-setup.exe` (1.12 MiB) and run it. It is a per-user
install — no elevation, nothing written outside your profile.

- Application: `%LOCALAPPDATA%\mugon\`
- Settings: `%APPDATA%\mugon\config.json`
- Start Menu shortcut: created by the installer, and **required for toasts** —
  see limitation 4 below.

Uninstall from Settings → Apps, or run `%LOCALAPPDATA%\mugon\uninstall.exe`.

## Using it

Left-click the tray icon for the settings window. Right-click it for Toggle
Mute, the mode switch, Settings, and Quit. **Quit is only on the tray menu** —
closing the settings window destroys it and leaves mugon running in the tray,
which is what keeps the idle footprint small and Windows' microphone-in-use
indicator dark.

mugon restores the mic to unmuted on its way out. This is verified for a Rust
panic — a panic hook unmutes the endpoint before the process dies — and
implemented, but not end-to-end tested, for tray Quit, logoff and shutdown. The
one case where it cannot work is a hard kill (Task Manager's "End task"), which
runs no cleanup code in any application; if that happens, unmute from the
Windows sound settings.

---

## The hotkey is shared, not exclusive

**mugon does not consume your keystroke.** The bound key fires mugon *and*
still reaches whatever application is focused, exactly as if mugon were not
running. This is deliberate: it means you can bind a key your game or your
comms app already uses and both will respond.

The consequence to be aware of is the other direction. If you bind a bare
printable character — say `M` — then pressing it mutes your mic **and types an
`M`**, and in Push to Talk holding it types `M` for as long as you hold. The
settings window warns you when you record a binding like that. It is not
stopping you, because for a key bound to a game action that behaviour is the
entire point; it is telling you so the first time you use it in a chat box is
not a surprise.

## Recommended binding: F13–F24

These are real virtual-key codes that no standard keyboard has physical keys
for, so nothing else in Windows is listening for them. Nothing types, nothing
scrolls, nothing opens a menu. That makes them the ideal mute key, and mugon
supports the whole `F13`–`F24` range.

To get one:

- **Firmware remapping — strongly preferred.** A keyboard with onboard remapping
  (QMK/VIA, or a vendor configurator) can emit `F13` directly. The key then
  behaves like any other real key and mugon sees it with no extra software
  running. A Keychron Q6 HE, for example, can put `F13`–`F16` on physical keys.
- **Software remapping — works, but less reliably.** PowerToys Keyboard
  Manager, AutoHotkey and similar tools synthesise the keypress. mugon
  deliberately does *not* filter injected input, so these work — but whether
  mugon sees the remapped key depends on **hook install order**: both tools
  install low-level keyboard hooks, and if the remapper's hook is installed
  after mugon's, mugon sees the original key rather than the `F13` it was
  remapped to. Launching the remapper first usually fixes it. Firmware
  remapping has no such failure mode.

---

## Known limitations

Stated up front, because none of these are fixable and all of them are better
known than discovered:

1. **The hotkey does not fire while an elevated window has focus** — unless
   mugon itself is running elevated. Low-level keyboard hooks from a
   normal-privilege process do not receive input destined for a higher-privilege
   window. This is a Windows security boundary (UIPI), not a bug, and it cannot
   be worked around from userspace. Practically: click on a non-elevated window
   first, or run mugon elevated if you accept the tradeoff.
2. **Some antivirus heuristics may flag the binary.** A low-level keyboard hook
   is the same API a keylogger uses, and mugon installs one because it is the
   only way to get key-*up* events, which push-to-talk requires. Code signing
   would reduce false positives; it is out of scope for v1. The source is here
   if you would rather build it yourself.
3. **`Ctrl+Alt+Del` and the secure desktop cannot be intercepted** by any
   process, mugon included. A push-to-talk hold interrupted by one — or by a UAC
   prompt or `Win+L` — should be caught by the stuck-key watchdog, which polls
   the key every 250ms and re-mutes rather than leaving the mic open. That is
   expected rather than measured: the watchdog is verified against a hold
   latched open by other means (it recovered in 222ms), and the secure-desktop
   case rests on `GetAsyncKeyState` being documented to report zero when the
   active desktop is not ours. It has not been tested against a real lock
   screen. If you find your mic still live after unlocking, that is a bug worth
   reporting.
4. **A portable single-`.exe` cannot toast.** Windows only delivers toast
   notifications for an application with an AppUserModelID backed by a matching
   Start Menu shortcut, and only the installer creates that shortcut. Run the
   installer if you want toasts. The beep has no such requirement and works
   either way.

---

## Build from source

Needs the Rust toolchain (MSVC target), Node, and the WebView2 runtime (present
on any current Windows 11).

```bash
npm install
npx tauri build
```

The NSIS installer lands in `src-tauri/target/release/bundle/nsis/`, at roughly
1.12 MiB for 0.1.0.

For a debug run, `npx tauri dev`. Tests:

```bash
cd src-tauri
cargo test
```

Note that toasts do **not** appear in a dev build — see limitation 4. They can
only be tested against an installed one.

## Licence

Unlicensed personal project.
