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
use windows::Win32::Media::Audio::{
    IAudioClient, IMMDevice, AUDCLNT_SHAREMODE_SHARED,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use super::endpoint::{win, Endpoint};
use super::{AudioError, DeviceInfo, MicBackend};

/// The one error every "the worker is gone" path produces. Callers include
/// Tauri command handlers and the keyboard-hook dispatch thread, so a dead
/// worker must degrade to this rather than panicking either of them.
///
/// A dedicated variant rather than a [`AudioError::Windows`] string, so those
/// callers can branch on "the audio thread is dead" without matching on an
/// error message — see [`AudioError::ThreadTerminated`].
fn worker_gone() -> AudioError {
    AudioError::ThreadTerminated
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
    /// Opens the worker's capture stream if one is not already open.
    /// Idempotent — see the handler in `run` and `CaptureStream`'s docs for
    /// why it exists and why its buffers are never read.
    StartMetering(Sender<Result<(), AudioError>>),
    /// Drops the worker's capture stream, if any. Idempotent.
    StopMetering(Sender<Result<(), AudioError>>),
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

/// A live WASAPI capture stream, held open for exactly one reason: as long as
/// it exists, the endpoint's `IAudioMeterInformation::GetPeakValue` keeps
/// reporting real levels instead of the flat zero it falls back to once
/// Windows decides nothing is listening to this endpoint (§4.7).
///
/// **We deliberately never call `GetBuffer`/`ReleaseBuffer` on it.** There is
/// no read loop anywhere in this file, and that is not a bug to "fix" — the
/// audio engine copies (and, once its ring buffer wraps, silently discards)
/// captured frames whether or not a client ever collects them. Reading the
/// buffers is exactly the work an `IAudioCaptureClient` read loop would need
/// for actual recording, which this app does not do; the stream exists only
/// to keep the meter honest.
///
/// Not `Send` (holds an `IAudioClient`, itself apartment-bound like every
/// other COM interface in this tree) — but it only ever lives inside the
/// worker's `run` loop below, on the one thread that is allowed to touch it,
/// so that is never a problem in practice.
pub(crate) struct CaptureStream {
    client: IAudioClient,
}

impl CaptureStream {
    /// One second of buffer, in 100-nanosecond units (WASAPI's native time
    /// unit). Generous on purpose: nothing ever drains this stream early, so
    /// the only cost of a longer buffer is a little memory, not a stall.
    const BUFFER_DURATION_100NS: i64 = 10_000_000;

    /// Activates a fresh `IAudioClient` on `device` and starts it in shared
    /// mode using the device's own mix format, so no format negotiation is
    /// needed. `device` is expected to be the same `IMMDevice` the caller's
    /// meter/volume interfaces were activated from, so this stream tracks
    /// the same endpoint currently selected — see [`Endpoint::device`].
    fn open(device: &IMMDevice) -> Result<Self, AudioError> {
        unsafe {
            let client: IAudioClient = win(device.Activate(CLSCTX_ALL, None))?;
            // `GetMixFormat` allocates with the COM task allocator; it is
            // ours to free once `Initialize` has read it.
            let format = win(client.GetMixFormat())?;
            let init = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,
                Self::BUFFER_DURATION_100NS,
                0,
                format,
                None,
            );
            CoTaskMemFree(Some(format as *const std::ffi::c_void));
            win(init)?;
            win(client.Start())?;
            Ok(Self { client })
        }
    }
}

impl Drop for CaptureStream {
    /// Best-effort stop so the mic-in-use indicator clears. If the client is
    /// already in a bad state there is nothing more useful to do than let it
    /// release on drop.
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Backends that can (optionally) open a capture stream to keep their meter
/// live. Only the real endpoint has anything real to open; every other
/// backend — `FakeMic` and the test doubles in this file's own test module —
/// gets the default, which has nothing to hold and always succeeds. That is
/// exactly the behaviour a backend with no real audio stream should have:
/// `StartMetering`/`StopMetering` become harmless no-ops rather than errors.
///
/// `Stream` is an associated type, not a hardcoded [`CaptureStream`], so that
/// a test double can hand back something it can *count* — see
/// `CountingCaptureFake` in this file's test module. `CaptureStream` itself
/// wraps a real `IAudioClient` and can only ever come from real hardware, so
/// pinning the trait to that concrete type would make it impossible for a
/// fake to prove "opened exactly once" rather than merely "did not error
/// twice", which is a materially weaker guarantee.
///
/// `pub(crate)` rather than private: `spawn_with` below is `pub(crate)` (so
/// `meter.rs`'s tests can use it), and a bound on a `pub(crate)` function
/// cannot be more private than the function itself.
pub(crate) trait MeterCapture: MicBackend {
    type Stream;

    fn open_capture_stream(&self) -> Result<Option<Self::Stream>, AudioError> {
        Ok(None)
    }
}

impl MeterCapture for MtaEndpoint {
    type Stream = CaptureStream;

    /// Opens a stream against the same device the endpoint's volume/meter
    /// interfaces already track (see [`Endpoint::device`]) — never a second,
    /// independently constructed endpoint.
    fn open_capture_stream(&self) -> Result<Option<CaptureStream>, AudioError> {
        CaptureStream::open(self.endpoint.device()).map(Some)
    }
}

/// `FakeMic` has no real audio to capture, so it gets the trivial default:
/// nothing to open, always succeeds. Implemented here (rather than in
/// `fake.rs`) because [`MeterCapture`] is private to this module — the
/// orphan rule only requires the trait or the type to be local, and both are,
/// so this is a perfectly ordinary same-crate impl.
#[cfg(test)]
impl MeterCapture for super::fake::FakeMic {
    type Stream = ();
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
    ///
    /// `pub(crate)` rather than private: `meter.rs`'s tests need a
    /// fake-backed worker too, and this is the same seam this module's own
    /// tests already use, not a new one grown just for them.
    pub(crate) fn spawn_with<B, F>(make: F) -> Result<Self, AudioError>
    where
        B: MicBackend + MeterCapture + 'static,
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
    ///
    /// Thin wrapper over the free [`send_command`] function, which is the
    /// actual funnel: [`MeterTap`] shares it too, so there is exactly one copy
    /// of this send/reply logic in the whole file, not one per handle type.
    fn call<T>(
        &self,
        build: impl FnOnce(Sender<Result<T, AudioError>>) -> Command,
    ) -> Result<T, AudioError> {
        send_command(&self.tx, build)
    }

    /// A cheap, cloneable, read-only view onto this worker's metering
    /// capability. Used by the level meter (`meter::MeterHandle`), which must
    /// be able to hand out its own clones without going anywhere near
    /// `MicHandle`'s single-ownership `Drop`. `MicHandle` itself has no
    /// metering methods of its own — see [`MeterTap`]'s docs for why the
    /// capture stream needs exactly one owner.
    pub fn tap(&self) -> MeterTap {
        MeterTap { tx: self.tx.clone() }
    }
}

/// The single channel funnel every command goes through, shared by
/// [`MicHandle::call`] and every [`MeterTap`] method. Both the send and the
/// receive degrade to [`worker_gone`] rather than ever unwrapping — this is
/// the one place that property has to hold for it to hold everywhere.
fn send_command<T>(
    tx: &Sender<Command>,
    build: impl FnOnce(Sender<Result<T, AudioError>>) -> Command,
) -> Result<T, AudioError> {
    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(build(reply_tx)).map_err(|_| worker_gone())?;
    reply_rx.recv().map_err(|_| worker_gone())?
}

/// The audio worker's *metering* capability: observe levels and control the
/// capture stream that keeps them live. Nothing more — it cannot mute,
/// unmute, set volume, or select a device, and there is no exception clause
/// to that: those four are the only things `MicBackend` exposes that are not
/// reachable through this type.
///
/// Holds only a command sender, so it is cheap to clone and `Send` falls out
/// of its field types — see the compile-time assertion below.
///
/// `MeterTap` is the **sole owner** of the capture stream's lifecycle
/// (`MicHandle` has no `start_metering`/`stop_metering` of its own). Two
/// independent handles able to open and close one non-refcounted resource
/// would let a `stop_metering()` call from one owner kill a running meter's
/// stream out from under the other with nothing left to reopen it — so
/// there is exactly one type in the tree that can touch it.
#[derive(Clone)]
pub struct MeterTap {
    tx: Sender<Command>,
}

const _: fn() = || {
    fn assert_send<T: Send>() {}
    fn assert_clone<T: Clone>() {}
    assert_send::<MeterTap>();
    assert_clone::<MeterTap>();
};

impl MeterTap {
    /// Same call/reply shape as every `MicHandle` method — funnelled through
    /// the same [`send_command`] helper, not a second copy of it.
    pub fn peak(&self) -> Result<f32, AudioError> {
        send_command(&self.tx, Command::Peak)
    }

    /// Opens the worker's capture stream if one is not already open, so the
    /// endpoint's meter stays live. See `Command::StartMetering` and
    /// [`CaptureStream`] for why. Idempotent, and a failure here is never
    /// fatal to the caller — it degrades to passive polling.
    pub fn start_metering(&self) -> Result<(), AudioError> {
        send_command(&self.tx, Command::StartMetering)
    }

    /// Drops the worker's capture stream, if any. Idempotent.
    pub fn stop_metering(&self) -> Result<(), AudioError> {
        send_command(&self.tx, Command::StopMetering)
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
    B: MicBackend + MeterCapture,
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

    // Declared after `backend` so it drops *before* `backend` at function
    // exit (locals drop in reverse declaration order) — the capture stream
    // stops before the endpoint (and, for the real backend, the COM
    // apartment) tears down, on every exit path: `Shutdown`, a disconnected
    // channel, or an explicit `StopMetering`.
    let mut capture: Option<B::Stream> = None;

    // `recv` returning `Err` means every sender is gone — the handle was
    // dropped without a clean shutdown. Same exit path either way.
    while let Ok(command) = rx.recv() {
        match command {
            Command::ListDevices(reply) => {
                let _ = reply.send(backend.list_devices());
            }
            Command::Select(id, reply) => {
                let result = backend.select(id.as_deref());
                // A running meter's capture stream is pinned to whichever
                // `IMMDevice` was selected when it was opened. If `select`
                // just moved the endpoint to a different device, that stream
                // is now stranded on the *old* one: the mic-in-use indicator
                // stays lit there indefinitely, and the meter goes dead
                // because nothing is capturing the newly-selected endpoint —
                // exactly the privacy-visible failure this design exists to
                // avoid. So: drop it and reopen against the new device.
                if result.is_ok() && capture.is_some() {
                    capture = None;
                    match backend.open_capture_stream() {
                        Ok(stream) => capture = stream,
                        // Same non-fatal policy as `StartMetering`: log and
                        // fall back to passive polling rather than taking
                        // the worker down over an optional stream.
                        Err(e) => {
                            eprintln!(
                                "mugon: failed to reopen capture stream after device change: {e}"
                            );
                        }
                    }
                }
                let _ = reply.send(result);
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
            Command::StartMetering(reply) => {
                let result = if capture.is_some() {
                    Ok(())
                } else {
                    match backend.open_capture_stream() {
                        Ok(stream) => {
                            capture = stream;
                            Ok(())
                        }
                        // Not fatal: log and let the meter fall back to
                        // passive polling, which reads zero unless something
                        // else happens to be capturing. See the module docs
                        // on `CaptureStream` for why this is acceptable.
                        Err(e) => {
                            eprintln!("mugon: failed to open capture stream for metering: {e}");
                            Err(e)
                        }
                    }
                };
                let _ = reply.send(result);
            }
            Command::StopMetering(reply) => {
                capture = None;
                let _ = reply.send(Ok(()));
            }
            Command::Shutdown => break,
        }
    }
    // `capture` drops here first (stopping any open stream), then `backend`,
    // releasing the COM interfaces and then the apartment.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::fake::FakeMic;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

    /// Nothing real to open; takes the trivial default (see [`MeterCapture`]).
    impl MeterCapture for SharedFake {
        type Stream = ();
    }

    /// Spawns a worker over a shared `FakeMic` and returns the handle plus a
    /// window onto the fake's recorded state.
    fn spawn_fake() -> (MicHandle, Arc<Mutex<FakeMic>>) {
        let (backend, view) = SharedFake::new();
        let handle = MicHandle::spawn_with(move || Ok(backend)).expect("fake backend must spawn");
        (handle, view)
    }

    /// A backend whose `open_capture_stream` actually returns `Some` (unlike
    /// every other test double in this file, which takes `MeterCapture`'s
    /// `Ok(None)` default) and counts every invocation.
    ///
    /// This exists because `capture.is_some()` — the guard in `run` that
    /// stops `StartMetering` from opening a second stream — can never go
    /// `true` against a backend that always returns `None`. Without a fake
    /// that can return `Some`, an implementation that opened a fresh stream
    /// on *every* `StartMetering` (leaking the first, doubling the mic hold)
    /// would pass every idempotency test in this file. The `opens` counter is
    /// what actually distinguishes "opened once, then no-opped" from "opened
    /// twice, but neither call errored".
    struct CountingCaptureFake {
        backend: SharedFake,
        opens: Arc<AtomicUsize>,
        fail_open: Arc<AtomicBool>,
    }

    impl CountingCaptureFake {
        fn new() -> (Self, Arc<Mutex<FakeMic>>, Arc<AtomicUsize>, Arc<AtomicBool>) {
            let (backend, view) = SharedFake::new();
            let opens = Arc::new(AtomicUsize::new(0));
            let fail_open = Arc::new(AtomicBool::new(false));
            (
                Self { backend, opens: Arc::clone(&opens), fail_open: Arc::clone(&fail_open) },
                view,
                opens,
                fail_open,
            )
        }
    }

    impl MicBackend for CountingCaptureFake {
        fn list_devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            self.backend.list_devices()
        }
        fn select(&mut self, id: Option<&str>) -> Result<(), AudioError> {
            self.backend.select(id)
        }
        fn is_muted(&self) -> Result<bool, AudioError> {
            self.backend.is_muted()
        }
        fn set_muted(&mut self, muted: bool) -> Result<(), AudioError> {
            self.backend.set_muted(muted)
        }
        fn volume(&self) -> Result<f32, AudioError> {
            self.backend.volume()
        }
        fn set_volume(&mut self, level: f32) -> Result<(), AudioError> {
            self.backend.set_volume(level)
        }
        fn peak(&self) -> Result<f32, AudioError> {
            self.backend.peak()
        }
    }

    impl MeterCapture for CountingCaptureFake {
        // No real resource to hold; `()` is enough for a countable "opened".
        type Stream = ();

        fn open_capture_stream(&self) -> Result<Option<()>, AudioError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            if self.fail_open.load(Ordering::SeqCst) {
                return Err(AudioError::Windows("simulated capture-stream failure".into()));
            }
            Ok(Some(()))
        }
    }

    /// Spawns a worker over a [`CountingCaptureFake`] and returns the handle
    /// plus handles onto its recorded state (mute view, open count, and a
    /// switch to make the *next* open fail).
    fn spawn_counting_fake(
    ) -> (MicHandle, Arc<Mutex<FakeMic>>, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let (backend, view, opens, fail_open) = CountingCaptureFake::new();
        let handle = MicHandle::spawn_with(move || Ok(backend)).expect("fake backend must spawn");
        (handle, view, opens, fail_open)
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
                Err(AudioError::ThreadTerminated) => {}
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

    /// Nothing real to open; takes the trivial default (see [`MeterCapture`]).
    impl MeterCapture for PanickingFake {
        type Stream = ();
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
        assert!(
            matches!(result, Err(AudioError::ThreadTerminated)),
            "expected a dead-worker error, got {result:?}"
        );

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
            Err(AudioError::ThreadTerminated) => {}
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

    #[test]
    fn meter_tap_peak_returns_the_backends_value_through_the_channel() {
        let (mic, view) = spawn_fake();
        let tap = mic.tap();

        view.lock().unwrap().peak = 0.42;
        assert!((tap.peak().unwrap() - 0.42).abs() < 1e-6);

        view.lock().unwrap().peak = 0.0;
        assert_eq!(tap.peak().unwrap(), 0.0);
    }

    /// A tap must be usable from another thread and cloneable without
    /// touching the worker — both fall out of holding nothing but a
    /// `Sender<Command>`, but that is exactly the property a future field
    /// addition could quietly break, so it is asserted at compile time (see
    /// the `const _` block by `MeterTap`'s definition) and exercised here at
    /// runtime.
    #[test]
    fn meter_tap_is_cloneable_and_usable_from_another_thread() {
        let (mic, view) = spawn_fake();
        view.lock().unwrap().peak = 0.75;
        let tap = mic.tap();
        let tap2 = tap.clone();

        let handle = std::thread::spawn(move || tap2.peak());
        assert!((tap.peak().unwrap() - 0.75).abs() < 1e-6);
        assert!((handle.join().unwrap().unwrap() - 0.75).abs() < 1e-6);
    }

    #[test]
    fn a_tap_on_a_dead_worker_errors_instead_of_hanging_or_panicking() {
        let (mic, _view) = spawn_fake();
        let tap = mic.tap();

        // Kill the worker deterministically, same idiom as
        // `a_dead_worker_degrades_to_an_error_on_every_path` above.
        drop(mic);

        match tap.peak() {
            Err(AudioError::ThreadTerminated) => {}
            other => panic!("expected a dead-worker error, got {other:?}"),
        }
    }

    /// The real idempotency property: a second `StartMetering` while a stream
    /// is already open must not open a *second* one. `SharedFake`-backed
    /// tests can't tell the difference (its `open_capture_stream` always
    /// returns `None`, so `capture.is_some()` never gates anything) — hence
    /// [`CountingCaptureFake`], which returns a real `Some` and counts calls.
    #[test]
    fn start_metering_twice_opens_the_capture_stream_exactly_once() {
        let (_mic, _view, opens, _fail_open) = spawn_counting_fake();
        let tap = _mic.tap();

        tap.start_metering().unwrap();
        tap.start_metering().unwrap();
        assert_eq!(opens.load(Ordering::SeqCst), 1, "second start must not reopen");
    }

    #[test]
    fn stop_metering_twice_is_a_no_op_not_an_error() {
        let (_mic, _view, _opens, _fail_open) = spawn_counting_fake();
        let tap = _mic.tap();

        tap.start_metering().unwrap();
        tap.stop_metering().unwrap();
        tap.stop_metering().unwrap(); // must not error the second time
    }

    /// Fix 2: a device change while metering is active must not strand the
    /// capture stream on the old device. If it did, the mic-in-use indicator
    /// would stay lit on a device that is no longer selected, and the meter
    /// would go dead against the one that is — the exact privacy-visible
    /// failure this design exists to avoid.
    #[test]
    fn selecting_a_new_device_while_metering_reopens_the_capture_stream() {
        let (mut mic, _view, opens, _fail_open) = spawn_counting_fake();
        let tap = mic.tap();

        tap.start_metering().unwrap();
        assert_eq!(opens.load(Ordering::SeqCst), 1, "initial open");

        mic.select(Some("device-2")).unwrap();
        assert_eq!(opens.load(Ordering::SeqCst), 2, "select must reopen against the new device");

        // And the reopened stream is tracked correctly: a further
        // `start_metering` must not open a *third* time.
        tap.start_metering().unwrap();
        assert_eq!(opens.load(Ordering::SeqCst), 2, "already open after the reopen");
    }

    /// The non-fatal policy applies to the reopen path too: if the new
    /// device's stream fails to open, `select` itself must still succeed
    /// (the device *was* selected), and the worker must keep serving with
    /// the meter simply falling back to passive polling.
    #[test]
    fn a_failed_reopen_on_device_change_does_not_fail_select_or_kill_the_worker() {
        let (mut mic, _view, opens, fail_open) = spawn_counting_fake();
        let tap = mic.tap();

        tap.start_metering().unwrap();
        fail_open.store(true, Ordering::SeqCst);

        mic.select(Some("device-2")).expect("select must succeed even if the reopen fails");
        assert_eq!(opens.load(Ordering::SeqCst), 2, "a reopen must still be attempted");

        // Worker is still alive and serving.
        assert!(!mic.is_muted().unwrap());
    }

    /// Selecting a device while nothing is metering must not touch the
    /// capture stream at all — there is nothing to reopen.
    #[test]
    fn selecting_a_device_without_an_active_meter_does_not_open_a_stream() {
        let (mut mic, _view, opens, _fail_open) = spawn_counting_fake();
        mic.select(Some("device-2")).unwrap();
        assert_eq!(opens.load(Ordering::SeqCst), 0);
    }

    /// The ambiguity this project's Task 8 brief flagged directly: a live
    /// `MeterTap` keeps the command channel from *disconnecting*, so the
    /// worker cannot rely on "every sender dropped" to know when to exit
    /// while a tap is still alive. `MicHandle::drop` sidesteps that by
    /// sending an explicit `Shutdown` first — this test proves that holds
    /// even when a tap is mid-`peak()` call at the exact moment the handle
    /// drops, by racing the two on separate threads.
    ///
    /// If a regression ever reintroduces a hang here, this test must not
    /// hang the whole suite with it — the background thread reports back
    /// over a channel with a bounded `recv_timeout` instead of being joined
    /// directly, so a real deadlock fails loudly instead of freezing CI.
    #[test]
    fn dropping_the_handle_while_a_tap_is_mid_poll_does_not_hang() {
        let (mic, _view) = spawn_fake();
        let tap = mic.tap();

        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Hammer `peak()` so at least one call is in flight (send issued,
            // reply pending) at the moment `mic` drops below. Every outcome
            // — `Ok` before shutdown, `Err` once the worker is gone — is
            // acceptable; the only failure mode this guards against is the
            // call never returning at all.
            for _ in 0..20_000 {
                let _ = tap.peak();
            }
            let _ = done_tx.send(());
        });

        drop(mic);

        done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("a MeterTap::peak() call hung across worker shutdown — this is a real deadlock");
    }

    #[test]
    #[ignore = "requires real audio hardware; run with --ignored"]
    fn real_capture_stream_keeps_the_meter_live_and_releases_cleanly() {
        let mic = MicHandle::spawn().expect("no capture endpoint");
        let tap = mic.tap();

        tap.start_metering().expect("capture stream must open on real hardware");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let peak = tap.peak().expect("peak through the tap");
        println!("peak with capture stream open: {peak}");
        assert!((0.0..=1.0).contains(&peak), "peak out of range: {peak}");

        tap.stop_metering().expect("capture stream must close");

        // Re-opening cleanly after a stop is the evidence the previous
        // stream was actually released rather than left half torn-down.
        tap.start_metering().expect("must be able to reopen after stop");
        tap.stop_metering().expect("second stop must also succeed");

        // Metering never touches mute state; nothing to restore here.
        drop(mic);
    }
}
