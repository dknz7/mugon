//! Low-level keyboard hook.
//!
//! DESIGN.md §4.4 mandates `WH_KEYBOARD_LL` rather than `RegisterHotKey`:
//! `RegisterHotKey` only delivers `WM_HOTKEY` on key-down, which makes
//! push-to-talk impossible, and it cannot bind arbitrary keys.

use super::Hotkey;
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_LMENU, VK_LWIN, VK_MENU, VK_RMENU, VK_RWIN, VK_SHIFT, VK_CONTROL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, HC_ACTION,
    KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookEvent {
    pub vk: u16,
    pub down: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

static SENDER: OnceLock<Sender<HookEvent>> = OnceLock::new();

/// True when the event exactly matches the binding — same key, and the modifier
/// set matches precisely. A superset must not fire, so Ctrl+Alt+M does not
/// trigger on Ctrl+Alt+Shift+M.
pub fn matches(binding: &Hotkey, ev: &HookEvent) -> bool {
    binding.vk == ev.vk
        && binding.ctrl == ev.ctrl
        && binding.alt == ev.alt
        && binding.shift == ev.shift
        && binding.win == ev.win
}

fn modifier_state() -> (bool, bool, bool, bool) {
    unsafe {
        let down = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| {
            (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0
        };
        (
            down(VK_CONTROL),
            down(VK_MENU) || down(VK_LMENU) || down(VK_RMENU),
            down(VK_SHIFT),
            down(VK_LWIN) || down(VK_RWIN),
        )
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);

        // DESIGN.md §4.4 — DO NOT add an `LLKHF_INJECTED` check here.
        // F13-F24 do not exist on standard keyboards; they arrive injected from
        // remapping tools (PowerToys, AutoHotkey, peripheral software). Filtering
        // injected input silently breaks extended-F-key support for every
        // software-remapped setup while still appearing to work on firmware
        // remaps. This omission is deliberate.

        let msg = wparam.0 as u32;
        let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
        let up = matches!(msg, WM_KEYUP | WM_SYSKEYUP);

        if down || up {
            let vk = kb.vkCode as u16;
            let (ctrl, alt, shift, win) = modifier_state();
            let ev = HookEvent { vk, down, ctrl, alt, shift, win };

            if let Some(tx) = SENDER.get() {
                let _ = tx.send(ev);
            }

            // DESIGN.md §4.4 — the hotkey is shared, not exclusive: this hook
            // only observes events, it never consumes them. Every event falls
            // through to `CallNextHookEx` below regardless of whether it
            // matches the bound combo. `matches()` still runs downstream (in
            // `lib.rs::handle_hook_event`, off the `HookEvent` sent above) to
            // decide whether to act on it — deciding whether to fire is not
            // the same thing as deciding whether to forward, and this hook
            // only ever did the latter.
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Installs the hook and runs its message loop. **Blocks forever** — call this on
/// a dedicated thread. A low-level hook requires a message pump on the thread
/// that installed it, or Windows silently stops delivering events.
pub fn install(tx: Sender<HookEvent>) -> Result<(), String> {
    let _ = SENDER.set(tx);
    unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0)
            .map_err(|e| format!("SetWindowsHookExW failed: {e}"))?;
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Polls the physical state of a key. Backs the stuck-key watchdog (§4.3).
///
/// **Reports `false` when it cannot tell, and that is the whole reason §4.3
/// needs no separate session-lock handler.** `GetAsyncKeyState` is documented
/// to return zero — not an error — when the current desktop is not the active
/// desktop, when UIPI blocks access to the foreground thread, or when the
/// calling thread lacks `DESKTOP_HOOKCONTROL` on the foreground thread's
/// desktop. A lock screen and a UAC prompt are both desktop switches, and an
/// elevated foreground window is the UIPI case, so all three of this section's
/// hazards make this function say "up" and the watchdog re-mute.
///
/// Erring towards "up" is the safe direction by design: the cost of being
/// wrong is a push-to-talk hold that ends early, against a live microphone
/// nobody can see.
pub fn is_physically_down(vk: u16) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::Hotkey;

    fn hk(ctrl: bool, alt: bool, vk: u16) -> Hotkey {
        Hotkey { ctrl, alt, shift: false, win: false, vk }
    }
    fn ev(ctrl: bool, alt: bool, vk: u16) -> HookEvent {
        HookEvent { vk, down: true, ctrl, alt, shift: false, win: false }
    }

    #[test]
    fn exact_match_fires() {
        assert!(matches(&hk(true, true, 0x4D), &ev(true, true, 0x4D)));
    }

    #[test]
    fn wrong_key_does_not_fire() {
        assert!(!matches(&hk(true, true, 0x4D), &ev(true, true, 0x4E)));
    }

    #[test]
    fn extra_modifier_does_not_fire() {
        // Ctrl+Alt+M must not fire on Ctrl+Alt+Shift+M.
        let mut e = ev(true, true, 0x4D);
        e.shift = true;
        assert!(!matches(&hk(true, true, 0x4D), &e));
    }

    #[test]
    fn missing_modifier_does_not_fire() {
        assert!(!matches(&hk(true, true, 0x4D), &ev(true, false, 0x4D)));
    }

    #[test]
    fn bare_f13_fires_with_no_modifiers() {
        assert!(matches(&hk(false, false, 0x7C), &ev(false, false, 0x7C)));
    }

    #[test]
    fn bare_binding_does_not_fire_when_a_modifier_is_held() {
        assert!(!matches(&hk(false, false, 0x7C), &ev(true, false, 0x7C)));
    }
}
