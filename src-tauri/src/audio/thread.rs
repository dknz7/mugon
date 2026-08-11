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
//!
//! # Drop order is load-bearing on *ordinary* exits only
//!
//! Several comments below turn on Rust's drop order — fields in declaration
//! order, locals in reverse, parameters last — and each of them is genuinely
//! load-bearing where it stands. But the release profile sets
//! `panic = "abort"` (DESIGN.md §10), and **an aborting process does not
//! unwind, so none of these `Drop` impls run on a panic**: not
//! [`MicHandle::drop`]'s worker join, not [`CaptureStream`]'s stream release,
//! not [`hotplug::Registration`]'s `UnregisterEndpointNotificationCallback`.
//!
//! That is accepted rather than overlooked. Every one of those is a
//! process-lifetime resource that Windows reclaims when the process dies: COM
//! registrations go with the apartment, the WASAPI stream goes with the
//! client, and the worker thread goes with the process. The ordering matters
//! for tray Quit, `WM_ENDSESSION`, and window close — the paths a user
//! actually takes — and it holds on all of them.
//!
//! The one piece of cleanup that would *not* be reclaimed is a microphone left
//! muted, and that deliberately does not use `Drop` at all: `main.rs` installs
//! a panic hook and `crate::emergency_unmute` restores the mic from inside it,
//! which still runs because hooks execute before the abort. Verified under
//! this profile by `examples/panic_hook_abort.rs`.

use std::marker::PhantomData;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::Media::Audio::{
    IAudioClient, IMMDevice, AUDCLNT_SHAREMODE_SHARED,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use super::endpoint::{win, Endpoint};
use super::hotplug;
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
    /// Registers the [`hotplug`] notification client on the worker thread, so
    /// device arrivals and departures start producing [`Command::DeviceChanged`].
    ///
    /// Carries the callback rather than a Tauri `AppHandle` because
    /// [`MicHandle::enable_hotplug`] builds the closure on the caller's side —
    /// which is what keeps this enum private to this module and keeps
    /// `hotplug.rs` free of any dependency on it.
    EnableHotplug(hotplug::OnChange, Sender<Result<(), AudioError>>),
    /// The set of devices, or the default device, changed. **No reply sender,
    /// deliberately.**
    ///
    /// This is sent from a COM notification callback on a thread belonging to
    /// the Windows audio service. Waiting for a reply there would park that
    /// thread — and if this worker happened to be inside a COM call at the
    /// time, it would be one COM thread blocked on another. So it is
    /// fire-and-forget: the sender returns immediately and never learns the
    /// outcome. The handler in [`run`] logs its own failures for that reason.
    DeviceChanged,
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

/// Backends that can re-resolve their device after a hotplug event.
///
/// Deliberately **not** a method on [`MicBackend`]. Only the real endpoint has
/// a device to re-resolve; putting a defaulted `refresh` on `MicBackend` would
/// silently hand [`MicHandle`] and `state::Mic` a `refresh()` that compiles,
/// looks like it works and does nothing — a trap for whoever calls it next.
/// Confining it to a worker-private trait means the only type that can be
/// asked to refresh is one the worker actually owns.
///
/// The default is a no-op for the same reason [`MeterCapture`]'s is: every
/// backend that is not a real endpoint has nothing to re-resolve, so a device
/// change is correctly a no-op rather than an error.
pub(crate) trait DeviceRefresh {
    fn refresh(&mut self) -> Result<(), AudioError> {
        Ok(())
    }
}

impl DeviceRefresh for MtaEndpoint {
    /// The hotplug path's route into [`Endpoint::refresh`], reached from
    /// `Command::DeviceChanged`. `Endpoint::select` calls `refresh` too, as
    /// part of re-pointing at a new device.
    fn refresh(&mut self) -> Result<(), AudioError> {
        self.endpoint.refresh()
    }
}

#[cfg(test)]
impl DeviceRefresh for super::fake::FakeMic {}

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
    /// Nothing is ever sent on this. Its *disconnection* is the signal: the
    /// matching `Sender` is a parameter of [`run`], so it drops after every
    /// local in that function — after the capture stream and after the backend
    /// — meaning a `Disconnected` here proves the worker has finished tearing
    /// down. [`MicHandle::drop`] waits on it with a bound, which a bare
    /// `JoinHandle::join` cannot offer.
    done: Receiver<()>,
    /// Always [`COMMAND_TIMEOUT`] in production. A field rather than a direct
    /// use of the constant so the two tests that must actually *wait out* the
    /// bound can set it to milliseconds: asserting the timeout fires is worth
    /// a test, but spending five real seconds per assertion to do it made the
    /// entire suite five seconds long. `command_timeout_is_five_seconds` pins
    /// the production value separately, so shrinking it here cannot hide a
    /// change to the real bound.
    timeout: Duration,
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
        B: MicBackend + MeterCapture + DeviceRefresh + 'static,
        F: FnOnce() -> Result<B, AudioError> + Send + 'static,
    {
        Self::spawn_bounded(COMMAND_TIMEOUT, make)
    }

    /// [`Self::spawn_with`] with the command bound chosen by the caller.
    ///
    /// Exists for the two tests that assert the timeout actually fires, which
    /// have to *wait it out* to do so. At the production five seconds that was
    /// the entire runtime of the test suite; at milliseconds the same
    /// assertions cost nothing and `command_timeout_is_five_seconds` pins the
    /// real value so the shortcut cannot mask a change to it.
    pub(crate) fn spawn_bounded<B, F>(timeout: Duration, make: F) -> Result<Self, AudioError>
    where
        B: MicBackend + MeterCapture + DeviceRefresh + 'static,
        F: FnOnce() -> Result<B, AudioError> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<Command>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), AudioError>>();
        let (done_tx, done_rx) = mpsc::channel::<()>();

        let join = std::thread::Builder::new()
            .name("mugon-audio".into())
            .spawn(move || run(make, ready_tx, rx, done_tx))
            .map_err(|e| AudioError::Windows(format!("could not spawn audio thread: {e}")))?;

        // A `RecvError` here means the worker died without reporting, which is
        // still a startup failure — never an unwrap.
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { tx, join: Some(join), done: done_rx, timeout }),
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
        send_command(&self.tx, self.timeout, build)
    }

    /// A cheap, cloneable, read-only view onto this worker's metering
    /// capability. Used by the level meter (`meter::MeterHandle`), which must
    /// be able to hand out its own clones without going anywhere near
    /// `MicHandle`'s single-ownership `Drop`. See [`MeterTap`]'s docs for why
    /// the capture stream needs exactly one owner.
    pub fn tap(&self) -> MeterTap {
        MeterTap { tx: self.tx.clone(), timeout: self.timeout }
    }

    /// Starts listening for capture-device arrivals, departures and
    /// default-device changes (§4.5).
    ///
    /// Separate from [`Self::spawn`] because registering the notification
    /// client needs an `AppHandle` to emit through, and no `AppHandle` exists
    /// until Tauri's `setup` runs — by which point the worker has been serving
    /// for a while. Call it once, from `setup`.
    ///
    /// The closure built here is everything a COM notification thread will
    /// ever do, and both halves of it are non-blocking by construction:
    ///
    /// - the [`Command::DeviceChanged`] send has no reply channel, so it
    ///   cannot wait on the worker;
    /// - `AppHandle::emit` is `Send + Sync` and queues to the webview.
    ///
    /// Neither touches an `Endpoint`, and neither can, because a closure that
    /// is `Send + Sync` cannot capture one.
    ///
    /// Both failures are swallowed inside the closure on purpose: by the time
    /// a notification arrives, a dead worker or a torn-down webview is not
    /// something a callback can act on, and returning an error from an
    /// `IMMNotificationClient` method does nothing useful either.
    ///
    /// Blocks on a reply, unlike the callback path — this runs on the main
    /// thread at startup against an idle worker, and a registration that
    /// silently failed would leave hotplug quietly dead with no diagnostic.
    pub fn enable_hotplug(&self, app: AppHandle) -> Result<(), AudioError> {
        let tx = self.tx.clone();
        let on_change: hotplug::OnChange = Box::new(move || {
            let _ = tx.send(Command::DeviceChanged);
            let _ = app.emit(hotplug::DEVICES_CHANGED, ());
        });
        self.call(|reply| Command::EnableHotplug(on_change, reply))
    }
}

/// How long a caller waits for the worker to answer one command.
///
/// Core Audio calls are milliseconds; five seconds means something is
/// genuinely broken, not merely slow. The bound exists because a *hung* worker
/// is a different failure from a *dead* one: death disconnects the reply
/// channel and unblocks the caller for free, while a hang inside a COM call
/// blocks forever. Tauri runs sync commands on the main thread and every
/// command holds the `Core` lock across this call, so an unbounded wait would
/// freeze the UI, stall the hook-dispatch thread behind the same lock, and
/// leave no path to the tray's Quit item.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// The single channel funnel every command goes through, shared by
/// [`MicHandle::call`] and every [`MeterTap`] method. The send, the receive
/// and the timeout all degrade to an error rather than ever unwrapping or
/// blocking indefinitely — this is the one place those properties have to hold
/// for them to hold everywhere.
fn send_command<T>(
    tx: &Sender<Command>,
    timeout: Duration,
    build: impl FnOnce(Sender<Result<T, AudioError>>) -> Command,
) -> Result<T, AudioError> {
    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(build(reply_tx)).map_err(|_| worker_gone())?;
    match reply_rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(AudioError::Timeout(timeout)),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(worker_gone()),
    }
}

/// The audio worker's *metering* capability: observe levels and control the
/// capture stream that keeps them live. Nothing more — it cannot mute, unmute,
/// read mute, set or read volume, or select a device, and there is no
/// exception clause to that.
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
    /// Inherited from the [`MicHandle`] that produced it — see its `timeout`.
    timeout: Duration,
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
        send_command(&self.tx, self.timeout, Command::Peak)
    }

    /// Opens the worker's capture stream if one is not already open, so the
    /// endpoint's meter stays live. See `Command::StartMetering` and
    /// [`CaptureStream`] for why. Idempotent, and a failure here is never
    /// fatal to the caller — it degrades to passive polling.
    pub fn start_metering(&self) -> Result<(), AudioError> {
        send_command(&self.tx, self.timeout, Command::StartMetering)
    }

    /// Drops the worker's capture stream, if any. Idempotent.
    pub fn stop_metering(&self) -> Result<(), AudioError> {
        send_command(&self.tx, self.timeout, Command::StopMetering)
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
    /// Every step ignores failure: the worker may already be gone.
    ///
    /// **The wait is bounded**, which is the whole reason [`Self::done`]
    /// exists. `COMMAND_TIMEOUT` bounds how long a *command* may take, not how
    /// long exit may: a worker wedged inside a COM call never reaches the
    /// `Shutdown` sitting in its queue, and a plain `join()` would then block
    /// forever. This drop runs as the process exits, by which point there is no
    /// window and no tray — so an unbounded join means an invisible process
    /// that only Task Manager can end.
    ///
    /// On timeout the `JoinHandle` is simply dropped, which detaches the
    /// thread. It keeps its endpoint and its apartment until the process goes,
    /// which is the correct trade: a leaked thread in a dying process beats a
    /// process that will not die.
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);

        // `Ok`/`Disconnected` both mean "finished"; only `Timeout` is failure.
        let wedged = matches!(
            self.done.recv_timeout(self.timeout),
            Err(mpsc::RecvTimeoutError::Timeout)
        );

        match self.join.take() {
            // The worker is already out of `run`, so this join returns at once.
            Some(join) if !wedged => {
                let _ = join.join();
            }
            Some(_detached) => eprintln!(
                "mugon: the audio worker did not shut down within {:?}; \
                 detaching it so the process can exit",
                self.timeout
            ),
            None => {}
        }
    }
}

/// Re-points a *running* capture stream at whatever device the backend now
/// tracks. A no-op when nothing is metering — there is no stream to move.
///
/// Shared by the two arms that can change which device the endpoint points at,
/// [`Command::Select`] and [`Command::DeviceChanged`], because the hazard is
/// identical in both and so is the fix. A stream stays pinned to the
/// `IMMDevice` it was opened against, so leaving it alone across a device
/// change strands Windows' microphone-in-use indicator lit on the old device —
/// possibly one the user has just unplugged — while the meter reads dead
/// silence on the new one.
///
/// Failure is non-fatal, matching [`Command::StartMetering`]'s policy: log it
/// and let the meter fall back to passive polling rather than taking the
/// worker down over an optional stream.
fn reopen_capture<B: MeterCapture>(backend: &B, capture: &mut Option<B::Stream>) {
    if capture.is_none() {
        return;
    }
    // Dropped *before* the reopen, not swapped after it: the old device must
    // be released before a second stream is opened, or both are held at once.
    *capture = None;
    match backend.open_capture_stream() {
        Ok(stream) => *capture = stream,
        Err(e) => eprintln!("mugon: failed to reopen capture stream after device change: {e}"),
    }
}

/// The worker thread body: construct, report, serve, tear down.
///
/// `_done` is never sent on and is never named again. It is a parameter rather
/// than a local so that it drops *after* every local below — parameters are
/// dropped last — which is what makes its disconnection mean "the capture
/// stream and the backend are already gone" rather than merely "the loop
/// ended". [`MicHandle::drop`] relies on exactly that.
fn run<B, F>(
    make: F,
    ready: Sender<Result<(), AudioError>>,
    rx: Receiver<Command>,
    _done: Sender<()>,
) where
    B: MicBackend + MeterCapture + DeviceRefresh,
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

    // Declared last so it drops *first*: the notification client must be
    // unregistered while this thread is still in the MTA and the enumerator it
    // registered on is still alive — i.e. before `capture` and before
    // `backend` (and, for the real backend, before `CoUninitialize`).
    let mut hotplug: Option<hotplug::Registration> = None;

    // `recv` returning `Err` means every sender is gone — the handle was
    // dropped without a clean shutdown. Same exit path either way.
    //
    // Note that once hotplug is registered this arm becomes unreachable: the
    // notification client holds a `Sender<Command>`, so the channel can no
    // longer disconnect while the worker is running. That is safe because
    // `MicHandle` is the sole owner of this worker and its `Drop` always sends
    // `Shutdown` first — the disconnect arm is a belt-and-braces fallback, not
    // the primary exit.
    while let Ok(command) = rx.recv() {
        match command {
            Command::ListDevices(reply) => {
                let _ = reply.send(backend.list_devices());
            }
            Command::Select(id, reply) => {
                let result = backend.select(id.as_deref());
                // A successful `select` may have moved the endpoint to a
                // different device; a running capture stream has to follow it.
                // See [`reopen_capture`].
                if result.is_ok() {
                    reopen_capture(&backend, &mut capture);
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
            Command::EnableHotplug(on_change, reply) => {
                // Registration happens here, on the worker thread, because
                // this is the thread that is in the MTA — and because it is
                // the thread whose exit must undo it.
                let result = hotplug::Registration::new(on_change).map(|registration| {
                    // Assigning over an existing registration unregisters it
                    // on drop, so a second call replaces rather than stacks.
                    hotplug = Some(registration);
                });
                let _ = reply.send(result);
            }
            Command::DeviceChanged => match backend.refresh() {
                // The endpoint now points at whatever `selected_id` resolves
                // to today; a running capture stream has to follow it, exactly
                // as it does after a `Select`.
                Ok(()) => reopen_capture(&backend, &mut capture),
                // **`selected_id` is deliberately left alone.** The device the
                // user picked may be merely absent — disabled, or unplugged —
                // and keeping the choice means the next notification, when
                // they plug it back in, restores it with no further action.
                // Clearing it here would silently and permanently demote them
                // to the system default. Same policy as a failed `select`.
                //
                // Nothing to reply to (see `Command::DeviceChanged`), so the
                // log is the only channel this failure has.
                Err(e) => eprintln!("mugon: could not re-resolve the device after a change: {e}"),
            },
            Command::Shutdown => break,
        }
    }
    // Locals drop in reverse declaration order, so: `hotplug` unregisters the
    // notification client (releasing the `Sender<Command>` it held), then
    // `capture` stops any open stream, then `backend` releases the COM
    // interfaces and finally the apartment.
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

    /// No device to re-resolve; takes the trivial default (see
    /// [`DeviceRefresh`]).
    impl DeviceRefresh for SharedFake {}

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
        /// Counts [`DeviceRefresh::refresh`] calls, and — when `fail_refresh`
        /// is set — makes them fail. Task 9b's hotplug handler is the only
        /// caller of `refresh`, so without these there is no way to tell a
        /// `DeviceChanged` that refreshed from one that quietly did nothing.
        refreshes: Arc<AtomicUsize>,
        fail_refresh: Arc<AtomicBool>,
    }

    /// The knobs and counters [`CountingCaptureFake`] exposes to a test, bundled
    /// so `spawn_counting_fake` returns two values instead of six.
    struct CaptureProbe {
        view: Arc<Mutex<FakeMic>>,
        opens: Arc<AtomicUsize>,
        fail_open: Arc<AtomicBool>,
        refreshes: Arc<AtomicUsize>,
        fail_refresh: Arc<AtomicBool>,
    }

    impl CaptureProbe {
        fn opens(&self) -> usize {
            self.opens.load(Ordering::SeqCst)
        }
        fn refreshes(&self) -> usize {
            self.refreshes.load(Ordering::SeqCst)
        }
        fn selected(&self) -> Option<String> {
            match self.view.lock() {
                Ok(guard) => guard.selected.clone(),
                Err(poisoned) => poisoned.into_inner().selected.clone(),
            }
        }
    }

    impl CountingCaptureFake {
        fn new() -> (Self, CaptureProbe) {
            let (backend, view) = SharedFake::new();
            let opens = Arc::new(AtomicUsize::new(0));
            let fail_open = Arc::new(AtomicBool::new(false));
            let refreshes = Arc::new(AtomicUsize::new(0));
            let fail_refresh = Arc::new(AtomicBool::new(false));
            (
                Self {
                    backend,
                    opens: Arc::clone(&opens),
                    fail_open: Arc::clone(&fail_open),
                    refreshes: Arc::clone(&refreshes),
                    fail_refresh: Arc::clone(&fail_refresh),
                },
                CaptureProbe { view, opens, fail_open, refreshes, fail_refresh },
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

    /// Counts and optionally fails, but — like `Endpoint::refresh` — never
    /// touches the selection. The tests below rely on that: if a
    /// `DeviceChanged` ever clears `selected`, it can only have been the
    /// worker's handler that did it.
    impl DeviceRefresh for CountingCaptureFake {
        fn refresh(&mut self) -> Result<(), AudioError> {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            if self.fail_refresh.load(Ordering::SeqCst) {
                return Err(AudioError::DeviceNotFound("simulated missing device".into()));
            }
            Ok(())
        }
    }

    /// Spawns a worker over a [`CountingCaptureFake`] and returns the handle
    /// plus the [`CaptureProbe`] onto its recorded state.
    fn spawn_counting_fake() -> (MicHandle, CaptureProbe) {
        let (backend, probe) = CountingCaptureFake::new();
        let handle = MicHandle::spawn_with(move || Ok(backend)).expect("fake backend must spawn");
        (handle, probe)
    }

    /// `DeviceChanged` carries no reply channel, so a test cannot wait on it
    /// the way it waits on every other command. This rides a *replying*
    /// command in behind it: the worker services its queue strictly in order,
    /// so once `is_muted` has answered, the `DeviceChanged` ahead of it has
    /// certainly been handled.
    ///
    /// A sleep would work too and would be flaky; this is exact.
    fn send_device_changed(mic: &MicHandle) {
        mic.tx.send(Command::DeviceChanged).expect("worker still alive");
        mic.is_muted().expect("the worker must still be serving after a device change");
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

    impl DeviceRefresh for PanickingFake {}

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

    /// A backend that hangs *inside* a call until the test lets it go.
    ///
    /// This models the failure [`COMMAND_TIMEOUT`] exists for and the one a
    /// disconnected channel cannot detect: the worker is alive, its channels
    /// are connected, and the reply is simply never coming. Every other fake
    /// in this file either answers or dies.
    ///
    /// Both `is_muted` and `peak` block, on the same receiver — one release per
    /// wedged call. Two entry points because the two tests below need to wedge
    /// the worker from different sides: one from the owning `MicHandle`, one
    /// from a `MeterTap` (which has no `is_muted`) so the handle stays free to
    /// be dropped while the worker is stuck.
    struct WedgedFake {
        release: Receiver<()>,
    }

    impl MicBackend for WedgedFake {
        fn list_devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            Ok(Vec::new())
        }
        fn select(&mut self, _id: Option<&str>) -> Result<(), AudioError> {
            Ok(())
        }
        fn is_muted(&self) -> Result<bool, AudioError> {
            // Blocks until the test releases it, or forever if it does not.
            let _ = self.release.recv();
            Ok(false)
        }
        fn set_muted(&mut self, _muted: bool) -> Result<(), AudioError> {
            Ok(())
        }
        fn volume(&self) -> Result<f32, AudioError> {
            Ok(0.0)
        }
        fn set_volume(&mut self, _level: f32) -> Result<(), AudioError> {
            Ok(())
        }
        fn peak(&self) -> Result<f32, AudioError> {
            // Blocks until the test releases it, or forever if it does not.
            let _ = self.release.recv();
            Ok(0.0)
        }
    }

    impl MeterCapture for WedgedFake {
        type Stream = ();
    }

    impl DeviceRefresh for WedgedFake {}

    /// The bound these two tests wait out. Production is
    /// [`COMMAND_TIMEOUT`] — five seconds — and waiting that out twice *was*
    /// the runtime of the whole suite. What is under test is that the bound is
    /// enforced at all, which does not depend on its length;
    /// `command_timeout_is_five_seconds` pins the real value.
    const TEST_TIMEOUT: Duration = Duration::from_millis(100);

    /// The production bound, asserted on its own so the millisecond timeout
    /// the two tests below use cannot quietly become the shipped one.
    #[test]
    fn command_timeout_is_five_seconds() {
        assert_eq!(COMMAND_TIMEOUT, Duration::from_secs(5));
    }

    /// The property: a caller waiting on a worker that never replies gets an
    /// error, not a permanent block. Without the bound this test would hang
    /// the suite — which is exactly what it would do to the UI thread, while
    /// holding the `Core` lock, in production.
    #[test]
    fn a_wedged_worker_times_out_instead_of_blocking_the_caller_forever() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let mic = MicHandle::spawn_bounded(TEST_TIMEOUT, move || {
            Ok(WedgedFake { release: release_rx })
        })
        .expect("must spawn");

        let started = std::time::Instant::now();
        let result = mic.is_muted();
        let elapsed = started.elapsed();

        match result {
            Err(AudioError::Timeout(bound)) => assert_eq!(bound, TEST_TIMEOUT),
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert!(elapsed >= TEST_TIMEOUT, "returned before the bound elapsed: {elapsed:?}");

        // Let the worker out before dropping the handle: `MicHandle::drop`
        // joins the thread, so a still-wedged worker would hang this test
        // rather than fail it.
        let _ = release_tx.send(());
        drop(mic);
    }

    /// The exit-side counterpart to the timeout test above, and the property
    /// the app's quit path depends on: a worker wedged inside a COM call never
    /// reaches the `Shutdown` in its queue, so `Drop` must give up on it rather
    /// than join forever. Unbounded, this is a process with no window and no
    /// tray that only Task Manager can end.
    ///
    /// Run on a background thread with a bounded wait for the same reason
    /// `dropping_the_handle_while_a_tap_is_mid_poll_does_not_hang` is: a
    /// regression here must fail the suite, not freeze it.
    #[test]
    fn dropping_a_handle_whose_worker_is_wedged_detaches_instead_of_hanging() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let mic = MicHandle::spawn_bounded(TEST_TIMEOUT, move || {
            Ok(WedgedFake { release: release_rx })
        })
        .expect("must spawn");

        // Wedge the worker through a tap, so the handle itself stays droppable.
        let tap = mic.tap();
        std::thread::spawn(move || {
            let _ = tap.peak();
        });
        // Long enough for that `peak` to be in the worker's hands rather than
        // still in the channel — otherwise `Shutdown` could win the race and
        // the drop would be testing nothing. `peak` itself now times out after
        // TEST_TIMEOUT, so this waits less than that.
        std::thread::sleep(TEST_TIMEOUT / 4);

        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            drop(mic);
            let _ = done_tx.send(started.elapsed());
        });

        let elapsed = done_rx
            .recv_timeout(COMMAND_TIMEOUT)
            .expect("MicHandle::drop hung on a wedged worker — the join is unbounded again");
        assert!(elapsed >= TEST_TIMEOUT, "drop gave up before the bound: {elapsed:?}");

        // Let the detached worker finish so it does not outlive the test run.
        let _ = release_tx.send(());
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
        let (mic, probe) = spawn_counting_fake();
        let tap = mic.tap();

        tap.start_metering().unwrap();
        tap.start_metering().unwrap();
        assert_eq!(probe.opens(), 1, "second start must not reopen");
    }

    #[test]
    fn stop_metering_twice_is_a_no_op_not_an_error() {
        let (mic, _probe) = spawn_counting_fake();
        let tap = mic.tap();

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
        let (mut mic, probe) = spawn_counting_fake();
        let tap = mic.tap();

        tap.start_metering().unwrap();
        assert_eq!(probe.opens(), 1, "initial open");

        mic.select(Some("device-2")).unwrap();
        assert_eq!(probe.opens(), 2, "select must reopen against the new device");

        // And the reopened stream is tracked correctly: a further
        // `start_metering` must not open a *third* time.
        tap.start_metering().unwrap();
        assert_eq!(probe.opens(), 2, "already open after the reopen");
    }

    /// The non-fatal policy applies to the reopen path too: if the new
    /// device's stream fails to open, `select` itself must still succeed
    /// (the device *was* selected), and the worker must keep serving with
    /// the meter simply falling back to passive polling.
    #[test]
    fn a_failed_reopen_on_device_change_does_not_fail_select_or_kill_the_worker() {
        let (mut mic, probe) = spawn_counting_fake();
        let tap = mic.tap();

        tap.start_metering().unwrap();
        probe.fail_open.store(true, Ordering::SeqCst);

        mic.select(Some("device-2")).expect("select must succeed even if the reopen fails");
        assert_eq!(probe.opens(), 2, "a reopen must still be attempted");

        // Worker is still alive and serving.
        assert!(!mic.is_muted().unwrap());
    }

    /// Selecting a device while nothing is metering must not touch the
    /// capture stream at all — there is nothing to reopen.
    #[test]
    fn selecting_a_device_without_an_active_meter_does_not_open_a_stream() {
        let (mut mic, probe) = spawn_counting_fake();
        mic.select(Some("device-2")).unwrap();
        assert_eq!(probe.opens(), 0);
    }

    // ---- Task 9b: `Command::DeviceChanged` ----------------------------------

    /// The headline property: a hotplug event must move a running capture
    /// stream onto whatever device the endpoint now resolves to. Left on the
    /// old one, Windows' microphone-in-use indicator stays lit on hardware the
    /// user has just unplugged, and the meter reads silence forever.
    #[test]
    fn a_device_change_while_metering_refreshes_and_reopens_the_capture_stream() {
        let (mic, probe) = spawn_counting_fake();
        let tap = mic.tap();

        tap.start_metering().unwrap();
        assert_eq!(probe.opens(), 1, "initial open");

        send_device_changed(&mic);

        assert_eq!(probe.refreshes(), 1, "the handler must re-resolve the device");
        assert_eq!(probe.opens(), 2, "the stream must follow the device");

        // The reopened stream is tracked, not leaked: a later `start_metering`
        // must find one already open rather than opening a third.
        tap.start_metering().unwrap();
        assert_eq!(probe.opens(), 2, "already open after the reopen");
    }

    /// With no meter running there is no stream to move, so a device change
    /// must refresh and stop there — not open a stream nobody asked for, which
    /// would light the microphone indicator with the settings window closed.
    #[test]
    fn a_device_change_without_an_active_meter_refreshes_but_opens_nothing() {
        let (mic, probe) = spawn_counting_fake();

        send_device_changed(&mic);

        assert_eq!(probe.refreshes(), 1);
        assert_eq!(probe.opens(), 0, "a closed meter must stay closed");
    }

    /// A device that is merely absent — disabled in Sound settings, or
    /// unplugged — must not cost the user their choice. Keeping `selected_id`
    /// is what makes a replug restore it by itself, and it matches the
    /// behaviour Task 9 established for a failed `select`.
    #[test]
    fn a_failed_refresh_leaves_the_selected_device_intact() {
        let (mut mic, probe) = spawn_counting_fake();
        mic.select(Some("device-2")).unwrap();
        assert_eq!(probe.selected().as_deref(), Some("device-2"));

        probe.fail_refresh.store(true, Ordering::SeqCst);
        send_device_changed(&mic);

        assert_eq!(probe.refreshes(), 1, "the handler must have tried");
        assert_eq!(
            probe.selected().as_deref(),
            Some("device-2"),
            "a failed refresh must not demote the user to the system default"
        );
    }

    /// The other half of a failed refresh: the endpoint never moved, so the
    /// stream is still valid and must be left alone. Tearing it down and
    /// reopening it against the same dead device would drop metering for no
    /// reason — and could fail, leaving nothing at all.
    #[test]
    fn a_failed_refresh_does_not_disturb_a_running_capture_stream() {
        let (mic, probe) = spawn_counting_fake();
        let tap = mic.tap();

        tap.start_metering().unwrap();
        probe.fail_refresh.store(true, Ordering::SeqCst);

        send_device_changed(&mic);

        assert_eq!(probe.opens(), 1, "the stream must not be reopened when nothing moved");
    }

    /// A notification storm must not take the worker down, and each event must
    /// leave exactly one stream open rather than stacking them.
    #[test]
    fn repeated_device_changes_keep_exactly_one_stream_open() {
        let (mut mic, probe) = spawn_counting_fake();
        let tap = mic.tap();
        tap.start_metering().unwrap();

        for _ in 0..5 {
            send_device_changed(&mic);
        }

        assert_eq!(probe.refreshes(), 5);
        assert_eq!(probe.opens(), 6, "one initial open plus one reopen per change");
        // Still serving, and still holding a single stream.
        tap.start_metering().unwrap();
        assert_eq!(probe.opens(), 6);
        assert!(!mic.is_muted().unwrap());
        mic.select(None).expect("the worker must still accept ordinary commands");
    }

    /// A device change against a dead worker must be a silent no-op, not a
    /// panic: `Command::DeviceChanged` is sent from a COM notification thread
    /// belonging to the Windows audio service, which is the last thread in the
    /// process that should be unwinding.
    #[test]
    fn a_device_change_sent_to_a_dead_worker_is_dropped_silently() {
        let (mic, _probe) = spawn_counting_fake();
        let orphan = mic.tx.clone();
        drop(mic);

        // Exactly what the closure in `enable_hotplug` does.
        let _ = orphan.send(Command::DeviceChanged);
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

    /// `Endpoint::refresh()` against real hardware, driven the way a hotplug
    /// callback drives it.
    ///
    /// This does not need a device to actually appear or disappear: the
    /// command is what the callback sends, so injecting it exercises the whole
    /// worker-side path (re-resolve, drop the stream, reopen against whatever
    /// the endpoint now points at) on real COM interfaces. Whether Windows
    /// delivers the callback in the first place is
    /// `hotplug::tests::a_real_device_change_reaches_the_watcher`'s job.
    ///
    /// Nothing is asserted until the microphone's original mute state has been
    /// written back, so a failure here cannot leave it muted.
    #[test]
    #[ignore = "requires real audio hardware; run with --ignored"]
    fn a_real_device_change_refreshes_the_endpoint_and_reopens_the_capture_stream() {
        let mut mic = MicHandle::spawn().expect("no capture endpoint");
        let tap = mic.tap();

        let original = mic.is_muted().expect("is_muted before the test");

        tap.start_metering().expect("capture stream must open on real hardware");
        std::thread::sleep(Duration::from_millis(50));
        let before = tap.peak().expect("peak before the device change");

        // Byte-for-byte what the notification callback sends: no reply channel.
        mic.tx.send(Command::DeviceChanged).expect("worker still alive");

        // The worker services its queue in order, so this reply returning
        // proves the refresh and the reopen have already happened.
        let after_refresh = mic.is_muted().expect("the endpoint must answer after a refresh");
        std::thread::sleep(Duration::from_millis(50));
        let after = tap.peak().expect("the meter must still be live after the reopen");

        tap.stop_metering().expect("capture stream must close");
        let restore = mic.set_muted(original);
        let restored = mic.is_muted();

        println!(
            "mute: original={original} after_refresh={after_refresh} restored={restored:?} \
             peak_before={before} peak_after={after}"
        );

        restore.expect("must restore the original mute state");
        assert_eq!(restored.unwrap(), original, "microphone left in the wrong state");
        assert_eq!(after_refresh, original, "a refresh must not change the mute state");
        assert!((0.0..=1.0).contains(&after), "peak out of range after the reopen: {after}");

        // Drop joins the worker, which unregisters nothing here (hotplug was
        // never enabled) and releases the endpoint and the apartment.
        drop(mic);
    }
}
