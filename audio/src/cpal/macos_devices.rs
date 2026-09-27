// cpal probes every device by building an AudioUnit for it, which makes coreaudiod run a
// TCC check and open an IO session per device. The recorder polls for its device twice a
// second, so on macOS we list device names straight from the HAL properties instead.

use std::mem;
use std::ptr::null;

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use coreaudio_sys::{
    AudioDeviceID, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectPropertyAddress, kAudioDevicePropertyStreams, kAudioHardwareNoError,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain, kAudioObjectPropertyName,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
};

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn device_ids() -> Vec<AudioDeviceID> {
    let address = address(
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &address, 0, null(), &mut size)
    };
    if status != kAudioHardwareNoError as i32 {
        return vec![];
    }

    let mut ids = vec![0 as AudioDeviceID; size as usize / mem::size_of::<AudioDeviceID>()];
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &address,
            0,
            null(),
            &mut size,
            ids.as_mut_ptr().cast(),
        )
    };
    if status != kAudioHardwareNoError as i32 {
        return vec![];
    }
    ids.truncate(size as usize / mem::size_of::<AudioDeviceID>());
    ids
}

fn has_streams(id: AudioDeviceID, input: bool) -> bool {
    let scope = if input {
        kAudioObjectPropertyScopeInput
    } else {
        kAudioObjectPropertyScopeOutput
    };
    let mut size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            id,
            &address(kAudioDevicePropertyStreams, scope),
            0,
            null(),
            &mut size,
        )
    };
    status == kAudioHardwareNoError as i32 && size > 0
}

fn device_name(id: AudioDeviceID) -> Option<String> {
    let mut name: CFStringRef = null();
    let mut size = mem::size_of::<CFStringRef>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            &address(kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal),
            0,
            null(),
            &mut size,
            (&mut name as *mut CFStringRef).cast(),
        )
    };
    if status != kAudioHardwareNoError as i32 || name.is_null() {
        return None;
    }
    // The HAL hands out a retained copy of the name.
    Some(unsafe { CFString::wrap_under_create_rule(name) }.to_string())
}

pub(crate) fn device_names(input: bool) -> Vec<String> {
    device_ids()
        .into_iter()
        .filter(|id| has_streams(*id, input))
        .filter_map(device_name)
        .map(|name| format!("CoreAudio*{name}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::device_names;
    use cpal::traits::{DeviceTrait, HostTrait};

    #[test]
    fn matches_cpal_device_names() {
        let host = cpal::host_from_id(cpal::HostId::CoreAudio).unwrap();
        for input in [true, false] {
            let devices = if input {
                host.input_devices().unwrap().collect::<Vec<_>>()
            } else {
                host.output_devices().unwrap().collect::<Vec<_>>()
            };
            let mut expected: Vec<_> = devices
                .iter()
                .map(|device| format!("CoreAudio*{}", device.name().unwrap()))
                .collect();
            let mut names = device_names(input);
            expected.sort();
            names.sort();
            assert_eq!(names, expected);
        }
    }
}
