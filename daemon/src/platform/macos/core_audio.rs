use std::ffi::c_char;
use std::os::raw::c_void;
use std::ptr::null;
use std::{mem, ptr};

use crate::platform::macos::device::StereoChannels;
use anyhow::Result;
use anyhow::bail;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, TCFType, UInt32, kCFAllocatorDefault};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFMutableDictionary, CFMutableDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use coreaudio_sys::{
    AudioDeviceID, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectSetPropertyData, AudioValueTranslation, KERN_SUCCESS,
    kAudioAggregateDevicePropertyFullSubDeviceList, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyPreferredChannelsForStereo, kAudioHardwareNoError,
    kAudioHardwarePropertyDevices, kAudioHardwarePropertyPlugInForBundleID,
    kAudioObjectPropertyElementMaster, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    kAudioObjectUnknown, kAudioPlugInCreateAggregateDevice, kAudioPlugInDestroyAggregateDevice,
};
use goxlr_usb::{PID_GOXLR_FULL, PID_GOXLR_MINI, VID_GOXLR};
use io_kit_sys::types::io_iterator_t;
use io_kit_sys::{
    IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperties,
    IOServiceGetMatchingServices, IOServiceMatching, kIOMasterPortDefault,
};

const CORE_AUDIO_UID: &str = "com.apple.audio.CoreAudio";
const AGGREGATE_PREFIX: &str = "GoXLR-Utility::Aggregate";
const LEGACY_PREFIX: &str = "com.adecorp.goxlr";

fn uid_matches_location(uid: &str, location: u32) -> bool {
    let Some((prefix, _stream)) = uid.rsplit_once(':') else {
        return false;
    };
    let Some((prefix, component)) = prefix.rsplit_once(':') else {
        return false;
    };

    prefix.starts_with("AppleUSBAudioEngine:") && u32::from_str_radix(component, 16) == Ok(location)
}

#[cfg(test)]
mod tests {
    use super::{
        StereoChannels, add_sub_device, create_aggregate_device, destroy_aggregate_device,
        get_goxlr_devices, set_active_channels, uid_matches_location,
    };

    #[test]
    fn matches_only_the_exact_usb_location_component() {
        let uid = "AppleUSBAudioEngine:TC-Helicon:GoXLR:1144400:1,2";
        assert!(uid_matches_location(uid, 0x1144400));
        assert!(!uid_matches_location(uid, 0x114440));
        assert!(!uid_matches_location(uid, 0x1120000));
        assert!(!uid_matches_location(
            "com.rogueamoeba.Loopback:GoXLR:1144400:1,2",
            0x1144400
        ));
    }

    #[test]
    #[ignore = "requires a connected GoXLR"]
    fn finds_connected_goxlr_by_usb_identity() {
        let devices = get_goxlr_devices().unwrap();
        assert!(!devices.is_empty());
        assert!(
            devices
                .iter()
                .all(|device| device.uid.starts_with("AppleUSBAudioEngine:"))
        );
    }

    #[test]
    #[ignore = "temporarily creates CoreAudio devices on a Mac with a GoXLR"]
    fn creates_three_channel_aggregates() {
        struct TemporaryAggregate(Option<u32>);
        impl Drop for TemporaryAggregate {
            fn drop(&mut self) {
                if let Some(id) = self.0.take() {
                    let _ = destroy_aggregate_device(id);
                }
            }
        }

        let device = get_goxlr_devices().unwrap().remove(0);
        for (name, input, channels) in [
            ("Probe Chat", false, StereoChannels { left: 5, right: 6 }),
            ("Probe Music", false, StereoChannels { left: 7, right: 8 }),
            ("Probe Mic", true, StereoChannels { left: 3, right: 4 }),
        ] {
            let id = create_aggregate_device(name.into(), &device).unwrap();
            let mut cleanup = TemporaryAggregate(Some(id));
            add_sub_device(id, device.uid.clone()).unwrap();
            set_active_channels(id, input, channels).unwrap();
            destroy_aggregate_device(id).unwrap();
            for _ in 0..10 {
                if !super::get_audio_device_ids().unwrap().contains(&id) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            assert!(!super::get_audio_device_ids().unwrap().contains(&id));
            cleanup.0 = None;
        }
    }
}

pub struct CoreAudioDevice {
    display_name: String,
    pub(crate) uid: String,
}

pub fn get_id_for_uid(uid: &str) -> anyhow::Result<AudioObjectID> {
    let properties = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyPlugInForBundleID,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };

    let mut size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject,
            &properties,
            0,
            ptr::null(),
            &mut size,
        )
    };
    if status != kAudioHardwareNoError as i32 {
        bail!("Error Lookup up Bundle ID: {}", status);
    }

    if size == 0 {
        bail!("Missing CoreAudio Plugin Size");
    }

    let mut plugin_id = kAudioObjectUnknown;
    let plugin_ref = CFString::new(uid);

    let mut translation_value = AudioValueTranslation {
        mInputData: &plugin_ref as *const CFString as *mut c_void,
        mInputDataSize: mem::size_of::<CFString>() as u32,
        mOutputData: &mut plugin_id as *mut AudioObjectID as *mut c_void,
        mOutputDataSize: mem::size_of::<AudioObjectID>() as u32,
    };

    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &properties,
            0,
            ptr::null(),
            &mut size,
            &mut translation_value as *mut _ as *mut _,
        )
    };

    if status != kAudioHardwareNoError as i32 {
        bail!("Error Fetching CoreAudio Plugin: {}", status);
    }
    Ok(plugin_id)
}

pub fn get_uid_for_id(id: AudioObjectID) -> anyhow::Result<String> {
    let properties = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceUID,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };

    let mut uid: CFStringRef = null();
    let mut size = mem::size_of::<CFStringRef>() as u32;

    let uid = unsafe {
        let status = AudioObjectGetPropertyData(
            id,
            &properties,
            0,
            null(),
            &mut size,
            &mut uid as *mut _ as *mut _,
        );

        if status != kAudioHardwareNoError as i32 {
            bail!("Error Extracting UID for {}", id);
        }

        if uid.is_null() {
            bail!("Missing UID for {}", id);
        }

        CFString::wrap_under_get_rule(uid)
    };

    Ok(uid.to_string())
}

pub fn create_aggregate_device(channel: String, device: &CoreAudioDevice) -> Result<AudioDeviceID> {
    let core_audio_id = get_id_for_uid(CORE_AUDIO_UID)?;

    let properties = AudioObjectPropertyAddress {
        mSelector: kAudioPlugInCreateAggregateDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };

    // I should probably have a method for this..
    let mut size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(core_audio_id, &properties, 0, ptr::null(), &mut size)
    };
    if status != kAudioHardwareNoError as i32 {
        bail!("Create Aggregate Error Getting Size: {}", status);
    }

    // We'll use the UID of the physical device as part of the aggregate's UID
    let uid = format!(
        "{}::{}::{}",
        AGGREGATE_PREFIX,
        device.uid,
        channel.replace(' ', "")
    );

    // Create the Dictionary responsible for building the Aggregate Device..
    let name = format!("{} ({})", channel, device.display_name);
    let dictionary = CFDictionary::from_CFType_pairs(&[
        (
            CFString::new("name").as_CFType(),
            CFString::new(&name).as_CFType(),
        ),
        (
            CFString::new("uid").as_CFType(),
            CFString::new(&uid).as_CFType(),
        ),
        (
            CFString::new("private").as_CFType(),
            CFBoolean::false_value().as_CFType(),
        ),
        (
            CFString::new("stacked").as_CFType(),
            CFBoolean::false_value().as_CFType(),
        ),
    ]);

    let mut device_id = kAudioObjectUnknown;
    let status = unsafe {
        AudioObjectGetPropertyData(
            core_audio_id,
            &properties,
            mem::size_of_val(&dictionary) as UInt32,
            &dictionary as *const _ as *const c_void,
            &mut size as *mut UInt32,
            &mut device_id as *mut _ as *mut _,
        )
    };

    // Bad Property Size - 561211770
    // Illegal Operation - 1852797029

    if status != kAudioHardwareNoError as i32 {
        bail!("Create Aggregate - Unable to Create Device: {}", status);
    }

    if device_id == kAudioObjectUnknown {
        bail!("Create Aggregate - Device broke?")
    }

    Ok(device_id)
}

pub fn destroy_aggregate_device(aggregate: AudioDeviceID) -> Result<()> {
    let core_audio_id = get_id_for_uid(CORE_AUDIO_UID)?;

    let properties = AudioObjectPropertyAddress {
        mSelector: kAudioPlugInDestroyAggregateDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };

    // I should probably have a method for this..
    let mut size = mem::size_of::<AudioDeviceID>() as u32;

    let status = unsafe {
        AudioObjectGetPropertyData(
            core_audio_id,
            &properties,
            0,
            null(),
            &mut size,
            &aggregate as *const _ as *mut _,
        )
    };

    if status != kAudioHardwareNoError as i32 {
        bail!("CoreAudio Error: {}", status);
    }

    Ok(())
}

/// Adds a Sub-device to to an aggregate devices, normally the physical GoXLR Device
pub fn add_sub_device(aggregate: AudioDeviceID, uid: String) -> anyhow::Result<()> {
    let sub_device = CFArray::from_CFTypes(&[CFString::new(&uid)]);
    let sub_device_ref = sub_device.as_concrete_TypeRef();
    unsafe {
        let properties = AudioObjectPropertyAddress {
            mSelector: kAudioAggregateDevicePropertyFullSubDeviceList,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMaster,
        };

        let size = mem::size_of::<CFArrayRef>();
        let status = AudioObjectSetPropertyData(
            aggregate,
            &properties,
            0,
            ptr::null(),
            size as UInt32,
            &sub_device_ref as *const _ as *const c_void,
        );

        if status != kAudioHardwareNoError as i32 {
            bail!("Error Executing Add: {}", status);
        }
    }
    Ok(())
}

/// Set's the Aggregates 'active' channels, this is normally the stereo channels for
/// the virtual outputs / inputs
pub fn set_active_channels(
    id: AudioDeviceID,
    input: bool,
    channels: StereoChannels,
) -> anyhow::Result<()> {
    let scope = if input {
        kAudioObjectPropertyScopeInput
    } else {
        kAudioObjectPropertyScopeOutput
    };

    let properties = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyPreferredChannelsForStereo,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMaster,
    };

    unsafe {
        let value: [UInt32; 2] = [channels.left, channels.right];

        let size = mem::size_of::<UInt32>() * 2;
        let status = AudioObjectSetPropertyData(
            id,
            &properties,
            0,
            ptr::null(),
            size as UInt32,
            &value as *const _ as *const c_void,
        );
        if status != kAudioHardwareNoError as i32 {
            bail!("Unable to Set Stereo Channels: {}", status);
        }
    }

    Ok(())
}

fn get_audio_device_ids() -> Result<Vec<AudioDeviceID>> {
    let properties = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };

    let mut size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &properties, 0, null(), &mut size)
    };
    if status != kAudioHardwareNoError as i32 {
        bail!("CoreAudio Error: {}", status);
    }

    let count: usize = size as usize / mem::size_of::<AudioDeviceID>();
    let mut device_ids = vec![kAudioObjectUnknown; count];
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &properties,
            0,
            null(),
            &mut size,
            device_ids.as_mut_ptr() as *mut _,
        )
    };
    if status != kAudioHardwareNoError as i32 {
        bail!("CoreAudio Error: {}", status);
    }
    device_ids.truncate(size as usize / mem::size_of::<AudioDeviceID>());

    Ok(device_ids)
}

pub fn find_all_existing_aggregates() -> Result<Vec<AudioDeviceID>> {
    let mut device_list = Vec::new();
    for device in get_audio_device_ids()? {
        if let Ok(uid) = get_uid_for_id(device)
            && (uid.starts_with(AGGREGATE_PREFIX) || uid.starts_with(LEGACY_PREFIX))
        {
            device_list.push(device);
        }
    }

    Ok(device_list)
}

/// Maps a verified GoXLR USB device to its CoreAudio UID through its USB location ID.
pub fn get_goxlr_devices() -> Result<Vec<CoreAudioDevice>> {
    let audio_uids: Vec<_> = get_audio_device_ids()?
        .into_iter()
        .filter_map(|id| get_uid_for_id(id).ok())
        .collect();
    let mut devices = Vec::new();

    let mut iterator = mem::MaybeUninit::<io_iterator_t>::uninit();
    let matcher = unsafe { IOServiceMatching(c"IOUSBHostDevice".as_ptr() as *const c_char) };
    let status = unsafe {
        IOServiceGetMatchingServices(kIOMasterPortDefault, matcher, iterator.as_mut_ptr())
    };
    if status != KERN_SUCCESS as i32 {
        bail!("Failed to Get Matching Service: {}", status);
    }
    let iterator = unsafe { iterator.assume_init() };

    let vid_key = CFString::new("idVendor");
    let pid_key = CFString::new("idProduct");
    let location_key = CFString::new("locationID");

    loop {
        let service = unsafe { IOIteratorNext(iterator) };
        if service == 0 {
            break;
        }

        let mut dictionary: CFMutableDictionaryRef = ptr::null_mut();
        let status = unsafe {
            IORegistryEntryCreateCFProperties(service, &mut dictionary, kCFAllocatorDefault, 0)
        };
        unsafe { IOObjectRelease(service) };
        if status != KERN_SUCCESS as i32 || dictionary.is_null() {
            if !dictionary.is_null() {
                unsafe { core_foundation::base::CFRelease(dictionary.cast()) };
            }
            continue;
        }
        let properties: CFDictionary<CFString, CFType> =
            unsafe { CFMutableDictionary::wrap_under_create_rule(dictionary).to_immutable() };

        let Some(vid) = properties
            .find(&vid_key)
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|value| value.to_i32())
        else {
            continue;
        };
        let Some(pid) = properties
            .find(&pid_key)
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|value| value.to_i32())
        else {
            continue;
        };
        let Some(location) = properties
            .find(&location_key)
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|value| value.to_i32())
        else {
            continue;
        };

        if vid != VID_GOXLR as i32 || (pid != PID_GOXLR_FULL as i32 && pid != PID_GOXLR_MINI as i32)
        {
            continue;
        }

        let mut matching_uids = audio_uids
            .iter()
            .filter(|uid| uid_matches_location(uid, location as u32));
        if let Some(uid) = matching_uids.next() {
            if matching_uids.next().is_some() {
                unsafe { IOObjectRelease(iterator) };
                bail!("Multiple CoreAudio devices match one GoXLR USB location");
            }
            devices.push(CoreAudioDevice {
                display_name: if pid == PID_GOXLR_FULL as i32 {
                    "GoXLR".into()
                } else {
                    "GoXLR Mini".into()
                },
                uid: uid.clone(),
            });
        }
    }
    unsafe { IOObjectRelease(iterator) };

    Ok(devices)
}
