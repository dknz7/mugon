//! Thread confinement for the audio backend.
//!
//! [`super::endpoint::Endpoint`] holds windows-rs COM interfaces. They are
//! `!Send` and apartment-bound, and tao puts Tauri's window thread in an **STA**
//! by calling `OleInitialize`, so the endpoint cannot live there and cannot be
//! moved around behind a lock either. Meanwhile [`MicControl`] requires `Send`
//! and Task 9 needs the whole thing inside a Tauri-managed `Mutex`.
//!
//! Resolution: a dedicated thread enters the MTA, constructs the backend, and
//! owns it permanently, servicing commands over a channel. [`MicHandle`] holds
//! nothing but channel endpoints and a `JoinHandle`, so it is `Send` on its own
//! merits. **No `unsafe impl Send` anywhere** — that is the entire point of this
//! module.
//!
//! The worker is generic over [`MicBackend`], so the plumbing is testable
//! against `fake::FakeMic` with no audio hardware.

use std::marker::PhantomData;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED,
};

use super::endpoint::Endpoint;
use super::{AudioError, DeviceInfo, MicBackend};

/// The one error every "the worker is gone" path produces. Callers include
/// Tauri command handlers and the keyboard-hook dispatch thread, so a dead
/// worker must degrade to this rather than panicking either of them.
fn worker_gone() -> AudioError {
    AudioError::Windows("audio thread terminated".into())
}

/// One variant per [`MicBackend`] method, each carrying the reply channel for
/// that call. A fresh reply channel per call costs microseconds and keeps
/// replies impossible to mis-route.
enum Command {
    ListDevices(Sender<Result<Vec<DeviceInfo>, AudioError>>),
    /// Owns a `String`, not a `&str` — it crosses a thread boundary.
    Select(Option<String>, Sender<Result<(), AudioError>>),
    IsMuted(Sender<Result<bool, AudioError>>),
    SetMuted(bool, Sender<Result<(), AudioError>>),
    Volume(Sender<Result<f32, AudioError>>),
    SetVolume(f32, Sender<Result<(), AudioError>>),
    Peak(Sender<Result<f32, AudioError>>),
    Shutdown,
}

/// RAII membership of the multi-threaded apartment for the current thread.
///
/// `CoInitializeEx` is reference counted per thread; this type exists so the
/// worker's call is balanced by exactly one `CoUninitialize` on the way out
/// instead of being silently leaked at thread exit.
///
/// Not `Send`: the uninitialise must happen on the very thread that initialised.
struct ComApartment(PhantomData<*const ()>);

impl ComApartment {
    /// Enters the MTA. `RPC_E_CHANGED_MODE` is a hard error: it means something
    /// already put this brand-new thread in another apartment, and continuing
    /// would leave every COM object apartment-bound to a thread we do not
    /// control. `S_FALSE` means "already initialised in this same mode", which
    /// is success and still takes a reference.
    fn enter() -> Result<Self, AudioError> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Err(AudioError::Windows(
                "audio thread is already in a different COM apartment \
                 (RPC_E_CHANGED_MODE)"
                    .into(),
            ));
        }
        if hr != S_OK && hr != S_FALSE {
            return Err(AudioError::Windows(format!(
                "CoInitializeEx failed on the audio thread: {}",
                hr.message()
            )));
        }
        Ok(Self(PhantomData))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// The production backend: a real [`Endpoint`] plus the worker thread's MTA
/// membership.
///
/// Field order is load-bearing. Rust drops fields in declaration order, so the
/// endpoint releases its COM interfaces *before* `_apartment` calls
/// `CoUninitialize`. Reversing these would tear down COM underneath live
/// interface pointers.
///
/// Be aware that the invariant is currently **inert**: `Endpoint::new` takes a
/// second, unreleased COM reference of its own (see the `CoInitializeEx` call in
/// `endpoint.rs`), so this thread's count never actually reaches zero and
/// `ComApartment::drop` tears nothing down. Swapping these two fields today
/// would not crash — it would crash the moment somebody fixes that imbalance.
/// Which is exactly why the order is written down rather than discovered later.
struct MtaEndpoint {
    endpoint: Endpoint,
    _apartment: ComApartment,
}

impl MtaEndpoint {
    fn new() -> Result<Self, AudioError> {
        // Order matters the other way round here: enter the apartment first so
        // that if `Endpoint::new` fails, the guard drops and releases it.
        let apartment = ComApartment::enter()?;
        let endpoint = Endpoint::new()?;
        Ok(Self { endpoint, _apartment: apartment })
    }
}

impl MicBackend for MtaEndpoint {
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        self.endpoint.list_devices()
    }
    fn select(&mut self, id: Option<&str>) -> Result<(), AudioError> {
        self.endpoint.select(id)
    }
    fn is_muted(&self) -> Result<bool, AudioError> {
        self.endpoint.is_muted()
    }
    fn set_muted(&mut self, muted: bool) -> Result<(), AudioError> {
        self.endpoint.set_muted(muted)
    }
    fn volume(&self) -> Result<f32, AudioError> {
        self.endpoint.volume()
    }
    fn set_volume(&mut self, level: f32) -> Result<(), AudioError> {
        self.endpoint.set_volume(level)
    }
    fn peak(&self) -> Result<f32, AudioError> {
        self.endpoint.peak()
    }
}

/// A `Send` proxy for a thread-confined [`MicBackend`].
///
/// Holds only channel endpoints, so `Send` falls out of the field types — see
/// the compile-time assertion below.
pub struct MicHandle {
    tx: Sender<Command>,
    join: Option<JoinHandle<()>>,
}

/// Task 9 puts `MicHandle` in a Tauri-managed `Mutex` that must be `Send + Sync`
/// (`Mutex<T>: Sync` needs `T: Send`). Assert it here so a future field that is
/// not `Send` fails at this line rather than three tasks later.
///
/// The second assertion is the other half: `MicControl` is `MicBackend + Send`,
/// so it is reached through the blanket impl and only while `MicHandle` stays
/// `Send`. Losing `Send` fails both lines.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    fn assert_mic_control<T: super::MicControl>() {}
    assert_send::<MicHandle>();
    assert_mic_control::<MicHandle>();
};

impl MicHandle {
    /// Spawns the audio thread against the real Core Audio endpoint.
    ///
    /// Blocks until the worker has entered the MTA and constructed the
    /// endpoint, so a machine with no capture device fails here at startup
    /// rather than silently on the first hotkey press.
    pub fn spawn() -> Result<Self, AudioError> {
        Self::spawn_with(MtaEndpoint::new)
    }

    /// Spawns the worker over any backend, constructed **on the worker thread**.
    ///
    /// The factory runs there rather than the caller passing a built backend,
    /// because a real `Endpoint` is `!Send` and could not cross the boundary.
    fn spawn_with<B, F>(make: F) -> Result<Self, AudioError>
    where
        B: MicBackend + 'static,
        F: FnOnce() -> Result<B, AudioError> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<Command>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), AudioError>>();

        let join = std::thread::Builder::new()
            .name("mugon-audio".into())
            .spawn(move || run(make, ready_tx, rx))
            .map_err(|e| AudioError::Windows(format!("could not spawn audio thread: {e}")))?;

        // A `RecvError` here means the worker died without reporting, which is
        // still a startup failure — never an unwrap.
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { tx, join: Some(join) }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                let _ = join.join();
                Err(worker_gone())
            }
        }
    }

    /// Every call goes through here: build a reply channel, send, block on the
    /// reply. Both the send and the receive degrade to [`worker_gone`] — there
    /// is deliberately no `unwrap` on any channel operation in this file.
    fn call<T>(
        &self,
        build: impl FnOnce(Sender<Result<T, AudioError>>) -> Command,
    ) -> Result<T, AudioError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx.send(build(reply_tx)).map_err(|_| worker_gone())?;
        reply_rx.recv().map_err(|_| worker_gone())?
    }
}

/// `MicHandle` implements the backend trait like everything else; because it is
/// `Send` (holding nothing but channel endpoints), the blanket impl in the
/// parent module hands it [`super::MicControl`] for free. It is the only type in
/// the tree that is both.
impl MicBackend for MicHandle {
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        self.call(Command::ListDevices)
    }
    fn select(&mut self, id: Option<&str>) -> Result<(), AudioError> {
        let id = id.map(str::to_owned);
        self.call(|reply| Command::Select(id, reply))
    }
    fn is_muted(&self) -> Result<bool, AudioError> {
        self.call(Command::IsMuted)
    }
    fn set_muted(&mut self, muted: bool) -> Result<(), AudioError> {
        self.call(|reply| Command::SetMuted(muted, reply))
    }
    fn volume(&self) -> Result<f32, AudioError> {
        self.call(Command::Volume)
    }
    fn set_volume(&mut self, level: f32) -> Result<(), AudioError> {
        self.call(|reply| Command::SetVolume(level, reply))
    }
    fn peak(&self) -> Result<f32, AudioError> {
        self.call(Command::Peak)
    }
}

impl Drop for MicHandle {
    /// Asks the worker to stop and waits for it, so the backend (and with it
    /// the COM apartment) is torn down before the handle's owner moves on.
    /// Both steps ignore failure: the worker may already be gone.
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// The worker thread body: construct, report, serve, tear down.
fn run<B, F>(make: F, ready: Sender<Result<(), AudioError>>, rx: Receiver<Command>)
where
    B: MicBackend,
    F: FnOnce() -> Result<B, AudioError>,
{
    let mut backend = match make() {
        Ok(backend) => {
            // If the report cannot be delivered, `spawn_with` has given up on
            // us; drop the backend and exit rather than serving a caller that
            // does not exist.
            if ready.send(Ok(())).is_err() {
                return;
            }
            backend
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    // `recv` returning `Err` means every sender is gone — the handle was
    // dropped without a clean shutdown. Same exit path either way.
    while let Ok(command) = rx.recv() {
        match command {
            Command::ListDevices(reply) => {
                let _ = reply.send(backend.list_devices());
            }
            Command::Select(id, reply) => {
                let _ = reply.send(backend.select(id.as_deref()));
            }
            Command::IsMuted(reply) => {
                let _ = reply.send(backend.is_muted());
            }
            Command::SetMuted(muted, reply) => {
                let _ = reply.send(backend.set_muted(muted));
            }
            Command::Volume(reply) => {
                let _ = reply.send(backend.volume());
            }
            Command::SetVolume(level, reply) => {
                let _ = reply.send(backend.set_volume(level));
            }
            Command::Peak(reply) => {
                let _ = reply.send(backend.peak());
            }
            Command::Shutdown => break,
        }
    }
    // `backend` drops here, releasing the COM interfaces and then the apartment.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::fake::FakeMic;
    use std::sync::{Arc, Mutex};

    /// Test-only introspection seam.
    ///
    /// The worker owns its backend outright, which is exactly what makes the
    /// design sound — so reading `FakeMic::mute_calls` back out needs a
    /// deliberate mechanism. This wrapper is it: the test keeps an `Arc` clone
    /// and the worker drives the fake through the mutex.
    ///
    /// Chosen over a test-gated `Command::Inspect` variant or having the worker
    /// hand the backend back on shutdown, because both of those add shape to
    /// production types (`Command`, the worker's return type, `MicHandle::drop`)
    /// purely to serve tests. This adds nothing outside `#[cfg(test)]`, and it
    /// lets a test observe the fake *while* the worker is still running rather
    /// than only after it has stopped.
    struct SharedFake(Arc<Mutex<FakeMic>>);

    impl SharedFake {
        fn new() -> (Self, Arc<Mutex<FakeMic>>) {
            let inner = Arc::new(Mutex::new(FakeMic::new()));
            (Self(Arc::clone(&inner)), inner)
        }

        fn with<T>(&self, f: impl FnOnce(&mut FakeMic) -> T) -> Result<T, AudioError> {
            match self.0.lock() {
                Ok(mut guard) => Ok(f(&mut guard)),
                Err(_) => Err(AudioError::Windows("fake mic mutex poisoned".into())),
            }
        }
    }

    impl MicBackend for SharedFake {
        fn list_devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            self.with(|m| m.list_devices())?
        }
        fn select(&mut self, id: Option<&str>) -> Result<(), AudioError> {
            self.with(|m| m.select(id))?
        }
        fn is_muted(&self) -> Result<bool, AudioError> {
            self.with(|m| m.is_muted())?
        }
        fn set_muted(&mut self, muted: bool) -> Result<(), AudioError> {
            self.with(|m| m.set_muted(muted))?
        }
        fn volume(&self) -> Result<f32, AudioError> {
            self.with(|m| m.volume())?
        }
        fn set_volume(&mut self, level: f32) -> Result<(), AudioError> {
            self.with(|m| m.set_volume(level))?
        }
        fn peak(&self) -> Result<f32, AudioError> {
            self.with(|m| m.peak())?
        }
    }

    /// Spawns a worker over a shared `FakeMic` and returns the handle plus a
    /// window onto the fake's recorded state.
    fn spawn_fake() -> (MicHandle, Arc<Mutex<FakeMic>>) {
        let (backend, view) = SharedFake::new();
        let handle = MicHandle::spawn_with(move || Ok(backend)).expect("fake backend must spawn");
        (handle, view)
    }

    #[test]
    fn set_muted_then_is_muted_round_trips_through_the_channel() {
        let (mut mic, _view) = spawn_fake();
        assert!(!mic.is_muted().unwrap(), "fake starts unmuted");
        mic.set_muted(true).unwrap();
        assert!(mic.is_muted().unwrap());
        mic.set_muted(false).unwrap();
        assert!(!mic.is_muted().unwrap());
    }

    /// Ordering is guaranteed **structurally**, not by this test: every call
    /// blocks on its own reply before returning, and `set_muted` takes
    /// `&mut self`, so a second command cannot even be issued until the first
    /// has been serviced. There is no implementation of the current shape that
    /// could fail this assertion.
    ///
    /// It is here as a regression tripwire for the *shape*, and it has a known
    /// blind spot: a future fire-and-forget path (a `SetMuted` that does not
    /// wait for a reply) or a `Clone` impl on `MicHandle` letting two threads
    /// interleave sends would both keep this test green while the guarantee
    /// Task 6's push-to-talk depends on quietly evaporates. Either change needs
    /// a new test that actually races.
    #[test]
    fn commands_reach_the_backend_in_the_order_they_were_issued() {
        let (mut mic, view) = spawn_fake();
        for muted in [true, true, false, true, false] {
            mic.set_muted(muted).unwrap();
        }
        assert_eq!(
            view.lock().unwrap().mute_calls,
            vec![true, true, false, true, false],
            "push-to-talk correctness depends on this ordering"
        );
    }

    #[test]
    fn backend_errors_propagate_to_the_caller_instead_of_panicking() {
        let (mut mic, view) = spawn_fake();
        view.lock().unwrap().fail_next = true;

        let result = mic.set_muted(true);
        assert!(matches!(result, Err(AudioError::NoDevice)), "got {result:?}");

        // The worker survives a backend error and keeps serving.
        mic.set_muted(true).unwrap();
        assert!(mic.is_muted().unwrap());
    }

    /// Covers the **send** side of worker death, on all seven methods: the
    /// worker is already gone when the call starts, so `tx.send` fails and
    /// `call` returns before it ever reaches `reply_rx.recv()`.
    ///
    /// The receive side — worker dies *mid-command* — is a different branch and
    /// is covered by `a_worker_that_dies_mid_command_..` below. Do not assume
    /// this test exercises it; it cannot.
    #[test]
    fn a_dead_worker_degrades_to_an_error_on_every_path() {
        let (mut mic, _view) = spawn_fake();

        // Stop the worker out from under the handle and wait for it, so the
        // send failure is deterministic rather than racing the worker's exit.
        mic.tx.send(Command::Shutdown).expect("worker still alive");
        mic.join.take().expect("handle owns the thread").join().expect("worker panicked");

        fn expect_dead<T: std::fmt::Debug>(result: Result<T, AudioError>) {
            match result {
                Err(AudioError::Windows(msg)) => {
                    assert!(msg.contains("audio thread terminated"), "got {msg}");
                }
                other => panic!("expected a dead-worker error, got {other:?}"),
            }
        }

        expect_dead(mic.list_devices());
        expect_dead(mic.is_muted());
        expect_dead(mic.volume());
        expect_dead(mic.peak());
        expect_dead(mic.select(Some("anything")));
        expect_dead(mic.set_muted(true));
        expect_dead(mic.set_volume(0.5));
    }

    /// A backend that blows up inside a call, to kill the worker *mid-command*.
    ///
    /// This is the only way to reach `call`'s `reply_rx.recv()` failure branch:
    /// the send succeeds, the worker unwinds, the reply `Sender` drops, and the
    /// caller must get an `Err` rather than blocking on a reply that will never
    /// come. That branch is the difference between a crashed caller and a
    /// *hung* one, so it gets a test rather than an argument.
    struct PanickingFake;

    impl MicBackend for PanickingFake {
        fn list_devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            Ok(Vec::new())
        }
        fn select(&mut self, _id: Option<&str>) -> Result<(), AudioError> {
            Ok(())
        }
        fn is_muted(&self) -> Result<bool, AudioError> {
            Ok(false)
        }
        fn set_muted(&mut self, _muted: bool) -> Result<(), AudioError> {
            panic!("backend exploded")
        }
        fn volume(&self) -> Result<f32, AudioError> {
            Ok(0.0)
        }
        fn set_volume(&mut self, _level: f32) -> Result<(), AudioError> {
            Ok(())
        }
        fn peak(&self) -> Result<f32, AudioError> {
            Ok(0.0)
        }
    }

    /// The panic hook is process-global, so the two tests that deliberately
    /// panic a worker serialise on this and put the previous hook back.
    /// Suppressing it keeps a real backtrace out of otherwise-clean output.
    static PANIC_HOOK: Mutex<()> = Mutex::new(());

    fn with_quiet_panics<T>(f: impl FnOnce() -> T) -> T {
        let _guard = PANIC_HOOK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let out = f();
        std::panic::set_hook(previous);
        out
    }

    #[test]
    fn a_worker_that_dies_mid_command_errors_the_caller_instead_of_hanging_it() {
        let mut mic = MicHandle::spawn_with(|| Ok(PanickingFake)).expect("must spawn");

        // Prove the worker is alive first, so the send below certainly succeeds
        // and the failure we observe can only be on the reply side.
        assert!(!mic.is_muted().unwrap(), "worker must be serving before the kill");

        let result = with_quiet_panics(|| mic.set_muted(true));
        match result {
            Err(AudioError::Windows(msg)) => {
                assert!(msg.contains("audio thread terminated"), "got {msg}");
            }
            other => panic!("expected a dead-worker error, got {other:?}"),
        }

        // And the handle stays usable-as-broken rather than poisoned.
        assert!(mic.is_muted().is_err(), "later calls must keep erroring");
    }

    #[test]
    fn spawn_reports_a_worker_that_panicked_during_construction() {
        let result = with_quiet_panics(|| {
            MicHandle::spawn_with(|| -> Result<PanickingFake, AudioError> {
                panic!("factory exploded")
            })
        });
        match result {
            Err(AudioError::Windows(msg)) => {
                assert!(msg.contains("audio thread terminated"), "got {msg}");
            }
            Err(other) => panic!("expected a dead-worker error, got {other:?}"),
            Ok(_) => panic!("spawn must not hand back a handle to a dead worker"),
        }
    }

    #[test]
    fn dropping_a_handle_whose_worker_already_died_does_not_panic() {
        let (mic, _view) = spawn_fake();
        mic.tx.send(Command::Shutdown).expect("worker still alive");
        // `Drop` sends into a closed channel and joins a finished thread.
        drop(mic);
    }

    #[test]
    fn select_marshals_both_some_and_none_across_the_boundary() {
        let (mut mic, view) = spawn_fake();

        mic.select(Some("device-2")).unwrap();
        assert_eq!(view.lock().unwrap().selected, Some("device-2".to_string()));

        mic.select(None).unwrap();
        assert_eq!(view.lock().unwrap().selected, None);
    }

    #[test]
    fn list_devices_and_volume_round_trip_through_the_channel() {
        let (mut mic, _view) = spawn_fake();

        let devices = mic.list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "fake-1");

        mic.set_volume(0.25).unwrap();
        assert!((mic.volume().unwrap() - 0.25).abs() < 1e-6);
        assert_eq!(mic.peak().unwrap(), 0.0);
    }

    /// Startup failure must surface from `spawn`, not from the first call.
    #[test]
    fn spawn_reports_backend_construction_failure_synchronously() {
        let result = MicHandle::spawn_with(|| Err::<SharedFake, _>(AudioError::NoDevice));
        assert!(matches!(result, Err(AudioError::NoDevice)));
    }

    /// The same `MicControl`-generic shape Task 6 will use, resolved for the
    /// real proxy rather than the fake. `MicHandle` never names `MicControl` in
    /// an impl — it gets it from the blanket impl because it is a `Send`
    /// `MicBackend` — so this is the runtime proof that the collapse holds.
    #[test]
    fn a_mic_control_generic_resolves_for_the_handle() {
        fn toggle<M: crate::audio::MicControl>(mic: &mut M) -> Result<bool, AudioError> {
            let before = mic.is_muted()?;
            mic.set_muted(!before)?;
            mic.is_muted()
        }

        let (mut mic, view) = spawn_fake();
        assert!(toggle(&mut mic).unwrap());
        assert_eq!(view.lock().unwrap().mute_calls, vec![true]);
    }

    /// The proxy is only useful if it can actually be moved to another thread —
    /// which is what the whole confinement design buys.
    #[test]
    fn the_handle_can_be_moved_to_another_thread() {
        let (mut mic, view) = spawn_fake();
        std::thread::spawn(move || {
            mic.set_muted(true).unwrap();
        })
        .join()
        .expect("mover thread panicked");
        assert_eq!(view.lock().unwrap().mute_calls, vec![true]);
    }

    #[test]
    #[ignore = "requires real audio hardware; run with --ignored"]
    fn real_endpoint_round_trips_through_the_audio_thread() {
        let mut mic = MicHandle::spawn().expect("no capture endpoint");

        let devices = mic.list_devices().expect("list_devices through the channel");
        for d in &devices {
            println!("device: is_default={} name={:?} id={}", d.is_default, d.name, d.id);
        }
        assert!(!devices.is_empty(), "expected at least one capture device");

        // Read-only volume round trip: write back exactly what was there, so the
        // machine owner's level is unchanged.
        let level = mic.volume().expect("volume through the channel");
        mic.set_volume(level).expect("set_volume through the channel");
        let peak = mic.peak().expect("peak through the channel");
        println!("volume={level} peak={peak}");

        // Mute toggle. Nothing is asserted until the original state has been
        // put back, so a failing assertion cannot leave the microphone muted.
        let original = mic.is_muted().expect("is_muted through the channel");
        let toggled = mic.set_muted(!original).and_then(|()| mic.is_muted());
        let restore = mic.set_muted(original);
        let restored = mic.is_muted();
        println!("mute: original={original} toggled={toggled:?} restored={restored:?}");

        // `select` both ways, then confirm the endpoint still answers.
        let by_id = mic.select(Some(&devices[0].id));
        let by_default = mic.select(None);
        let after_select = mic.is_muted();

        restore.expect("must restore the original mute state");
        assert_eq!(restored.unwrap(), original, "microphone left in the wrong state");
        assert_eq!(toggled.unwrap(), !original, "set_muted must take effect");
        by_id.expect("select by id must resolve");
        by_default.expect("select(None) must follow the system default");
        after_select.expect("endpoint must still work after select");

        // Drop joins the worker, which drops the endpoint and leaves the MTA.
        drop(mic);
    }
}
