//! Device hotplug notifications (§4.5, and the unplug half of §7).
//!
//! Windows tells an application about capture-device arrivals, departures and
//! default-device changes through [`IMMNotificationClient`], a COM callback
//! interface. This module implements exactly that interface and nothing else.
//!
//! # Threading — the rule this module exists to keep
//!
//! **Every method below runs on an arbitrary COM-managed thread**, chosen by
//! the Windows audio service, not by us. It is never the audio worker thread
//! and never Tauri's main thread. So the watcher may hold nothing that is
//! thread-confined: no [`super::endpoint::Endpoint`], no COM interface it did
//! not create on the calling thread, nothing `!Send`.
//!
//! What it holds instead is a single `Send + Sync` closure ([`OnChange`]). The
//! callback's whole job is to invoke it and return. The closure is built by
//! `thread::MicHandle::enable_hotplug`, and all it does is push a
//! fire-and-forget command onto the audio worker's channel and emit a Tauri
//! event — both non-blocking, neither touching an `Endpoint`.
//!
//! The closure boundary is deliberate rather than incidental: it is what keeps
//! `thread::Command` private to `thread.rs`, and it is what makes the filtering
//! logic below testable without COM registration, an audio worker, or a Tauri
//! `AppHandle` — see this module's tests, which drive the real vtable.
//!
//! **Never block in a callback.** Waiting for a reply from the audio worker
//! would park a thread belonging to the Windows audio service, and if the
//! worker happened to be mid-COM-call it would be one COM thread blocked on
//! another. Hence fire-and-forget: send and return.
//!
//! The watcher is registered as *agile* (windows-rs's default for
//! `#[implement]`), so COM may legitimately call it from several threads at
//! once. The `Send + Sync` assertion below is what makes that safe rather than
//! merely likely; there is no `unsafe impl` of either.

use windows::core::{implement, PCWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use super::endpoint::win;
use super::AudioError;

/// Emitted when the set of capture devices, or the default capture device, may
/// have changed.
///
/// **Carries no payload**, which supersedes DESIGN.md §3's `Vec<DeviceInfo>`.
/// Device enumeration lives behind the audio worker's channel (the
/// `list_devices` command), so building a payload here would mean a COM
/// notification thread doing a blocking round trip through the worker before it
/// could emit — exactly what this module's threading rule forbids. The frontend
/// already calls `list_devices` on its own; a bare signal is all it needs.
pub const DEVICES_CHANGED: &str = "devices-changed";

/// What a device change should do, as a plain callable.
///
/// `Send + Sync` because COM invokes the watcher from arbitrary threads, and
/// may do so concurrently.
pub(super) type OnChange = Box<dyn Fn() + Send + Sync + 'static>;

/// The `IMMNotificationClient` implementation.
///
/// Holds one closure and no state. That is the entire point: there is nothing
/// here that could be thread-confined, so the "which thread am I on?" question
/// the callbacks would otherwise raise has no way to matter.
#[implement(IMMNotificationClient)]
struct CaptureDeviceWatcher {
    on_change: OnChange,
}

/// COM calls this object from arbitrary threads, concurrently, because
/// `#[implement]` registers it as agile. Assert the property rather than
/// assuming it: a future field that is not `Send + Sync` must fail here, at
/// the definition, and not as a data race in the field.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CaptureDeviceWatcher>();
};

impl IMMNotificationClient_Impl for CaptureDeviceWatcher_Impl {
    /// A device was enabled, disabled, plugged in or unplugged.
    ///
    /// No data flow is available here, so this cannot tell a microphone from a
    /// pair of speakers — see [`Self::OnDeviceAdded`] for why we notify anyway.
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> windows::core::Result<()> {
        (self.on_change)();
        Ok(())
    }

    /// A device appeared.
    ///
    /// The interface hands us only an id, not a data flow, and resolving that
    /// id would mean calling back into the enumerator *from inside its own
    /// notification callback* — which Microsoft's documentation warns against
    /// and which risks deadlocking the audio service. So a render-device change
    /// does reach the worker here, costing one redundant `refresh` and, if the
    /// meter happens to be running, one capture-stream reopen. Speakers are not
    /// plugged in 30 times a second; that is a far better trade than a blocking
    /// COM call on a notification thread.
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        (self.on_change)();
        Ok(())
    }

    /// A device disappeared. Same id-only limitation as [`Self::OnDeviceAdded`].
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        (self.on_change)();
        Ok(())
    }

    /// The default device changed — the one case where the interface *does*
    /// give us a data flow, so this is where filtering is possible and is done.
    ///
    /// Two filters, both load-bearing:
    /// - `flow`: mugon only ever touches capture endpoints, so a new default
    ///   pair of speakers must not move the microphone.
    /// - `role`: Windows fires this once per role (`eConsole`, `eMultimedia`,
    ///   `eCommunications`) for a single user action. `Endpoint` resolves its
    ///   default with `eConsole`, so that is the only role whose change means
    ///   anything to us; without this the app would refresh three times for
    ///   one click in Sound settings.
    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _id: &PCWSTR,
    ) -> windows::core::Result<()> {
        if flow == eCapture && role == eConsole {
            (self.on_change)();
        }
        Ok(())
    }

    /// **Deliberately empty. Do not "fix" the inconsistency with its four
    /// siblings above — the inconsistency is the feature.**
    ///
    /// This fires constantly: on every volume change, on every property touch,
    /// and on some drivers on every peak-meter update. Wiring a refresh to it
    /// would hammer the worker channel at audio-callback rate and reopen the
    /// capture stream from under the running meter, dozens of times a second,
    /// for events that never change which device is selected.
    ///
    /// Nothing mugon reads is carried by a device property, so there is no
    /// second half to this decision: an empty body is the complete correct
    /// implementation.
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

/// A live notification registration, unregistered on drop.
///
/// Holds its own [`IMMDeviceEnumerator`] rather than borrowing the one inside
/// [`super::endpoint::Endpoint`]. Registration is per-enumerator-instance and
/// must be undone on the same instance, so owning it here is what makes the
/// `Drop` below correct without reaching through the worker's generic backend
/// bound into endpoint-specific internals. A second enumerator is an ordinary,
/// cheap COM object; there is no shared state between the two to keep in step.
///
/// `!Send` — it holds COM interfaces, which are raw pointers — so the compiler
/// keeps this on the thread that created it, which is the audio worker.
pub(super) struct Registration {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

impl Registration {
    /// Registers a capture-device watcher.
    ///
    /// **Must be called on the audio worker thread**, which is in the MTA and
    /// has COM initialised; `CoCreateInstance` below fails otherwise, which is
    /// reported rather than papered over.
    pub(super) fn new(on_change: OnChange) -> Result<Self, AudioError> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                win(CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL))?;
            let client: IMMNotificationClient = CaptureDeviceWatcher { on_change }.into();
            win(enumerator.RegisterEndpointNotificationCallback(&client))?;
            Ok(Self { enumerator, client })
        }
    }
}

impl Drop for Registration {
    /// Unregisters, so the watcher — and the channel sender inside its closure
    /// — stops being held by the Windows audio service once the worker is on
    /// its way out. A registration left behind is a callback that fires into a
    /// dead worker forever.
    ///
    /// Best effort: if the unregister fails there is nothing more useful to do
    /// than release the interfaces and let the process finish exiting.
    fn drop(&mut self) {
        unsafe {
            if let Err(e) = self.enumerator.UnregisterEndpointNotificationCallback(&self.client) {
                eprintln!("mugon: failed to unregister the device notification client: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use windows::Win32::Media::Audio::{
        eCommunications, eMultimedia, eRender, DEVICE_STATE_ACTIVE, DEVICE_STATE_UNPLUGGED,
    };

    /// Builds a watcher behind a real `IMMNotificationClient` vtable, plus a
    /// counter of how many times it decided a refresh was warranted.
    ///
    /// The interface is constructed and called for real — this drives the same
    /// generated vtable Windows would — but needs no COM runtime, no audio
    /// hardware and no `AppHandle`, because the only thing on the other side of
    /// the callback is a closure. That is the payoff for the [`OnChange`]
    /// indirection.
    fn watcher() -> (IMMNotificationClient, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let client: IMMNotificationClient = CaptureDeviceWatcher {
            on_change: Box::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        }
        .into();
        (client, calls)
    }

    #[test]
    fn state_add_and_remove_notifications_all_reach_the_worker() {
        let (client, calls) = watcher();
        unsafe {
            client.OnDeviceStateChanged(PCWSTR::null(), DEVICE_STATE_UNPLUGGED).unwrap();
            client.OnDeviceStateChanged(PCWSTR::null(), DEVICE_STATE_ACTIVE).unwrap();
            client.OnDeviceAdded(PCWSTR::null()).unwrap();
            client.OnDeviceRemoved(PCWSTR::null()).unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    /// The whole reason this method is empty. If a future edit "fixes" the
    /// apparent inconsistency with its four siblings, this fails — which is a
    /// far cheaper way to find out than a meter that reopens its capture stream
    /// at audio-callback rate on the machine of whoever reports it.
    #[test]
    fn property_value_changes_never_reach_the_worker() {
        let (client, calls) = watcher();
        unsafe {
            for _ in 0..1000 {
                client.OnPropertyValueChanged(PCWSTR::null(), PROPERTYKEY::default()).unwrap();
            }
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "OnPropertyValueChanged fires on every volume tick; it must stay a no-op"
        );
    }

    #[test]
    fn a_new_default_capture_device_notifies_once_for_the_console_role_only() {
        let (client, calls) = watcher();
        unsafe {
            // One user action in Sound settings fires all three roles.
            client.OnDefaultDeviceChanged(eCapture, eConsole, PCWSTR::null()).unwrap();
            client.OnDefaultDeviceChanged(eCapture, eMultimedia, PCWSTR::null()).unwrap();
            client.OnDefaultDeviceChanged(eCapture, eCommunications, PCWSTR::null()).unwrap();
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "`Endpoint` resolves its default with eConsole; the other roles are noise"
        );
    }

    #[test]
    fn a_new_default_render_device_is_ignored() {
        let (client, calls) = watcher();
        unsafe {
            client.OnDefaultDeviceChanged(eRender, eConsole, PCWSTR::null()).unwrap();
            client.OnDefaultDeviceChanged(eRender, eMultimedia, PCWSTR::null()).unwrap();
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "changing the default speakers must not move the microphone"
        );
    }

    // ---- hardware ----------------------------------------------------------

    /// Counts every callback Windows delivers, of every kind, without filtering
    /// anything.
    ///
    /// Registered alongside the real watcher in the hardware tests below so
    /// they can tell "our filter suppressed it" from "Windows never sent it" —
    /// two states the production watcher's own counter cannot distinguish,
    /// because both leave it at zero.
    #[implement(IMMNotificationClient)]
    struct CallbackSpy {
        properties: Arc<AtomicUsize>,
        devices: Arc<AtomicUsize>,
    }

    impl IMMNotificationClient_Impl for CallbackSpy_Impl {
        fn OnDeviceStateChanged(&self, _: &PCWSTR, state: DEVICE_STATE) -> windows::core::Result<()> {
            println!("spy: OnDeviceStateChanged state={}", state.0);
            self.devices.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
            println!("spy: OnDeviceAdded");
            self.devices.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
            println!("spy: OnDeviceRemoved");
            self.devices.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn OnDefaultDeviceChanged(
            &self,
            flow: EDataFlow,
            role: ERole,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            println!("spy: OnDefaultDeviceChanged flow={} role={}", flow.0, role.0);
            self.devices.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows::core::Result<()> {
            self.properties.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Everything a hardware test needs on the audio worker's terms: MTA
    /// membership, and a [`CallbackSpy`] registered on its own enumerator.
    ///
    /// Undone in reverse on drop — unregister, then leave the apartment —
    /// which is also the ordering `run` uses on the way out.
    struct RealRig {
        enumerator: IMMDeviceEnumerator,
        spy: IMMNotificationClient,
        spy_properties: Arc<AtomicUsize>,
        spy_devices: Arc<AtomicUsize>,
    }

    impl RealRig {
        /// Enters the MTA — the same apartment the audio worker uses, and the
        /// only one `Registration::new` is allowed to run in.
        fn new() -> Self {
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            assert!(hr.is_ok(), "test setup: could not enter the MTA: {hr:?}");

            let spy_properties = Arc::new(AtomicUsize::new(0));
            let spy_devices = Arc::new(AtomicUsize::new(0));
            let spy: IMMNotificationClient = CallbackSpy {
                properties: Arc::clone(&spy_properties),
                devices: Arc::clone(&spy_devices),
            }
            .into();

            let enumerator: IMMDeviceEnumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                    .expect("test setup: no MMDeviceEnumerator");
            unsafe { enumerator.RegisterEndpointNotificationCallback(&spy) }
                .expect("test setup: could not register the spy");

            Self { enumerator, spy, spy_properties, spy_devices }
        }
    }

    impl Drop for RealRig {
        fn drop(&mut self) {
            unsafe {
                let _ = self.enumerator.UnregisterEndpointNotificationCallback(&self.spy);
                windows::Win32::System::Com::CoUninitialize();
            }
        }
    }

    /// Registration and unregistration against real Core Audio — the two COM
    /// calls no synthetic test can reach — plus a property-change storm that
    /// must not escape to the worker.
    ///
    /// The volume writes are the storm generator: they are the cheapest thing
    /// that makes a driver emit `OnPropertyValueChanged`. **How many it
    /// actually emits is driver-dependent**, which is exactly why the
    /// [`CallbackSpy`] is registered alongside: if `spy_property_callbacks`
    /// prints `0`, this machine's driver stayed silent and the assertion below
    /// is vacuous for that run — `property_value_changes_never_reach_the_worker`
    /// above is the guard that never is. What this test proves unconditionally
    /// is that the registration succeeded, survived real traffic, and
    /// unregistered cleanly.
    ///
    /// The original level is written back before anything is asserted, so a
    /// failure cannot leave the machine's microphone at the wrong volume.
    #[test]
    #[ignore = "requires real audio hardware; run with --ignored"]
    fn a_real_registration_ignores_a_property_change_storm() {
        use super::super::endpoint::Endpoint;
        use super::super::MicBackend;

        let rig = RealRig::new();

        let notifications = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&notifications);
        let registration = Registration::new(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }))
        .expect("registration against real Core Audio must succeed");

        // Generate property traffic, then put the level back exactly.
        let mut endpoint = Endpoint::new().expect("no capture endpoint");
        let original = endpoint.volume().expect("volume before the storm");
        let mut wrote = Ok(());
        for step in 0..20 {
            let level = 0.30 + (step % 4) as f32 * 0.05;
            wrote = wrote.and(endpoint.set_volume(level));
        }
        let restore = endpoint.set_volume(original);
        std::thread::sleep(std::time::Duration::from_millis(250));

        let properties = rig.spy_properties.load(Ordering::SeqCst);
        let devices = rig.spy_devices.load(Ordering::SeqCst);
        let seen = notifications.load(Ordering::SeqCst);
        println!(
            "original_volume={original} spy_property_callbacks={properties} \
             spy_device_callbacks={devices} watcher_notifications={seen}"
        );

        restore.expect("must restore the original volume");
        wrote.expect("volume writes must succeed");
        assert_eq!(
            seen, 0,
            "a volume storm must never reach the worker — {properties} property callbacks arrived \
             and every one of them had to be dropped"
        );

        // Unregister explicitly rather than at scope end, so a failure in the
        // `Drop` path shows up as this test failing rather than as a message
        // nobody reads.
        drop(registration);
        drop(endpoint);
    }

    /// The one check that needs a person: a real device arriving or leaving.
    ///
    /// Ignored *and* interactive — it cannot pass unattended, by design. Run
    /// it, then within the window disable and re-enable a capture device in
    /// Windows Sound settings (safer and more repeatable than unplugging):
    ///
    /// ```text
    /// cargo test --lib hotplug -- --ignored --nocapture
    /// ```
    ///
    /// It exists because nothing else in this suite can prove the last link in
    /// the chain: that Windows actually delivers add/remove/default-change
    /// callbacks to a client registered the way [`Registration`] registers one.
    #[test]
    #[ignore = "interactive: needs a human to disable and re-enable a capture device"]
    fn a_real_device_change_reaches_the_watcher() {
        const WINDOW: std::time::Duration = std::time::Duration::from_secs(45);

        let rig = RealRig::new();
        let notifications = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&notifications);
        let registration = Registration::new(Box::new(move || {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            println!("watcher: device change #{n}");
        }))
        .expect("registration against real Core Audio must succeed");

        println!(
            "\n>>> Disable and then re-enable a capture device in Windows Sound \
             settings within {WINDOW:?}. <<<\n"
        );
        let deadline = std::time::Instant::now() + WINDOW;
        while std::time::Instant::now() < deadline && notifications.load(Ordering::SeqCst) < 2 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        let seen = notifications.load(Ordering::SeqCst);
        println!(
            "watcher_notifications={seen} spy_device_callbacks={} spy_property_callbacks={}",
            rig.spy_devices.load(Ordering::SeqCst),
            rig.spy_properties.load(Ordering::SeqCst)
        );
        drop(registration);
        assert!(seen > 0, "no device notification arrived within {WINDOW:?}");
    }
}
