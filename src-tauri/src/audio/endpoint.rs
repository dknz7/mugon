//! Core Audio access to the microphone. Operates on capture endpoints
//! (`eCapture`), so mute applies system-wide and appears in Windows' own sound
//! settings (§1).
//!
//! # Thread confinement
//!
//! `Endpoint` is thread-confined **by design**. It holds windows-rs COM
//! interfaces, which are `!Send`, and it deliberately stays that way: it must be
//! constructed and used on one single thread, and that thread must be in the
//! multi-threaded apartment (`Endpoint::new` enforces this and fails loudly
//! otherwise).
//!
//! Confinement is not a limitation we are working around, it is the only sound
//! option here. Tao calls `OleInitialize` on Tauri's window-creation thread,
//! putting it in an **STA**; a later `CoInitializeEx(MTA)` there returns
//! `RPC_E_CHANGED_MODE` and leaves every COM object apartment-bound. An
//! apartment-bound object is not safe to touch from another thread no matter
//! what lock guards it, so no `unsafe impl Send` can rescue this — the objects
//! have to live on a thread we control.
//!
//! Consequently `Endpoint` implements [`super::MicBackend`] — the non-`Send`
//! half of the pair — and can **never** implement [`super::MicControl`], which
//! is `MicBackend + Send`. That is not a convention anyone has to remember: it
//! is enforced by the compiler, and pinned by a `compile_fail` doctest below.
//! [`super::thread::MicHandle`] is the only legitimate way to drive these
//! methods from a `Send` context.

use super::{AudioError, DeviceInfo, MicBackend};
use windows::core::PCWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Variant::VT_LPWSTR;

fn win<T>(r: windows::core::Result<T>) -> Result<T, AudioError> {
    r.map_err(|e| AudioError::Windows(e.to_string()))
}

/// A Core Audio capture endpoint, bound for life to the thread that built it.
///
/// The invariant the whole confinement design rests on: an `Endpoint` is a
/// `MicBackend`, but it is **not** `Send`, so it can never satisfy
/// `MicControl`. Anything that wants a `MicControl` must go through
/// [`super::thread::MicHandle`].
///
/// The bound resolves for the `Send` proxy:
///
/// ```
/// fn requires_mic_control<T: mugon_lib::audio::MicControl>() {}
/// requires_mic_control::<mugon_lib::audio::thread::MicHandle>();
/// ```
///
/// and is rejected for the apartment-bound endpoint. If this ever starts
/// compiling, something has made `Endpoint` `Send` and the confinement is gone:
///
/// ```compile_fail
/// fn requires_mic_control<T: mugon_lib::audio::MicControl>() {}
/// requires_mic_control::<mugon_lib::audio::endpoint::Endpoint>();
/// ```
pub struct Endpoint {
    enumerator: IMMDeviceEnumerator,
    /// `None` means "follow the system default" (§4.5).
    selected_id: Option<String>,
    /// The live endpoint the `volume` and `meter` interfaces were activated
    /// from. Held so `refresh` can swap all three together.
    device: IMMDevice,
    volume: IAudioEndpointVolume,
    meter: IAudioMeterInformation,
}

// Note: no `unsafe impl Send for Endpoint`. See the thread-confinement section
// in the module docs — the objects are potentially apartment-bound, so `Send`
// here would be unsound rather than merely unproven.

impl Endpoint {
    pub fn new() -> Result<Self, AudioError> {
        unsafe {
            // The calling thread must be in the MTA, and this is the only place
            // that can tell. `RPC_E_CHANGED_MODE` means the thread is already
            // in a *different* apartment — typically an STA, because tao calls
            // `OleInitialize` on Tauri's window thread. Constructing here would
            // leave every COM object below apartment-bound to a thread we do
            // not control, which no amount of locking makes safe. Refuse.
            //
            // `S_FALSE` is success: COM was already initialised on this thread
            // in the same mode, so we are in the MTA as required.
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr == RPC_E_CHANGED_MODE {
                return Err(AudioError::Windows(
                    "thread is already in a different COM apartment (RPC_E_CHANGED_MODE); \
                     Endpoint must be constructed on a dedicated MTA thread"
                        .into(),
                ));
            }
            if hr != S_OK && hr != S_FALSE {
                return Err(AudioError::Windows(format!(
                    "CoInitializeEx failed: {}",
                    hr.message()
                )));
            }
            let enumerator: IMMDeviceEnumerator =
                win(CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL))?;
            let device = win(enumerator.GetDefaultAudioEndpoint(eCapture, eConsole))?;
            let (volume, meter) = Self::activate(&device)?;
            Ok(Self { enumerator, selected_id: None, device, volume, meter })
        }
    }

    unsafe fn activate(
        device: &IMMDevice,
    ) -> Result<(IAudioEndpointVolume, IAudioMeterInformation), AudioError> {
        let volume: IAudioEndpointVolume = win(device.Activate(CLSCTX_ALL, None))?;
        let meter: IAudioMeterInformation = win(device.Activate(CLSCTX_ALL, None))?;
        Ok((volume, meter))
    }

    unsafe fn friendly_name(device: &IMMDevice) -> Result<String, AudioError> {
        let store = win(device.OpenPropertyStore(STGM_READ))?;
        let mut value = win(store.GetValue(&PKEY_Device_FriendlyName))?;
        // `PROPVARIANT` in windows 0.61 is the raw union with no accessors and
        // no `Drop`, so the string is read out by hand and the variant freed
        // explicitly.
        let name = {
            let inner = &value.Anonymous.Anonymous;
            if inner.vt == VT_LPWSTR && !inner.Anonymous.pwszVal.is_null() {
                inner.Anonymous.pwszVal.to_string().ok()
            } else {
                None
            }
        };
        let _ = PropVariantClear(&mut value);
        match name {
            Some(n) if !n.is_empty() => Ok(n),
            _ => Err(AudioError::Windows(
                "device friendly name is missing or not a string".into(),
            )),
        }
    }

    unsafe fn device_id(device: &IMMDevice) -> Result<String, AudioError> {
        // `GetId` hands back a `PWSTR` allocated with the COM task allocator;
        // it is ours to free.
        let raw = win(device.GetId())?;
        if raw.is_null() {
            return Err(AudioError::Windows("device returned a null id".into()));
        }
        let id = raw.to_string();
        CoTaskMemFree(Some(raw.as_ptr() as *const core::ffi::c_void));
        id.map_err(|e| AudioError::Windows(e.to_string()))
    }

    /// Re-resolves `selected_id` to a live device. Called after a hotplug event
    /// and whenever the default device changes.
    ///
    /// Also the entry point the later `IMMNotificationClient` task will call
    /// from its device-change callback; that callback needs an `AppHandle`
    /// which does not exist yet, so for now `select` is the only caller.
    pub fn refresh(&mut self) -> Result<(), AudioError> {
        unsafe {
            let device = match &self.selected_id {
                Some(id) => {
                    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
                    win(self.enumerator.GetDevice(PCWSTR(wide.as_ptr())))?
                }
                None => win(self.enumerator.GetDefaultAudioEndpoint(eCapture, eConsole))?,
            };
            let (volume, meter) = Self::activate(&device)?;
            self.device = device;
            self.volume = volume;
            self.meter = meter;
            Ok(())
        }
    }
}

/// The seven backend operations. `Endpoint` implements [`MicBackend`], the
/// non-`Send` half of the pair, and **cannot** implement [`super::MicControl`]:
/// that is `MicBackend + Send`, and these COM interfaces are apartment-bound.
/// The `compile_fail` doctest on the struct above pins that down.
///
/// Reaching these from a `Send` context is [`super::thread::MicHandle`]'s job.
impl MicBackend for Endpoint {
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        unsafe {
            let default_id = self
                .enumerator
                .GetDefaultAudioEndpoint(eCapture, eConsole)
                .ok()
                .and_then(|d| Self::device_id(&d).ok());
            let collection =
                win(self.enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE))?;
            let count = win(collection.GetCount())?;
            let mut out = Vec::with_capacity(count as usize);
            for i in 0..count {
                let device = win(collection.Item(i))?;
                let id = Self::device_id(&device)?;
                let is_default = Some(&id) == default_id.as_ref();
                out.push(DeviceInfo {
                    name: Self::friendly_name(&device).unwrap_or_else(|_| id.clone()),
                    id,
                    is_default,
                });
            }
            Ok(out)
        }
    }

    /// A failed `select` is a no-op. If `refresh` cannot resolve `id` — a stale
    /// or unplugged device — the previous selection is put back, so the struct
    /// never ends up believing it targets a device it could not reach while
    /// `device`/`volume`/`meter` still point at the old one. Without this, every
    /// later `refresh` (including the future hotplug callback) would re-attempt
    /// the bad id and silently pin the app to the stale endpoint.
    fn select(&mut self, id: Option<&str>) -> Result<(), AudioError> {
        let previous = self.selected_id.take();
        self.selected_id = id.map(str::to_owned);
        match self.refresh() {
            Ok(()) => Ok(()),
            Err(e) => {
                self.selected_id = previous;
                Err(e)
            }
        }
    }

    fn is_muted(&self) -> Result<bool, AudioError> {
        unsafe { Ok(win(self.volume.GetMute())?.as_bool()) }
    }

    fn set_muted(&mut self, muted: bool) -> Result<(), AudioError> {
        unsafe { win(self.volume.SetMute(muted, std::ptr::null())) }
    }

    fn volume(&self) -> Result<f32, AudioError> {
        unsafe { win(self.volume.GetMasterVolumeLevelScalar()) }
    }

    fn set_volume(&mut self, level: f32) -> Result<(), AudioError> {
        unsafe {
            win(self.volume
                .SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null()))
        }
    }

    fn peak(&self) -> Result<f32, AudioError> {
        unsafe { win(self.meter.GetPeakValue()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards Fix 2 and, through it, the whole thread-confinement design: an
    /// `Endpoint` must refuse to be built on a thread that is already in
    /// another apartment. This is exactly the situation tao creates by calling
    /// `OleInitialize` on Tauri's window thread. Needs COM but no audio
    /// hardware, so it is not `#[ignore]`d.
    ///
    /// Runs on its own thread so it cannot disturb the apartment state of any
    /// other test in this process.
    #[test]
    fn refuses_to_construct_on_a_thread_in_another_apartment() {
        let outcome = std::thread::spawn(|| unsafe {
            let hr = CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
            assert!(hr.is_ok(), "test setup: could not enter an STA: {hr:?}");
            Endpoint::new().err().map(|e| e.to_string())
        })
        .join()
        .expect("apartment test thread panicked");

        let message = outcome.expect("Endpoint::new must fail on an STA thread, not succeed");
        assert!(
            message.contains("RPC_E_CHANGED_MODE"),
            "expected an apartment-mismatch error, got: {message}"
        );
    }

    #[test]
    #[ignore = "requires real audio hardware; run with --ignored"]
    fn enumerates_and_toggles_a_real_device() {
        let mut ep = Endpoint::new().expect("no capture endpoint");
        let devices = ep.list_devices().unwrap();
        for d in &devices {
            println!("device: is_default={} name={:?} id={}", d.is_default, d.name, d.id);
        }
        assert!(!devices.is_empty(), "expected at least one capture device");
        assert!(devices.iter().any(|d| d.is_default), "expected a default device");
        assert!(devices.iter().all(|d| !d.name.is_empty()), "device names must resolve");
        assert!(
            devices.iter().all(|d| d.name != d.id),
            "friendly name lookup fell back to the device id"
        );

        // Exercise both arms of `refresh`. The `Some` arm is the only manual
        // UTF-16 conversion and raw-pointer construction left in the file, so
        // it gets a real device id and a round-trip call afterwards to prove
        // the re-activated interfaces actually work.
        let first_id = devices[0].id.clone();
        ep.select(Some(&first_id)).expect("select by id must resolve");
        ep.is_muted().expect("interfaces must work after select(Some)");

        // A failed select must be a no-op: the bad id is rolled back, so the
        // endpoint stays usable and still tracks the previous selection.
        let err = ep.select(Some("{not-a-real-device-id}"));
        assert!(err.is_err(), "selecting a bogus device id must fail");
        ep.is_muted().expect("endpoint must still work after a failed select");
        ep.refresh().expect("failed select must not poison later refreshes");

        ep.select(None).expect("select(None) must follow the system default");
        ep.is_muted().expect("interfaces must work after select(None)");

        // Restore before asserting, so a failed toggle cannot leave the
        // machine's microphone muted.
        let original = ep.is_muted().unwrap();
        ep.set_muted(!original).unwrap();
        let toggled = ep.is_muted().unwrap();
        ep.set_muted(original).unwrap();
        let restored = ep.is_muted().unwrap();
        println!("mute: original={original} toggled={toggled} restored={restored}");
        assert_eq!(toggled, !original, "set_muted must take effect");
        assert_eq!(restored, original, "must restore original state");
    }
}
