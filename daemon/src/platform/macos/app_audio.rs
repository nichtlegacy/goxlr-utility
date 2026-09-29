use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::{mem, ptr};

use anyhow::{Result, bail};
use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use coreaudio_sys::{
    AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectPropertyScope,
    AudioObjectPropertySelector, AudioObjectRemovePropertyListener, OSStatus,
    kAudioHardwareNoError, kAudioHardwarePropertyProcessObjectList,
    kAudioObjectPropertyElementMaster, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyDevices, kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID,
};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSRunningApplication;

use crate::platform::macos::audio_bridge::CAPTURE_COUNT;
use crate::platform::macos::core_audio::get_uid_for_id;
use crate::settings::{MacosAppRule, apply_app_rule};

/// The visible GoXLR playback devices, in playback route order.
pub const PLAYBACK_NAMES: [&str; 5] = ["System", "Game", "Chat", "Music", "Sample"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioApp {
    pub bundle_id: String,
    pub name: String,
    pub pids: Vec<i32>,
    pub playing: bool,
    /// The GoXLR playback route the app itself plays to, if any.
    pub device_route: Option<usize>,
}

/// What the bridge thread last saw, for the tray menu.
#[derive(Debug, Clone, Default)]
pub struct AppAudioSnapshot {
    pub apps: Vec<AudioApp>,
    pub rules: HashMap<String, MacosAppRule>,
    pub routes: u32,
}

pub(crate) enum BridgeSignal {
    Stop,
    // Re-apply the per-app rules now, rather than on the next tick.
    Refresh,
    // coreaudiod's process list changed, so an app may have started or stopped using audio.
    AppsChanged,
}

const PROCESS_LIST: AudioObjectPropertyAddress = AudioObjectPropertyAddress {
    mSelector: kAudioHardwarePropertyProcessObjectList,
    mScope: kAudioObjectPropertyScopeGlobal,
    mElement: kAudioObjectPropertyElementMaster,
};

/// Wakes the bridge whenever coreaudiod's process list changes. An app joins that list when it
/// first opens audio, usually before it plays, so its rule can be in place from the first buffer.
pub(crate) struct ProcessWatch {
    sender: *mut Sender<BridgeSignal>,
}

// The sender is only touched by the listener and freed after the listener is removed.
unsafe impl Send for ProcessWatch {}

impl ProcessWatch {
    pub(crate) fn new(sender: Sender<BridgeSignal>) -> Result<Self> {
        let sender = Box::into_raw(Box::new(sender));
        let status = unsafe {
            AudioObjectAddPropertyListener(
                kAudioObjectSystemObject,
                &PROCESS_LIST,
                Some(process_list_changed),
                sender.cast(),
            )
        };
        if status != kAudioHardwareNoError as i32 {
            drop(unsafe { Box::from_raw(sender) });
            bail!("Unable to watch CoreAudio processes: {status}");
        }
        Ok(Self { sender })
    }
}

impl Drop for ProcessWatch {
    fn drop(&mut self) {
        unsafe {
            AudioObjectRemovePropertyListener(
                kAudioObjectSystemObject,
                &PROCESS_LIST,
                Some(process_list_changed),
                self.sender.cast(),
            );
            drop(Box::from_raw(self.sender));
        }
    }
}

extern "C" fn process_list_changed(
    _object: AudioObjectID,
    _count: u32,
    _addresses: *const AudioObjectPropertyAddress,
    data: *mut c_void,
) -> OSStatus {
    let sender = unsafe { &*(data as *const Sender<BridgeSignal>) };
    let _ = sender.send(BridgeSignal::AppsChanged);
    kAudioHardwareNoError as OSStatus
}

#[derive(Clone, Default)]
pub struct AppAudioHandle {
    snapshot: Arc<Mutex<AppAudioSnapshot>>,
    bridge: Arc<Mutex<Option<Sender<BridgeSignal>>>>,
}

impl AppAudioHandle {
    pub fn snapshot(&self) -> AppAudioSnapshot {
        self.snapshot.lock().unwrap().clone()
    }

    pub(crate) fn publish(&self, snapshot: AppAudioSnapshot) {
        *self.snapshot.lock().unwrap() = snapshot;
    }

    pub(crate) fn set_bridge(&self, sender: Option<Sender<BridgeSignal>>) {
        *self.bridge.lock().unwrap() = sender;
    }

    /// Mirrors a rule that was just stored in the settings and wakes the bridge to apply it.
    pub fn rule_changed(&self, bundle_id: String, rule: MacosAppRule) {
        apply_app_rule(&mut self.snapshot.lock().unwrap().rules, bundle_id, rule);
        if let Some(bridge) = self.bridge.lock().unwrap().as_ref() {
            let _ = bridge.send(BridgeSignal::Refresh);
        }
    }
}

pub fn playback_route_enabled(routes: u32, route: usize) -> bool {
    routes & (1 << (CAPTURE_COUNT + route)) != 0
}

fn playback_route_for_uid(uid: &str) -> Option<usize> {
    let name = uid.strip_prefix("GoXLRVirtual::")?;
    PLAYBACK_NAMES.iter().position(|route| *route == name)
}

/// Builds the plug-in's rule table for every process of every app that has a rule.
pub fn rules_string(apps: &[AudioApp], rules: &HashMap<String, MacosAppRule>) -> String {
    let mut entries = Vec::new();
    for app in apps {
        let Some(rule) = rules.get(&app.bundle_id) else {
            continue;
        };
        let route = match rule.route {
            Some(route) if route < PLAYBACK_NAMES.len() => route.to_string(),
            _ => "-".into(),
        };
        let gain = u32::from(rule.volume.min(100)) * 10;
        entries.extend(
            app.pids
                .iter()
                .map(|pid| (*pid, format!("{pid}:{route}:{gain}"))),
        );
    }
    entries.sort();
    entries
        .into_iter()
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>()
        .join(";")
}

fn address(
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMaster,
    }
}

fn read_array(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Result<Vec<AudioObjectID>> {
    let address = address(selector, scope);
    let mut size = 0u32;
    let status =
        unsafe { AudioObjectGetPropertyDataSize(object, &address, 0, ptr::null(), &mut size) };
    if status != kAudioHardwareNoError as i32 {
        bail!("CoreAudio Error: {status}");
    }
    let mut values = vec![0; size as usize / mem::size_of::<AudioObjectID>()];
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut size,
            values.as_mut_ptr() as *mut c_void,
        )
    };
    if status != kAudioHardwareNoError as i32 {
        bail!("CoreAudio Error: {status}");
    }
    values.truncate(size as usize / mem::size_of::<AudioObjectID>());
    Ok(values)
}

fn read_u32(object: AudioObjectID, selector: AudioObjectPropertySelector) -> Option<u32> {
    let address = address(selector, kAudioObjectPropertyScopeGlobal);
    let mut value = 0u32;
    let mut size = mem::size_of::<u32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut size,
            &mut value as *mut u32 as *mut c_void,
        )
    };
    (status == kAudioHardwareNoError as i32).then_some(value)
}

fn read_bundle_id(object: AudioObjectID) -> Option<String> {
    let address = address(
        kAudioProcessPropertyBundleID,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut value: CFStringRef = ptr::null();
    let mut size = mem::size_of::<CFStringRef>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut size,
            &mut value as *mut CFStringRef as *mut c_void,
        )
    };
    if status != kAudioHardwareNoError as i32 || value.is_null() {
        return None;
    }
    let value = unsafe { CFString::wrap_under_create_rule(value) }.to_string();
    (!value.is_empty()).then_some(value)
}

type ResponsibleForPid = unsafe extern "C" fn(libc::pid_t) -> libc::pid_t;

// Helper processes (WebKit, Chrome, Electron) play audio on behalf of their app. The call
// is private API, so look it up at runtime and treat each process as its own app without it.
fn responsible_pid(pid: i32) -> i32 {
    static FUNCTION: OnceLock<Option<ResponsibleForPid>> = OnceLock::new();
    let function = FUNCTION.get_or_init(|| {
        let symbol = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                c"responsibility_get_pid_responsible_for_pid".as_ptr(),
            )
        };
        (!symbol.is_null())
            .then(|| unsafe { mem::transmute::<*mut c_void, ResponsibleForPid>(symbol) })
    });
    match function.map(|function| unsafe { function(pid) }) {
        Some(owner) if owner > 0 => owner,
        _ => pid,
    }
}

fn process_name(pid: i32) -> Option<String> {
    let mut buffer = [0u8; 256];
    let length =
        unsafe { libc::proc_name(pid, buffer.as_mut_ptr() as *mut c_void, buffer.len() as u32) };
    (length > 0).then(|| String::from_utf8_lossy(&buffer[..length as usize]).into_owned())
}

fn is_system_process(pid: i32) -> bool {
    let mut buffer = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr() as *mut c_void, buffer.len() as u32) };
    length <= 0
        || ["/System/", "/usr/", "/Library/Apple/"]
            .iter()
            .any(|prefix| buffer[..length as usize].starts_with(prefix.as_bytes()))
}

/// Returns the bundle ID and name of the app a process belongs to.
fn identify(object: AudioObjectID, pid: i32) -> Option<(String, String)> {
    let owner = responsible_pid(pid);
    for candidate in [owner, pid] {
        if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(candidate)
            && let Some(bundle_id) = app.bundleIdentifier()
        {
            let bundle_id = bundle_id.to_string();
            let name = app
                .localizedName()
                .map_or_else(|| bundle_id.clone(), |name| name.to_string());
            return Some((bundle_id, name));
        }
    }

    // No app, so this is a daemon or command line tool. Skip the system's own (coreaudiod,
    // systemsoundserverd, ..) and anything without a bundle ID.
    if is_system_process(owner) {
        return None;
    }
    let bundle_id = read_bundle_id(object)?;
    let name = process_name(owner).unwrap_or_else(|| bundle_id.clone());
    Some((bundle_id, name))
}

/// Lists the apps with an audio client in coreaudiod, grouped by bundle ID.
pub fn list_audio_apps() -> Result<Vec<AudioApp>> {
    let own_pid = std::process::id() as i32;
    let processes = read_array(
        kAudioObjectSystemObject,
        kAudioHardwarePropertyProcessObjectList,
        kAudioObjectPropertyScopeGlobal,
    )?;
    let mut device_routes: HashMap<AudioObjectID, Option<usize>> = HashMap::new();
    let mut apps: Vec<AudioApp> = Vec::new();

    autoreleasepool(|_| {
        for object in processes {
            let Some(pid) = read_u32(object, kAudioProcessPropertyPID).map(|pid| pid as i32) else {
                continue;
            };
            if pid == own_pid {
                continue;
            }
            let Some((bundle_id, name)) = identify(object, pid) else {
                continue;
            };
            let playing =
                read_u32(object, kAudioProcessPropertyIsRunningOutput).is_some_and(|v| v != 0);
            let device_route = read_array(
                object,
                kAudioProcessPropertyDevices,
                kAudioObjectPropertyScopeOutput,
            )
            .unwrap_or_default()
            .into_iter()
            .find_map(|device| {
                *device_routes.entry(device).or_insert_with(|| {
                    get_uid_for_id(device)
                        .ok()
                        .and_then(|uid| playback_route_for_uid(&uid))
                })
            });

            if let Some(app) = apps.iter_mut().find(|app| app.bundle_id == bundle_id) {
                app.pids.push(pid);
                // Prefer the output of a process that's actually playing.
                if device_route.is_some() && (playing || app.device_route.is_none()) {
                    app.device_route = device_route;
                }
                app.playing |= playing;
            } else {
                apps.push(AudioApp {
                    bundle_id,
                    name,
                    pids: vec![pid],
                    playing,
                    device_route,
                });
            }
        }
    });
    apps.sort_by_cached_key(|app| app.name.to_lowercase());
    Ok(apps)
}

#[cfg(test)]
mod tests {
    use super::{AudioApp, list_audio_apps, playback_route_for_uid, rules_string};
    use crate::settings::MacosAppRule;
    use std::collections::HashMap;

    fn app(bundle_id: &str, pids: Vec<i32>) -> AudioApp {
        AudioApp {
            bundle_id: bundle_id.into(),
            name: bundle_id.into(),
            pids,
            playing: true,
            device_route: Some(0),
        }
    }

    #[test]
    fn maps_visible_playback_uids_to_routes() {
        assert_eq!(playback_route_for_uid("GoXLRVirtual::System"), Some(0));
        assert_eq!(playback_route_for_uid("GoXLRVirtual::Music"), Some(3));
        assert_eq!(playback_route_for_uid("GoXLRVirtual::Sample"), Some(4));
        assert_eq!(playback_route_for_uid("GoXLRVirtual::System::Bridge"), None);
        assert_eq!(playback_route_for_uid("GoXLRVirtual::GameCapture"), None);
        assert_eq!(playback_route_for_uid("BuiltInSpeakerDevice"), None);
    }

    #[test]
    fn builds_rules_for_every_pid_of_ruled_apps() {
        let apps = [
            app("com.apple.Safari", vec![512, 40]),
            app("com.spotify.client", vec![77]),
            app("com.apple.Music", vec![90]),
        ];
        let rules = HashMap::from([
            (
                "com.apple.Safari".to_string(),
                MacosAppRule {
                    route: Some(1),
                    volume: 100,
                },
            ),
            (
                "com.apple.Music".to_string(),
                MacosAppRule {
                    route: None,
                    volume: 35,
                },
            ),
            (
                "com.example.NotRunning".to_string(),
                MacosAppRule {
                    route: Some(2),
                    volume: 0,
                },
            ),
        ]);
        assert_eq!(rules_string(&apps, &rules), "40:1:1000;90:-:350;512:1:1000");
        assert_eq!(rules_string(&apps, &HashMap::new()), "");
    }

    #[test]
    #[ignore = "prints the apps currently known to coreaudiod"]
    fn lists_current_audio_apps() {
        for app in list_audio_apps().unwrap() {
            println!("{app:?}");
        }
    }
}
