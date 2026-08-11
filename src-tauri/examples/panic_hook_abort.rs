//! Proves that the panic hook still restores the microphone under the release
//! profile's `panic = "abort"` (DESIGN.md §4.2, §10).
//!
//! `panic = "abort"` stops unwinding, so no `Drop` runs on a panic. Three
//! components document drop-based cleanup and none of them fire — accepted,
//! because Windows reclaims all three when the process dies. The microphone is
//! the exception: nothing reclaims a muted endpoint, and the only thing that
//! restores it on a crash is `main.rs`'s panic hook. Panic hooks are supposed
//! to run before the abort, but "supposed to" is not verification, and this is
//! the entire crash-safety story.
//!
//! Run it, and look for `microphone restored after a panic` **before** the
//! panic message:
//!
//! ```text
//! cargo run --release --example panic_hook_abort
//! ```
//!
//! Expected: both lines, then an abort (exit code `0xC0000409`, STATUS_FAIL_FAST).
//! A run that reaches the panic message with no restore line, or that aborts
//! silently, means the guarantee is gone.
//!
//! This unmutes the machine's configured capture device. That is the safe
//! direction and the whole point.

fn main() {
    // Byte-for-byte the hook `main.rs` installs; if that changes, this stops
    // testing the thing it claims to.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        mugon_lib::emergency_unmute();
        default_hook(info);
    }));

    eprintln!("panic_hook_abort: panicking now — the hook must restore the mic before the abort");
    panic!("deliberate panic: verifying the hook runs under panic = \"abort\"");
}
