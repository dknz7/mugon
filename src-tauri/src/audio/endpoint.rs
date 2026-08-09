//! Core Audio implementation of `MicControl`. Operates on capture endpoints
//! (`eCapture`), so mute applies system-wide and appears in Windows' own sound
//! settings (§1).

use super::{AudioError, DeviceInfo, MicControl};
use windows::core::PCWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Variant::VT_LPWSTR;

fn win<T>(r: windows::core::Result<T>) -> Result<T, AudioError> {
    r.map_err(|e| AudioError::Windows(e.to_string()))
}

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

// SAFETY: windows-rs marks COM interface wrappers `!Send` because a COM object
// may be apartment-bound (STA), where calls from another thread must be
// marshalled. That is not the case here, and this impl rests on two conditions
// that must both continue to hold. If you are here to delete the `unsafe`,
// check these first:
//
//  1. `Endpoint::new` initialises COM with `COINIT_MULTITHREADED`, so every
//     object it creates lives in the process-wide multi-threaded apartment
//     (MTA). MTA objects are not thread-affine: any thread in the MTA may call
//     them directly, with COM providing no serialisation of its own.
//  2. Because COM provides no serialisation, the caller must. `Endpoint` is
//     only ever reached through a `Mutex` (see the Tauri managed state), so no
//     two threads are ever inside these interfaces at the same time.
//
// Corollary, and the easy one to get wrong: any thread that calls into an
// `Endpoint` must itself have called `CoInitializeEx` (with a compatible model)
// before doing so. A thread that never initialised COM is not in the MTA, and
// calling a COM interface from it is undefined behaviour regardless of the
// `Mutex`. Tauri command handlers and spawned worker threads are not
// COM-initialised for you.
//
// Deliberately no `unsafe impl Sync`: `&Endpoint` must not be shared across
// threads, because that would allow the concurrent access condition 2 forbids.
// `Mutex<Endpoint>` is `Sync` on the strength of `Send` alone, which is all the
// app needs.
unsafe impl Send for Endpoint {}

impl Endpoint {
    pub fn new() -> Result<Self, AudioError> {
        unsafe {
            // Ignore RPC_E_CHANGED_MODE — another component may already have
            // initialised COM on this thread with a different model.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
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

impl MicControl for Endpoint {
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

    fn select(&mut self, id: Option<&str>) -> Result<(), AudioError> {
        self.selected_id = id.map(str::to_owned);
        self.refresh()
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
