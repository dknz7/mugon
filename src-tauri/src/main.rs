// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // §4.2: a panic must not leave the user's microphone muted. Best-effort by
    // design — see `mugon_lib::emergency_unmute` for why it cannot use the
    // running audio worker or the shared state.
    //
    // This is the *whole* crash-safety story under the release profile, which
    // sets `panic = "abort"` (§10): nothing unwinds, so no `Drop` runs, and
    // this hook is the only cleanup left. Panic hooks do still run before the
    // abort — verified rather than assumed, by
    // `cargo run --release --example panic_hook_abort`.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        mugon_lib::emergency_unmute();
        default_hook(info);
    }));

    mugon_lib::run()
}
