//! Low-level keyboard hook.
//!
//! DESIGN.md §4.4 mandates `WH_KEYBOARD_LL` rather than `RegisterHotKey`:
//! `RegisterHotKey` only delivers `WM_HOTKEY` on key-down, which makes
//! push-to-talk impossible, and it cannot bind arbitrary keys.

use super::{keys, Hotkey};
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

/// VKs currently swallowed on key-down, awaiting their matching key-up (see
/// `should_swallow_up` for why this must be a collection rather than a single
/// slot). Bounded by `MAX_PENDING_SWALLOWS` — see `record_swallowed`.
static PENDING_SWALLOWS: OnceLock<Mutex<Vec<u16>>> = OnceLock::new();

/// Caps `PENDING_SWALLOWS` so a key-down that is swallowed but whose key-up is
/// never observed by this hook (should not happen in normal use, but this
/// process does not control end-to-end delivery of every keyboard event)
/// cannot grow the list without bound. In practice at most one or two entries
/// are ever pending at once.
const MAX_PENDING_SWALLOWS: usize = 8;

fn binding_slot() -> &'static Mutex<Option<Hotkey>> {
    ACTIVE_BINDING.get_or_init(|| Mutex::new(None))
}

fn pending_slot() -> &'static Mutex<Vec<u16>> {
    PENDING_SWALLOWS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Updates the binding the hook swallows. `None` disables swallowing entirely
/// (used while the recorder is active, and when the user clears the hotkey).
pub fn set_binding(hk: Option<Hotkey>) {
    // Recovers a poisoned lock rather than propagating the panic: the
    // protected value is a plain `Option<Hotkey>` with no invariant a panic
    // mid-write could leave broken, and a panic unwinding out of `hook_proc`
    // (an `extern "system"` callback invoked by Windows) would be undefined
    // behaviour. See the identical rationale on every other lock site below.
    *binding_slot().lock().unwrap_or_else(|e| e.into_inner()) = hk;
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

/// True when a key-down event should be swallowed and its VK remembered for
/// the matching key-up.
///
/// Swallowing happens only on a full combo match (`matches()` — key AND exact
/// modifier set), not on VK alone: the naive VK-only check would make binding
/// Ctrl+M swallow every bare press of M too, breaking the ability to type the
/// letter M anywhere in Windows.
///
/// The `!keys::is_modifier(b.vk)` check is defence-in-depth. `binding.vk`
/// must never be a modifier VK — that invariant is enforced at both places a
/// `Hotkey` is produced (`Hotkey::deserialize` in `hotkey/mod.rs` rejects
/// modifier key names from config, and `Recorder::feed` in `recorder.rs`
/// refuses to record a modifier as the bound key), so this branch should be
/// unreachable in practice. It stays here anyway because if either of those
/// producers were ever changed to skip that validation, the consequence
/// would not be a cosmetic bug: swallowing a bare modifier means eating
/// Ctrl, Alt, Shift, or Win system-wide, silently breaking every other
/// keyboard shortcut on the machine. This function costs nothing extra to
/// keep that failure mode impossible regardless of what upstream validation
/// does or doesn't do.
fn should_swallow_down(binding: Option<&Hotkey>, ev: &HookEvent) -> bool {
    match binding {
        Some(b) => !keys::is_modifier(b.vk) && matches(b, ev),
        None => false,
    }
}

/// Records that `vk` was swallowed on key-down so its key-up is swallowed too.
/// Deduplicates (auto-repeat sends repeated key-downs for a held key) and
/// evicts the oldest entry once `MAX_PENDING_SWALLOWS` is reached.
fn record_swallowed(pending: &mut Vec<u16>, vk: u16) {
    if !pending.contains(&vk) {
        if pending.len() >= MAX_PENDING_SWALLOWS {
            pending.remove(0);
        }
        pending.push(vk);
    }
}

/// True when a key-up event's VK is pending (i.e. its key-down was
/// swallowed), removing it from `pending` if so.
///
/// `pending` is a collection rather than a single slot because a single slot
/// can leak: bind Ctrl+M, hold M (swallowed, recorded), rebind the hotkey
/// while M is still held, then press the new binding's key before M is
/// released — with a single slot the new key overwrites the record and M's
/// eventual key-up leaks through unpaired. A collection lets both survive
/// concurrently.
fn should_swallow_up(pending: &mut Vec<u16>, ev: &HookEvent) -> bool {
    if let Some(pos) = pending.iter().position(|&v| v == ev.vk) {
        pending.remove(pos);
        true
    } else {
        false
    }
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

            // Thin shell: all swallow decisions live in the pure, unit-tested
            // `should_swallow_down` / `should_swallow_up` / `record_swallowed`
            // functions above. Modifiers themselves are never swallowed — see
            // `should_swallow_down`'s doc comment.
            if down {
                let binding = *binding_slot().lock().unwrap_or_else(|e| e.into_inner());
                if should_swallow_down(binding.as_ref(), &ev) {
                    let mut pending = pending_slot().lock().unwrap_or_else(|e| e.into_inner());
                    record_swallowed(&mut pending, vk);
                    return LRESULT(1);
                }
            } else if up {
                let mut pending = pending_slot().lock().unwrap_or_else(|e| e.into_inner());
                if should_swallow_up(&mut pending, &ev) {
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
    fn ev_up(ctrl: bool, alt: bool, vk: u16) -> HookEvent {
        HookEvent { vk, down: false, ctrl, alt, shift: false, win: false }
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

    // --- should_swallow_down / should_swallow_up / record_swallowed ---

    #[test]
    fn full_combo_match_should_swallow_down() {
        let binding = hk(true, false, 0x4D); // Ctrl+M
        assert!(should_swallow_down(Some(&binding), &ev(true, false, 0x4D)));
    }

    #[test]
    fn bare_key_matching_only_the_bindings_vk_is_not_swallowed() {
        // Regression check for the original defect: binding Ctrl+M must not
        // swallow a bare, unmodified press of M.
        let binding = hk(true, false, 0x4D); // Ctrl+M
        assert!(!should_swallow_down(Some(&binding), &ev(false, false, 0x4D)));
    }

    #[test]
    fn modifier_superset_is_not_swallowed() {
        let binding = hk(true, false, 0x4D); // Ctrl+M
        let mut e = ev(true, false, 0x4D);
        e.shift = true; // Ctrl+Shift+M
        assert!(!should_swallow_down(Some(&binding), &e));
    }

    #[test]
    fn no_binding_never_swallows_down() {
        assert!(!should_swallow_down(None, &ev(true, false, 0x4D)));
    }

    #[test]
    fn modifier_named_binding_is_never_swallowed_down_defence_in_depth() {
        // The invariant that binding.vk is never a modifier is enforced at
        // both Hotkey producers (see should_swallow_down's doc comment), but
        // this test proves the defensive check independently holds even if a
        // modifier-VK Hotkey were constructed directly.
        let binding = Hotkey { ctrl: true, alt: false, shift: false, win: false, vk: 0x11 }; // VK_CONTROL
        let e = HookEvent { vk: 0x11, down: true, ctrl: true, alt: false, shift: false, win: false };
        // Without the defence-in-depth check, matches() alone would say true.
        assert!(matches(&binding, &e), "sanity: matches() would fire without the guard");
        assert!(!should_swallow_down(Some(&binding), &e));
    }

    #[test]
    fn key_up_whose_vk_is_pending_is_swallowed_and_removed() {
        let mut pending = vec![0x4D];
        assert!(should_swallow_up(&mut pending, &ev_up(false, false, 0x4D)));
        assert!(pending.is_empty(), "VK must be removed once its key-up is swallowed");
    }

    #[test]
    fn key_up_whose_vk_is_not_pending_is_not_swallowed() {
        let mut pending = vec![0x4D];
        assert!(!should_swallow_up(&mut pending, &ev_up(false, false, 0x4E)));
        assert_eq!(pending, vec![0x4D], "unrelated pending entry must be left alone");
    }

    #[test]
    fn record_swallowed_deduplicates() {
        let mut pending = Vec::new();
        record_swallowed(&mut pending, 0x4D);
        record_swallowed(&mut pending, 0x4D); // auto-repeat key-down
        assert_eq!(pending, vec![0x4D]);
    }

    #[test]
    fn record_swallowed_is_bounded() {
        let mut pending = Vec::new();
        for vk in 0..(MAX_PENDING_SWALLOWS as u16 + 5) {
            record_swallowed(&mut pending, vk);
        }
        assert_eq!(pending.len(), MAX_PENDING_SWALLOWS, "pending list must not grow unbounded");
    }

    #[test]
    fn rebind_while_key_held_does_not_leak_the_original_keys_up() {
        // Fix 2's target sequence: binding is Ctrl+M, user holds M (swallowed,
        // recorded). Binding changes (simulated here by simply swallowing a
        // second, unrelated key) while M is still held; the new key's
        // key-down is recorded too, before M's key-up ever arrives. With a
        // single-slot record this overwrites M's entry and its key-up would
        // leak through unpaired. With a Vec both survive.
        let mut pending = Vec::new();
        record_swallowed(&mut pending, 0x4D); // Ctrl+M's M, held
        record_swallowed(&mut pending, 0x7C); // new binding's key pressed before M released

        // M's key-up must still be recognised and swallowed.
        assert!(should_swallow_up(&mut pending, &ev_up(false, false, 0x4D)));
        // The new binding's key is still tracked afterwards.
        assert_eq!(pending, vec![0x7C]);
    }
}
