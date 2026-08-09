//! The hybrid level meter (§4.7): polls peak amplitude through a [`MicTap`]
//! at ~30Hz and emits a `level` event carrying its dBFS conversion.
//!
//! "Hybrid" is [`thread::MicHandle::start_metering`]/`stop_metering`'s job,
//! not this module's — this poll loop reads whatever `MicTap::peak()`
//! currently returns, whether that is a passive `GetPeakValue` read (no
//! capture stream open, window closed) or one kept live by an open
//! [`thread`]-owned capture stream (window open). This module does not know
//! or care which; it only polls and emits.
//!
//! Task 9 owns calling `start`/`stop` from window show/hide.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use super::peak_to_dbfs;
use super::thread::MicTap;

/// ~30Hz, per the plan's `level` event contract.
const POLL_INTERVAL: Duration = Duration::from_millis(33);

const LEVEL_EVENT: &str = "level";

/// The `level` event payload.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LevelPayload {
    pub peak_db: f32,
}

/// Peak amplitude -> dBFS -> event payload. Pulled out of the poll loop so it
/// is a plain, unit-testable function; the loop itself needs a live
/// `MicTap` and (in production) a real `AppHandle` to emit through, neither
/// of which this needs.
fn tick_payload(peak: f32) -> LevelPayload {
    LevelPayload { peak_db: peak_to_dbfs(peak) }
}

/// Owns the meter's poll thread. `start`/`stop` are both idempotent and both
/// safe to call from any state.
pub struct MeterHandle {
    running: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl MeterHandle {
    pub fn new() -> Self {
        Self { running: Arc::new(AtomicBool::new(false)), join: None }
    }

    /// Idempotent. Spawns the poll thread, requests the capture stream, and
    /// emits `level` at ~30Hz until [`Self::stop`].
    ///
    /// A failure to open the capture stream is logged and otherwise ignored
    /// — see `thread::Command::StartMetering` — the loop still starts and
    /// polls passively.
    pub fn start(&mut self, app: AppHandle, tap: MicTap) {
        self.start_with(tap, move |payload| {
            let _ = app.emit(LEVEL_EVENT, payload);
        });
    }

    /// The testable core of `start`: identical thread lifecycle, but takes an
    /// arbitrary sink instead of a concrete Tauri `AppHandle`. Emitting a
    /// real Tauri event needs a running `AppHandle`, which is awkward to
    /// construct in a unit test and not worth a test harness for — so this
    /// is where the untestable surface shrinks to exactly one line (the
    /// `app.emit(...)` closure `start` builds above). Everything else —
    /// spawn, poll cadence, the peak → dBFS → payload conversion, shutdown,
    /// and join — is exercised through this method in tests.
    fn start_with(&mut self, tap: MicTap, on_tick: impl Fn(LevelPayload) + Send + 'static) {
        if self.running.swap(true, Ordering::SeqCst) {
            return; // already running
        }

        if let Err(e) = tap.start_metering() {
            eprintln!("mugon: failed to start capture stream for metering: {e}");
        }

        let running = Arc::clone(&self.running);
        let spawned = std::thread::Builder::new().name("mugon-meter".into()).spawn(move || {
            while running.load(Ordering::SeqCst) {
                match tap.peak() {
                    Ok(peak) => on_tick(tick_payload(peak)),
                    // The worker is gone; there is nothing left to meter.
                    // Do not spin logging errors 30 times a second.
                    Err(_) => break,
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            // Reset in case the loop above broke on its own (worker died)
            // rather than via `stop()`, so a later `start()` is not fooled
            // into a no-op by a stale `true`.
            running.store(false, Ordering::SeqCst);
            // Release the capture stream deterministically before this
            // thread exits, so `stop()` joining it is enough to guarantee
            // the stream is gone by the time the caller moves on.
            let _ = tap.stop_metering();
        });

        match spawned {
            Ok(join) => self.join = Some(join),
            Err(e) => {
                self.running.store(false, Ordering::SeqCst);
                eprintln!("mugon: failed to spawn meter thread: {e}");
            }
        }
    }

    /// Idempotent. Stops the poll thread and blocks until it has actually
    /// exited, so the capture stream it held is released before this
    /// returns — not just flags-flipped-eventually.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Default for MeterHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::fake::FakeMic;
    use crate::audio::thread::MicHandle;
    use crate::audio::DBFS_FLOOR;
    use std::sync::Mutex;

    fn spawn_tap() -> (MicHandle, MicTap) {
        let mic = MicHandle::spawn_with(|| Ok(FakeMic::new())).expect("fake backend must spawn");
        let tap = mic.tap();
        (mic, tap)
    }

    #[test]
    fn tick_payload_converts_peak_to_the_expected_dbfs() {
        assert_eq!(tick_payload(1.0), LevelPayload { peak_db: 0.0 });
        assert_eq!(tick_payload(0.0), LevelPayload { peak_db: DBFS_FLOOR });
    }

    #[test]
    fn start_then_stop_terminates_the_poll_thread() {
        let (_mic, tap) = spawn_tap();
        let mut meter = MeterHandle::new();

        meter.start_with(tap, |_| {});
        assert!(meter.join.is_some(), "start must spawn the poll thread");

        meter.stop();
        assert!(meter.join.is_none(), "stop must join and take the handle");
        assert!(!meter.running.load(Ordering::SeqCst));
    }

    #[test]
    fn stop_without_a_prior_start_is_a_no_op_not_a_panic() {
        let mut meter = MeterHandle::new();
        meter.stop();
        assert!(meter.join.is_none());
    }

    #[test]
    fn start_is_idempotent_and_only_spawns_one_thread() {
        let (_mic, tap) = spawn_tap();
        let mut meter = MeterHandle::new();

        meter.start_with(tap.clone(), |_| {});
        // A second `start` while already running must not replace the
        // handle — if it did, the first thread's `JoinHandle` would leak and
        // this assertion would still pass by accident, so what actually
        // matters here is that `stop()` below joins cleanly exactly once.
        meter.start_with(tap, |_| {});
        assert!(meter.join.is_some());

        meter.stop();
        assert!(meter.join.is_none());
    }

    #[test]
    fn each_tick_emits_the_converted_payload() {
        // `_mic` (not `_`) so the handle — and with it the worker thread the
        // tap depends on — stays alive for the whole test instead of
        // dropping (and sending `Shutdown`) immediately.
        let (_mic, tap) = spawn_tap();

        let seen: Arc<Mutex<Vec<LevelPayload>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);

        let mut meter = MeterHandle::new();
        meter.start_with(tap, move |payload| seen2.lock().unwrap().push(payload));

        // Give the poll thread a few ticks.
        std::thread::sleep(POLL_INTERVAL * 5);
        meter.stop();

        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty(), "expected at least one tick to fire");
        // `FakeMic` defaults to a silent peak, so every payload must read as
        // the documented floor rather than anything else.
        assert!(seen.iter().all(|p| p.peak_db == DBFS_FLOOR), "got {seen:?}");
    }
}
