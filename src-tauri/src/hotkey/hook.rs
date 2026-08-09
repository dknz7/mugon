//! Low-level keyboard hook.
//!
//! DESIGN.md §4.4 mandates `WH_KEYBOARD_LL` rather than `RegisterHotKey`:
//! `RegisterHotKey` only delivers `WM_HOTKEY` on key-down, which makes
//! push-to-talk impossible, and it cannot bind arbitrary keys.

use super::Hotkey;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
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

/// The binding the hook currently swallows. `None` disables swallowing entirely
/// (used while the recorder is active, and when the user clears the hotkey).
static ACTIVE_BINDING: OnceLock<Mutex<Option<Hotkey>>> = OnceLock::new();
static SENDER: OnceLock<Sender<HookEvent>> = OnceLock::new();

/// VK of the key currently swallowed on key-down, so the matching key-up can
/// also be swallowed even though by the time it arrives the modifiers that
/// made it match may already have been released (see `hook_proc` below for
/// why matching key-up on VK alone, rather than modifiers-and-all, matters).
static SWALLOWED_VK: OnceLock<Mutex<Option<u16>>> = OnceLock::new();

fn binding_slot() -> &'static Mutex<Option<Hotkey>> {
    ACTIVE_BINDING.get_or_init(|| Mutex::new(None))
}

fn swallowed_vk_slot() -> &'static Mutex<Option<u16>> {
    SWALLOWED_VK.get_or_init(|| Mutex::new(None))
}

/// Updates the binding the hook swallows. `None` disables swallowing entirely
/// (used while the recorder is active, and when the user clears the hotkey).
pub fn set_binding(hk: Option<Hotkey>) {
    *binding_slot().lock().unwrap() = hk;
}

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

            // Swallow logic (corrected from the original brief — see task-7
            // report for the defect this replaces):
            //
            // The naive approach swallows on VK match alone, regardless of
            // modifiers. That means binding Ctrl+M swallows every bare press
            // of M too, breaking the ability to type the letter M anywhere in
            // Windows. Instead we swallow key-down only when the FULL combo
            // matches (`matches()` — key AND exact modifier set), and record
            // which VK we swallowed. On key-up we swallow if and only if the
            // VK matches what we recorded, because by key-up time the
            // modifiers may already have been released and so would no
            // longer match the binding even though this is genuinely the
            // release of the key we swallowed on the way down.
            //
            // Modifiers themselves are never swallowed — that would break
            // every other shortcut on the system.
            if down {
                if let Some(binding) = *binding_slot().lock().unwrap() {
                    if matches(&binding, &ev) {
                        *swallowed_vk_slot().lock().unwrap() = Some(vk);
                        return LRESULT(1);
                    }
                }
            } else if up {
                let mut swallowed = swallowed_vk_slot().lock().unwrap();
                if *swallowed == Some(vk) {
                    *swallowed = None;
                    return LRESULT(1);
                }
            }
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
