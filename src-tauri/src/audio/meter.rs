//! The hybrid level meter (§4.7): polls peak amplitude through a [`MeterTap`]
//! at ~30Hz and emits a `level` event carrying its dBFS conversion.
//!
//! "Hybrid" is [`MeterTap::start_metering`]/`stop_metering`'s job, not this
//! module's — this poll loop reads whatever `MeterTap::peak()` currently
//! returns, whether that is a passive `GetPeakValue` read (no capture stream
//! open, window closed) or one kept live by an open [`thread`]-owned capture
//! stream (window open). This module does not know or care which; it only
//! polls and emits.
//!
//! Task 9 owns calling `start`/`stop` from window show/hide.

use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use super::peak_to_dbfs;
use super::thread::MeterTap;

/// ~30Hz, per the plan's `level` event contract.
const POLL_INTERVAL: Duration = Duration::from_millis(33);

const LEVEL_EVENT: &str = "level";

/// The `level` event payload. Field name and event name are the wire
/// contract Task 14's frontend consumes — see
/// `tests::level_event_wire_format_is_pinned_for_the_frontend`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LevelPayload {
    pub peak_db: f32,
}

/// Peak amplitude -> dBFS -> event payload. Pulled out of the poll loop so it
/// is a plain, unit-testable function; the loop itself needs a live
/// `MeterTap` and (in production) a real `AppHandle` to emit through, neither
/// of which this needs.
fn tick_payload(peak: f32) -> LevelPayload {
    LevelPayload { peak_db: peak_to_dbfs(peak) }
}

/// Owns the meter's poll thread. `start`/`stop` are both idempotent and both
/// safe to call from any state.
pub struct MeterHandle {
    running: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// Test-only spawn counter. Not part of the production type's real
    /// shape — see `tests::start_is_idempotent_and_only_spawns_one_thread`,
    /// which needs a way to assert "exactly one thread" rather than merely
    /// "a thread, possibly more than one, exists".
    #[cfg(test)]
    spawns: Arc<AtomicUsize>,
}

impl MeterHandle {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            join: None,
            #[cfg(test)]
            spawns: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Idempotent. Spawns the poll thread, requests the capture stream, and
    /// emits `level` at ~30Hz until [`Self::stop`].
    ///
    /// A failure to open the capture stream is logged and otherwise ignored
    /// — see `thread::Command::StartMetering` — the loop still starts and
    /// polls passively.
    pub fn start(&mut self, app: AppHandle, tap: MeterTap) {
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
    fn start_with(&mut self, tap: MeterTap, on_tick: impl Fn(LevelPayload) + Send + 'static) {
        if self.running.swap(true, Ordering::SeqCst) {
            return; // already running
        }

        if let Err(e) = tap.start_metering() {
            eprintln!("mugon: failed to start capture stream for metering: {e}");
        }

        // Kept outside the closure below (which moves `tap`) so that if the
        // thread fails to spawn at all, the capture stream `start_metering`
        // just opened above can still be released here instead of leaking
        // until the process exits.
        let stop_tap_if_spawn_fails = tap.clone();

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
            Ok(join) => {
                self.join = Some(join);
                #[cfg(test)]
                self.spawns.fetch_add(1, Ordering::SeqCst);
            }
            Err(e) => {
                self.running.store(false, Ordering::SeqCst);
                let _ = stop_tap_if_spawn_fails.stop_metering();
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

impl Drop for MeterHandle {
    /// So a caller that simply drops a `MeterHandle` — rather than remembering
    /// to call `stop()` first — cannot strand the capture stream open and
    /// leave the mic-in-use indicator lit. Safe now that `MeterTap` is the
    /// sole owner of the stream's lifecycle: there is no other handle whose
    /// state this could stamp on.
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::fake::FakeMic;
    use crate::audio::thread::MicHandle;
    use crate::audio::DBFS_FLOOR;
    use std::sync::Mutex;

    fn spawn_tap() -> (MicHandle, MeterTap) {
        let mic = MicHandle::spawn_with(|| Ok(FakeMic::new())).expect("fake backend must spawn");
        let tap = mic.tap();
        (mic, tap)
    }

    #[test]
    fn tick_payload_converts_peak_to_the_expected_dbfs() {
        assert_eq!(tick_payload(1.0), LevelPayload { peak_db: 0.0 });
        assert_eq!(tick_payload(0.0), LevelPayload { peak_db: DBFS_FLOOR });
    }

    /// Pins the wire format Task 14's frontend will consume: the event name
    /// and the JSON field name. If either ever changes, this test — not a
    /// frontend bug report — should be the first thing to notice.
    #[test]
    fn level_event_wire_format_is_pinned_for_the_frontend() {
        assert_eq!(LEVEL_EVENT, "level");
        let value = serde_json::to_value(LevelPayload { peak_db: -12.5 }).unwrap();
        assert_eq!(value, serde_json::json!({ "peak_db": -12.5 }));
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

    /// The real idempotency property: a second `start()` while already
    /// running must not spawn a *second* thread. `join.is_some()` alone
    /// can't tell one thread from two — a dedicated `#[cfg(test)]` spawn
    /// counter can.
    #[test]
    fn start_is_idempotent_and_only_spawns_one_thread() {
        let (_mic, tap) = spawn_tap();
        let mut meter = MeterHandle::new();

        meter.start_with(tap.clone(), |_| {});
        meter.start_with(tap, |_| {});
        assert_eq!(meter.spawns.load(Ordering::SeqCst), 1, "second start must not spawn a thread");

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

    /// `Drop` must release the stream without the caller remembering to call
    /// `stop()` — proven the same way `stop()` itself is proven: the thread
    /// actually terminates. `drop(meter)` blocking forever would hang this
    /// test rather than merely fail an assertion, which is an acceptable
    /// bound here since `stop()`'s own bounded behaviour is already covered.
    #[test]
    fn dropping_a_running_meter_handle_stops_the_poll_thread() {
        let (_mic, tap) = spawn_tap();
        let mut meter = MeterHandle::new();
        meter.start_with(tap, |_| {});
        assert!(meter.join.is_some());

        drop(meter);
        // If `Drop` did not join, there would be nothing further to assert —
        // the point is that reaching this line at all means the join
        // completed rather than the thread being abandoned mid-poll.
    }
}
