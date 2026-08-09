// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // §4.2: a panic must not leave the user's microphone muted. This runs
    // before the default hook prints and unwinds, and is best-effort by
    // design — see `mugon_lib::emergency_unmute` for why it cannot use the
    // running audio worker or the shared state.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        mugon_lib::emergency_unmute();
        default_hook(info);
    }));

    mugon_lib::run()
}
