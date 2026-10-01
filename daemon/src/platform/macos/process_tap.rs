use std::collections::{HashMap, HashSet};
use std::ffi::{CString, c_char, c_void};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use log::{info, warn};

use super::app_audio::{AudioApp, PLAYBACK_NAMES, playback_route_enabled};
use crate::settings::MacosAppRule;

unsafe extern "C" {
    fn goxlr_process_tap_create(
        pids: *const i32,
        count: usize,
        output_uid: *const c_char,
        gain: f32,
        error: *mut i32,
    ) -> *mut c_void;
    fn goxlr_process_tap_gain(tap: *mut c_void, gain: f32);
    fn goxlr_process_tap_take_peak(tap: *mut c_void) -> f32;
    fn goxlr_process_tap_alive(tap: *mut c_void) -> bool;
    fn goxlr_process_tap_destroy(tap: *mut c_void);
}

struct Tap {
    pointer: NonNull<c_void>,
    bundle_id: String,
    route: usize,
}

impl Tap {
    fn alive(&self) -> bool {
        unsafe { goxlr_process_tap_alive(self.pointer.as_ptr()) }
    }

    fn set_gain(&self, gain: f32) {
        unsafe { goxlr_process_tap_gain(self.pointer.as_ptr(), gain) }
    }

    fn take_peak(&self) -> f32 {
        unsafe { goxlr_process_tap_take_peak(self.pointer.as_ptr()) }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        unsafe { goxlr_process_tap_destroy(self.pointer.as_ptr()) }
    }
}

struct Failed {
    bundle_id: String,
    route: usize,
    retry_at: Instant,
}

#[derive(Default)]
pub(super) struct TapManager {
    active: HashMap<i32, Tap>,
    failed: HashMap<i32, Failed>,
}

pub(super) struct TapPids {
    pub active: HashSet<i32>,
    pub failed: HashSet<i32>,
}

impl TapManager {
    pub fn take_levels(&self) -> HashMap<String, f32> {
        let mut levels = HashMap::<String, f32>::new();
        for tap in self.active.values() {
            let peak = tap.take_peak().min(1.0);
            if peak > 0.0 {
                let entry = levels.entry(tap.bundle_id.clone()).or_default();
                *entry = entry.max(peak);
            }
        }
        levels
    }

    pub fn clear(&mut self) {
        self.active.clear();
        self.failed.clear();
    }

    /// Activate only routes whose app currently outputs somewhere else. The native output
    /// stays available if CoreAudio refuses a tap; the driver then applies gain only.
    pub fn sync(
        &mut self,
        apps: &[AudioApp],
        rules: &HashMap<String, MacosAppRule>,
        routes: u32,
        bridge_running: bool,
    ) -> TapPids {
        if !bridge_running {
            self.clear();
            let failed = apps
                .iter()
                .filter(|app| {
                    rules.get(&app.bundle_id).is_some_and(|rule| {
                        rule.route.is_some_and(|route| {
                            route < PLAYBACK_NAMES.len() && app.device_route != Some(route)
                        })
                    })
                })
                .flat_map(|app| app.pids.iter().copied())
                .collect();
            return TapPids {
                active: HashSet::new(),
                failed,
            };
        }
        // A separate tap per audio process keeps existing streams running when an app
        // starts or stops an unrelated helper process.
        let desired: HashMap<_, _> = apps
            .iter()
            .filter_map(|app| {
                let rule = rules.get(&app.bundle_id)?;
                let route = rule.route?;
                (route < PLAYBACK_NAMES.len()
                    && playback_route_enabled(routes, route)
                    && app.device_route != Some(route))
                .then_some((app, rule, route))
            })
            .flat_map(|(app, rule, route)| {
                app.pids
                    .iter()
                    .copied()
                    .map(move |pid| (pid, (app, rule, route)))
            })
            .collect();
        self.active.retain(|pid, tap| {
            desired.get(pid).is_some_and(|(app, _, route)| {
                tap.route == *route && tap.bundle_id == app.bundle_id && tap.alive()
            })
        });
        self.failed.retain(|pid, failure| {
            desired.get(pid).is_some_and(|(app, _, route)| {
                failure.route == *route && failure.bundle_id == app.bundle_id
            })
        });

        let mut active = HashSet::new();
        let mut failed = HashSet::new();
        for (pid, (app, rule, route)) in desired {
            let gain = if rule.muted {
                0.0
            } else {
                f32::from(rule.volume.min(200)) / 100.0
            };
            if let Some(tap) = self.active.get(&pid) {
                tap.set_gain(gain);
                active.insert(pid);
                continue;
            }
            if self
                .failed
                .get(&pid)
                .is_some_and(|failure| Instant::now() < failure.retry_at)
            {
                failed.insert(pid);
                continue;
            }
            let uid = CString::new(format!("GoXLRVirtual::{}", PLAYBACK_NAMES[route])).unwrap();
            let mut error = 0;
            let pointer =
                unsafe { goxlr_process_tap_create(&pid, 1, uid.as_ptr(), gain, &mut error) };
            if let Some(pointer) = NonNull::new(pointer) {
                info!(
                    "App audio tap active for {} ({pid}) on {}",
                    app.name, PLAYBACK_NAMES[route]
                );
                self.failed.remove(&pid);
                self.active.insert(
                    pid,
                    Tap {
                        pointer,
                        bundle_id: app.bundle_id.clone(),
                        route,
                    },
                );
                active.insert(pid);
            } else {
                if !self.failed.contains_key(&pid) {
                    warn!(
                        "App audio tap for {} ({pid}) failed (CoreAudio {error}); check System Audio Recording permission",
                        app.name
                    );
                }
                self.failed.insert(
                    pid,
                    Failed {
                        bundle_id: app.bundle_id.clone(),
                        route,
                        retry_at: Instant::now() + Duration::from_secs(30),
                    },
                );
                failed.insert(pid);
            }
        }
        TapPids { active, failed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_outage_preserves_app_output() {
        let app = AudioApp {
            bundle_id: "com.google.Chrome".into(),
            name: "Chrome".into(),
            pids: vec![42],
            playing: true,
            recent: true,
            device_route: Some(2),
        };
        let rules = HashMap::from([(
            app.bundle_id.clone(),
            MacosAppRule {
                route: Some(3),
                volume: 50,
                muted: false,
            },
        )]);
        let pids = TapManager::default().sync(&[app], &rules, u32::MAX, false);
        assert!(pids.active.is_empty());
        assert_eq!(pids.failed, HashSet::from([42]));
    }
}
