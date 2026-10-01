use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use std::{mem, ptr};

use anyhow::{Result, bail};
use core_foundation::base::TCFType;
use core_foundation::bundle::CFBundle;
use core_foundation::string::{CFString, CFStringRef};
use core_foundation::url::CFURL;
use coreaudio_sys::{
    AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectPropertyScope,
    AudioObjectPropertySelector, AudioObjectRemovePropertyListener, OSStatus,
    kAudioHardwareNoError, kAudioHardwarePropertyProcessObjectList,
    kAudioObjectPropertyElementMaster, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyDevices, kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID,
};
use goxlr_ipc::{MacosAppAudio, MacosAudioApp};
use log::warn;
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSRunningApplication;
use tokio::sync::Notify;

use crate::platform::macos::audio_bridge::CAPTURE_COUNT;
use crate::platform::macos::core_audio::{get_uid_for_id, get_virtual_audio_app_levels};
use crate::settings::{MacosAppRule, SettingsHandle, apply_app_rule};

/// The visible GoXLR playback devices, in playback route order.
pub const PLAYBACK_NAMES: [&str; 5] = ["System", "Game", "Chat", "Music", "Sample"];

/// The most rules the plug-in holds (`kMaxRules` in the driver's `AppRouting.hpp`).
pub const MAX_APP_RULES: usize = 128;

/// How often the bridge reads the plug-in's levels while someone is asking for them.
pub(crate) const LEVEL_INTERVAL: Duration = Duration::from_millis(50);
// The bridge keeps reading levels this long after the last request.
const LEVELS_WANTED_FOR: Duration = Duration::from_secs(2);
// A cached peak halves every this often, so a client polling slower than the bridge reads
// still sees short peaks.
const LEVEL_HALF_LIFE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioApp {
    pub bundle_id: String,
    pub name: String,
    pub pids: Vec<i32>,
    pub playing: bool,
    /// Playing now or within the last RECENT_PLAYBACK, see `mark_recent`.
    pub recent: bool,
    /// The GoXLR playback route the app itself plays to, if any.
    pub device_route: Option<usize>,
}

/// What the bridge thread last saw, for the tray menu.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppAudioSnapshot {
    pub apps: Vec<AudioApp>,
    pub rules: HashMap<String, MacosAppRule>,
    pub names: HashMap<String, String>,
    pub hidden: Vec<String>,
    pub routes: u32,
    pub mixers: Vec<String>,
}

/// Per-app mixers that tap apps and play their audio again from their own process. While one
/// controls an app, coreaudiod sees the mixer rather than the app on our devices.
const TAPPING_MIXERS: [(&str, &str); 4] = [
    ("com.finetuneapp.FineTune", "FineTune"),
    ("dev.pantafive.fader", "fader"),
    ("com.rogueamoeba.soundsource", "SoundSource"),
    ("com.bearisdriving.BGM.App", "Background Music"),
];

/// Names of the tapping mixers that are running right now.
pub(crate) fn running_mixers() -> Vec<String> {
    autoreleasepool(|_| {
        TAPPING_MIXERS
            .iter()
            .filter(|(bundle_id, _)| {
                let id = objc2_foundation::NSString::from_str(bundle_id);
                NSRunningApplication::runningApplicationsWithBundleIdentifier(&id).count() > 0
            })
            .map(|(_, name)| (*name).to_owned())
            .collect()
    })
}

pub(crate) enum BridgeSignal {
    Stop,
    // Re-apply the per-app rules now, rather than on the next tick.
    Refresh,
    // coreaudiod's process list changed, so an app may have started or stopped using audio.
    AppsChanged,
    // A client asked for levels after none did for a while, start reading them.
    Levels,
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

/// The latest peak of each app, decaying from when it was read. `gxlv` resets on every read,
/// so only the bridge reads it and every client is answered from here.
#[derive(Debug, Default)]
struct LevelCache {
    requested: Option<Instant>,
    peaks: HashMap<String, (f32, Instant)>,
}

fn decayed(peak: f32, since: Instant, now: Instant) -> f32 {
    let elapsed = now.saturating_duration_since(since).as_secs_f32();
    peak * 0.5f32.powf(elapsed / LEVEL_HALF_LIFE.as_secs_f32())
}

impl LevelCache {
    fn wanted(&self, now: Instant) -> bool {
        self.requested
            .is_some_and(|at| now.saturating_duration_since(at) < LEVELS_WANTED_FOR)
    }

    // Marks the levels as wanted and returns whether they weren't already.
    fn request(&mut self, now: Instant) -> bool {
        let started = !self.wanted(now);
        if started {
            self.peaks.clear();
        }
        self.requested = Some(now);
        started
    }

    fn record(&mut self, levels: HashMap<String, f32>, now: Instant) {
        self.peaks
            .retain(|_, (peak, at)| decayed(*peak, *at, now) > 0.001);
        for (bundle_id, level) in levels {
            let entry = self.peaks.entry(bundle_id).or_insert((0.0, now));
            if level >= decayed(entry.0, entry.1, now) {
                *entry = (level, now);
            }
        }
    }

    fn levels(&self, now: Instant) -> HashMap<String, f32> {
        self.peaks
            .iter()
            .map(|(bundle_id, (peak, at))| (bundle_id.clone(), decayed(*peak, *at, now)))
            .filter(|(_, level)| *level > 0.001)
            .collect()
    }
}

#[derive(Clone, Default)]
pub struct AppAudioHandle {
    snapshot: Arc<Mutex<AppAudioSnapshot>>,
    bridge: Arc<Mutex<Option<Sender<BridgeSignal>>>>,
    // Wakes the primary worker to refresh the daemon status when the snapshot changes.
    changed: Arc<Notify>,
    levels: Arc<Mutex<LevelCache>>,
}

impl AppAudioHandle {
    pub fn snapshot(&self) -> AppAudioSnapshot {
        self.snapshot.lock().unwrap().clone()
    }

    /// Resolves once the snapshot has changed since the last call.
    pub async fn changed(&self) {
        self.changed.notified().await;
    }

    // The bridge publishes every tick, only wake the primary worker when something differs.
    pub(crate) fn publish(&self, snapshot: AppAudioSnapshot) {
        let mut current = self.snapshot.lock().unwrap();
        if *current != snapshot {
            *current = snapshot;
            self.changed.notify_one();
        }
    }

    pub(crate) fn set_bridge(&self, sender: Option<Sender<BridgeSignal>>) {
        *self.bridge.lock().unwrap() = sender;
    }

    /// Mirrors a rule that was just stored in the settings and wakes the bridge to apply it.
    pub fn rule_changed(&self, bundle_id: String, rule: MacosAppRule) {
        apply_app_rule(&mut self.snapshot.lock().unwrap().rules, bundle_id, rule);
        self.changed.notify_one();
        if let Some(bridge) = self.bridge.lock().unwrap().as_ref() {
            let _ = bridge.send(BridgeSignal::Refresh);
        }
    }

    /// Mirrors the hidden apps that were just stored in the settings.
    pub fn hidden_changed(&self, hidden: Vec<String>) {
        self.snapshot.lock().unwrap().hidden = hidden;
        self.changed.notify_one();
    }

    /// The recent peak level of each app, from 0 to 1, without asking coreaudiod. The bridge
    /// reads the levels while any client asked for them in the last 2 seconds.
    pub fn levels(&self) -> HashMap<String, f32> {
        let now = Instant::now();
        let mut cache = self.levels.lock().unwrap();
        if cache.request(now)
            && let Some(bridge) = self.bridge.lock().unwrap().as_ref()
        {
            let _ = bridge.send(BridgeSignal::Levels);
        }
        cache.levels(now)
    }

    pub(crate) fn levels_wanted(&self) -> bool {
        self.levels.lock().unwrap().wanted(Instant::now())
    }

    /// Reads the plug-in and tap levels into the cache. Called on the bridge thread because
    /// the plug-in read asks coreaudiod and can block.
    pub(crate) fn sample_levels(
        &self,
        apps: &[AudioApp],
        tapped: HashMap<String, f32>,
    ) -> Result<()> {
        let driver_levels = get_virtual_audio_app_levels();
        let mut levels = driver_levels
            .as_deref()
            .map(|value| levels_by_app(apps, value))
            .unwrap_or_default();
        for (id, peak) in tapped {
            let entry = levels.entry(id).or_default();
            *entry = entry.max(peak);
        }
        self.levels.lock().unwrap().record(levels, Instant::now());
        driver_levels.map(|_| ())
    }
}

/// The per-app audio part of the daemon status: the apps the bridge last saw, and the rules,
/// names and hidden apps from the settings (so they show even while the driver is missing).
pub async fn app_audio_status(
    app_audio: &AppAudioHandle,
    settings: &SettingsHandle,
) -> MacosAppAudio {
    let apps = app_audio
        .snapshot()
        .apps
        .into_iter()
        .map(|app| MacosAudioApp {
            bundle_id: app.bundle_id,
            name: app.name,
            playing: app.playing,
            recent: app.recent,
            device_route: app.device_route,
        })
        .collect();
    MacosAppAudio {
        apps,
        rules: settings.get_macos_app_rules().await,
        names: settings.get_macos_app_names().await,
        hidden: settings.get_macos_hidden_apps().await,
        routes: settings.get_macos_virtual_audio_routes().await,
        mixers: app_audio.snapshot().mixers,
    }
}

/// How long an app stays listed after it last played.
const RECENT_PLAYBACK: Duration = Duration::from_secs(10 * 60);

/// Marks apps that are playing or played within RECENT_PLAYBACK, remembering when each app
/// last played in `last_played`.
pub fn mark_recent(apps: &mut [AudioApp], last_played: &mut HashMap<String, Instant>) {
    let now = Instant::now();
    last_played.retain(|_, at| now.duration_since(*at) < RECENT_PLAYBACK);
    for app in apps {
        if app.playing {
            last_played.insert(app.bundle_id.clone(), now);
        }
        app.recent = app.playing || last_played.contains_key(&app.bundle_id);
    }
}

/// Maps the plug-in's `pid:peak;..` levels (per mille) to the apps owning those processes,
/// keeping the loudest process of each app.
pub fn levels_by_app(apps: &[AudioApp], levels: &str) -> HashMap<String, f32> {
    let mut result: HashMap<String, f32> = HashMap::new();
    for entry in levels.split(';') {
        let Some((pid, peak)) = entry.split_once(':') else {
            continue;
        };
        let (Ok(pid), Ok(peak)) = (pid.trim().parse::<i32>(), peak.trim().parse::<u32>()) else {
            continue;
        };
        let Some(app) = apps.iter().find(|app| app.pids.contains(&pid)) else {
            continue;
        };
        let level = peak.min(1000) as f32 / 1000.0;
        let value = result.entry(app.bundle_id.clone()).or_default();
        *value = value.max(level);
    }
    result
}

pub fn playback_route_enabled(routes: u32, route: usize) -> bool {
    routes & (1 << (CAPTURE_COUNT + route)) != 0
}

fn playback_route_for_uid(uid: &str) -> Option<usize> {
    let name = uid.strip_prefix("GoXLRVirtual::")?;
    PLAYBACK_NAMES.iter().position(|route| *route == name)
}

/// Builds the plug-in's rule table for every process of every app that has a rule, keeping the
/// lowest `MAX_APP_RULES` PIDs.
#[cfg(test)]
pub fn rules_string(apps: &[AudioApp], rules: &HashMap<String, MacosAppRule>) -> String {
    rules_string_with_taps(apps, rules, &HashSet::new(), &HashSet::new())
}

/// A tapped process is rendered by the bridge, so the plug-in must not redirect it a second
/// time. On tap failure, preserve its native route and apply only the volume rule.
pub(crate) fn rules_string_with_taps(
    apps: &[AudioApp],
    rules: &HashMap<String, MacosAppRule>,
    tapped: &HashSet<i32>,
    failed: &HashSet<i32>,
) -> String {
    static WARNED: AtomicBool = AtomicBool::new(false);

    let mut entries = Vec::new();
    for app in apps {
        let Some(rule) = rules.get(&app.bundle_id) else {
            continue;
        };
        let route = match rule.route {
            Some(route) if route < PLAYBACK_NAMES.len() => route.to_string(),
            _ => "-".into(),
        };
        // Per mille, the plug-in soft limits anything above 1000.
        let gain = if rule.muted {
            0
        } else {
            u32::from(rule.volume.min(200)) * 10
        };
        entries.extend(
            app.pids
                .iter()
                .filter(|pid| !tapped.contains(pid))
                .map(|pid| {
                    let route = if failed.contains(pid) { "-" } else { &route };
                    (*pid, format!("{pid}:{route}:{gain}"))
                }),
        );
    }
    entries.sort();
    if entries.len() > MAX_APP_RULES && !WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            "{} app processes have audio rules, only the first {MAX_APP_RULES} are applied",
            entries.len()
        );
    }
    entries.truncate(MAX_APP_RULES);
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

const OWN_BUNDLE_ID: &str = "com.github.goxlr-on-linux.goxlr-utility";

fn process_path(pid: i32) -> Option<String> {
    let mut buffer = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr() as *mut c_void, buffer.len() as u32) };
    (length > 0).then(|| String::from_utf8_lossy(&buffer[..length as usize]).into_owned())
}

/// The outermost `.app` bundle in a path, e.g. `Google Chrome.app` for its helpers.
fn outer_app_path(path: &str) -> Option<&str> {
    path.find(".app/").map(|end| &path[..end + ".app".len()])
}

/// Reads the bundle ID and display name of an app bundle on disk.
fn app_bundle_info(path: &str) -> Option<(String, String)> {
    let url = CFURL::from_path(path, true)?;
    let bundle = CFBundle::new(url)?;
    let info = bundle.info_dictionary();
    let string = |key: &str| {
        info.find(CFString::new(key))
            .and_then(|value| value.downcast::<CFString>())
            .map(|value| value.to_string())
    };
    let bundle_id = string("CFBundleIdentifier")?;
    let name = string("CFBundleDisplayName")
        .or_else(|| string("CFBundleName"))
        .unwrap_or_else(|| bundle_id.clone());
    Some((bundle_id, name))
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn LSCopyApplicationURLsForBundleIdentifier(
        bundle_id: CFStringRef,
        error: *mut *const c_void,
    ) -> core_foundation::array::CFArrayRef;
}

/// Looks up the display name of an installed app that isn't running, e.g. one with a rule saved
/// before its name was recorded. Each bundle ID is only looked up once per daemon run.
pub(crate) fn installed_app_name(bundle_id: &str) -> Option<String> {
    static LOOKED_UP: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    let cache = LOOKED_UP.get_or_init(Default::default);
    if let Some(name) = cache.lock().unwrap().get(bundle_id) {
        return name.clone();
    }

    let id = CFString::new(bundle_id);
    let urls = unsafe {
        LSCopyApplicationURLsForBundleIdentifier(id.as_concrete_TypeRef(), ptr::null_mut())
    };
    let name = (!urls.is_null())
        .then(|| unsafe { core_foundation::array::CFArray::<CFURL>::wrap_under_create_rule(urls) })
        .and_then(|urls| urls.get(0).and_then(|url| url.to_path()))
        .and_then(|path| app_bundle_info(&path.to_string_lossy()))
        .map(|(_, name)| name);
    cache
        .lock()
        .unwrap()
        .insert(bundle_id.to_owned(), name.clone());
    name
}

/// Returns the bundle ID and name of the app a process belongs to.
fn identify(object: AudioObjectID, pid: i32) -> Option<(String, String)> {
    let owner = responsible_pid(pid);

    // Helpers that macOS doesn't attribute to their app (e.g. when the app wasn't started
    // through Launch Services) still live inside it, so use the outermost app bundle.
    if let Some(path) = process_path(owner)
        && let Some(app_path) = outer_app_path(&path)
        && !path[app_path.len()..]
            .trim_start_matches('/')
            .starts_with("Contents/MacOS/")
        && let Some(info) = app_bundle_info(app_path)
    {
        return Some(info);
    }

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
            // The Utility's own UI (and its WebKit helpers) isn't something to route.
            if bundle_id == OWN_BUNDLE_ID || name.starts_with("goxlr-utility-ui") {
                continue;
            }
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
                    recent: playing,
                    device_route,
                });
            }
        }
    });
    for app in &mut apps {
        app.pids.sort_unstable();
        app.pids.dedup();
    }
    apps.sort_by_cached_key(|app| app.name.to_lowercase());
    Ok(apps)
}

#[cfg(test)]
mod tests {
    use super::{
        AudioApp, LevelCache, MAX_APP_RULES, RECENT_PLAYBACK, app_bundle_info, levels_by_app,
        list_audio_apps, mark_recent, outer_app_path, playback_route_for_uid, rules_string,
        rules_string_with_taps,
    };
    use crate::settings::MacosAppRule;
    use goxlr_ipc::{MacosAppAudio, MacosAudioApp};
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    #[test]
    fn looks_up_installed_app_names() {
        // Finder is always installed; an unknown bundle has no app.
        assert_eq!(
            super::installed_app_name("com.apple.finder").as_deref(),
            Some("Finder")
        );
        assert_eq!(
            super::installed_app_name("invalid.goxlr.not-installed"),
            None
        );
    }

    #[test]
    fn helpers_belong_to_their_outer_app() {
        let helper = "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome \
                      Framework.framework/Helpers/Google Chrome Helper.app/Contents/MacOS/\
                      Google Chrome Helper";
        assert_eq!(
            outer_app_path(helper),
            Some("/Applications/Google Chrome.app")
        );
        assert_eq!(outer_app_path("/usr/bin/afplay"), None);
        // The Utility itself is a real bundle on this machine, check it reads.
        if let Some((bundle_id, _)) = app_bundle_info("/Applications/GoXLR Utility.app") {
            assert_eq!(bundle_id, "com.github.goxlr-on-linux.goxlr-utility");
        }
    }

    fn rule(route: Option<usize>, volume: u16, muted: bool) -> MacosAppRule {
        MacosAppRule {
            route,
            volume,
            muted,
        }
    }

    #[test]
    fn keeps_recently_playing_apps_listed() {
        let mut last_played = HashMap::new();
        let mut apps = vec![
            app("com.spotify.client", vec![1]),
            app("com.example.idle", vec![2]),
        ];
        apps[0].playing = true;
        apps[1].playing = false;
        mark_recent(&mut apps, &mut last_played);
        assert!(apps[0].recent && !apps[1].recent);

        // Spotify pauses; it stays listed because it played a moment ago.
        apps[0].playing = false;
        mark_recent(&mut apps, &mut last_played);
        assert!(apps[0].recent && !apps[1].recent);

        // Once RECENT_PLAYBACK has passed, it drops out like the idle app.
        last_played.insert(
            "com.spotify.client".into(),
            Instant::now() - RECENT_PLAYBACK,
        );
        mark_recent(&mut apps, &mut last_played);
        assert!(!apps[0].recent);
    }

    fn app(bundle_id: &str, pids: Vec<i32>) -> AudioApp {
        AudioApp {
            bundle_id: bundle_id.into(),
            name: bundle_id.into(),
            pids,
            playing: true,
            recent: false,
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
            ("com.apple.Safari".to_string(), rule(Some(1), 100, false)),
            ("com.apple.Music".to_string(), rule(None, 35, false)),
            (
                "com.example.NotRunning".to_string(),
                rule(Some(2), 0, false),
            ),
        ]);
        assert_eq!(rules_string(&apps, &rules), "40:1:1000;90:-:350;512:1:1000");
        assert_eq!(rules_string(&apps, &HashMap::new()), "");
    }

    #[test]
    fn maps_volume_and_mute_to_gain() {
        let apps = [
            app("com.spotify.client", vec![77]),
            app("com.apple.Music", vec![90]),
            app("com.apple.Safari", vec![12]),
        ];
        let rules = HashMap::from([
            // Muted wins over the volume, and keeps the route.
            ("com.spotify.client".to_string(), rule(Some(3), 150, true)),
            ("com.apple.Music".to_string(), rule(None, 200, false)),
            // Out of range volumes are capped at 200 %.
            ("com.apple.Safari".to_string(), rule(None, 900, false)),
        ]);
        assert_eq!(rules_string(&apps, &rules), "12:-:2000;77:3:0;90:-:2000");
    }

    #[test]
    fn tapped_apps_are_not_redirected_twice_and_failed_taps_keep_native_output() {
        let apps = [
            app("com.google.Chrome", vec![12]),
            app("com.example.Player", vec![77]),
            app("com.example.Other", vec![90]),
        ];
        let rules = HashMap::from([
            ("com.google.Chrome".into(), rule(Some(3), 50, false)),
            ("com.example.Player".into(), rule(Some(3), 45, false)),
            ("com.example.Other".into(), rule(Some(3), 80, false)),
        ]);
        assert_eq!(
            rules_string_with_taps(&apps, &rules, &HashSet::from([12]), &HashSet::from([77])),
            "77:-:450;90:3:800"
        );
    }

    #[test]
    fn caps_rules_at_what_the_plug_in_holds() {
        let apps = [app(
            "com.google.Chrome",
            (1..=MAX_APP_RULES as i32 + 5).collect(),
        )];
        let rules = HashMap::from([("com.google.Chrome".to_string(), rule(Some(1), 50, false))]);
        let value = rules_string(&apps, &rules);
        let entries: Vec<_> = value.split(';').collect();
        assert_eq!(entries.len(), MAX_APP_RULES);
        assert_eq!(
            entries.last(),
            Some(&format!("{MAX_APP_RULES}:1:500").as_str())
        );
    }

    #[test]
    fn caches_decaying_levels_while_wanted() {
        let start = Instant::now();
        let at = |millis| start + Duration::from_millis(millis);
        let mut cache = LevelCache::default();
        assert!(!cache.wanted(start));
        assert!(cache.request(start));
        assert!(!cache.request(at(1500)));
        assert!(cache.wanted(at(3000)) && !cache.wanted(at(3600)));

        cache.record(HashMap::from([("a".to_string(), 0.8)]), at(1500));
        // Every client reading sees the same peak, halving every 100 ms.
        assert_eq!(cache.levels(at(1500))["a"], 0.8);
        assert_eq!(cache.levels(at(1500))["a"], 0.8);
        assert!((cache.levels(at(1600))["a"] - 0.4).abs() < 1e-4);
        // A quieter read doesn't replace a louder peak that hasn't decayed below it.
        cache.record(HashMap::from([("a".to_string(), 0.3)]), at(1550));
        assert!(cache.levels(at(1550))["a"] > 0.5);
        cache.record(HashMap::from([("a".to_string(), 0.3)]), at(1700));
        assert_eq!(cache.levels(at(1700))["a"], 0.3);
        // Silent apps fall out, and a new request after an idle spell starts empty.
        assert!(cache.levels(at(3000)).is_empty());
        cache.record(HashMap::from([("b".to_string(), 1.0)]), at(3000));
        assert!(cache.request(at(5000)));
        assert!(cache.levels(at(5000)).is_empty());
    }

    #[test]
    fn maps_levels_to_apps() {
        let apps = [
            app("com.apple.Safari", vec![512, 40]),
            app("com.spotify.client", vec![77]),
        ];
        let levels = levels_by_app(&apps, "40:250;512:600;77:1500;999:800;garbage;13:x");
        assert_eq!(levels.len(), 2);
        // The loudest process of an app wins, peaks are clamped to 1.
        assert_eq!(levels["com.apple.Safari"], 0.6);
        assert_eq!(levels["com.spotify.client"], 1.0);
        assert!(levels_by_app(&apps, "").is_empty());
    }

    #[test]
    fn serialises_status_like_the_web_ui_expects() {
        let status = MacosAppAudio {
            apps: vec![MacosAudioApp {
                bundle_id: "com.spotify.client".into(),
                name: "Spotify".into(),
                playing: true,
                recent: true,
                device_route: Some(0),
            }],
            rules: HashMap::from([("com.spotify.client".to_string(), rule(Some(3), 100, false))]),
            names: HashMap::from([("com.spotify.client".to_string(), "Spotify".to_string())]),
            hidden: vec!["com.apple.siri".into()],
            routes: 61442,
            mixers: vec!["FineTune".into()],
        };
        let value = serde_json::to_value(&status).unwrap();
        println!("{value}");
        assert_eq!(
            value,
            serde_json::json!({
                "apps": [{"bundle_id": "com.spotify.client", "name": "Spotify", "playing": true,
                          "recent": true,
                          "device_route": 0}],
                "rules": {"com.spotify.client": {"route": 3, "volume": 100, "muted": false}},
                "names": {"com.spotify.client": "Spotify"},
                "hidden": ["com.apple.siri"],
                "routes": 61442,
                "mixers": ["FineTune"]
            })
        );
    }

    #[test]
    fn detects_running_mixers_by_bundle_id() {
        // None of the known mixers has to be running, but the lookup must not fail or report
        // apps that aren't in the list.
        for name in super::running_mixers() {
            assert!(
                super::TAPPING_MIXERS
                    .iter()
                    .any(|(_, known)| *known == name)
            );
        }
    }

    #[test]
    #[ignore = "prints the apps currently known to coreaudiod"]
    fn lists_current_audio_apps() {
        for app in list_audio_apps().unwrap() {
            println!("{app:?}");
        }
    }
}
