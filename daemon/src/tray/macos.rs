#![allow(deprecated)]

use dispatch2::Queue;
use enum_map::{Enum, EnumMap};
use log::{debug, warn};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class,
    msg_send, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSCellImagePosition, NSControl, NSControlStateValueOn, NSEvent,
    NSEventModifierFlags, NSEventSubtype, NSEventType, NSFont, NSImage, NSMenu, NSMenuDelegate,
    NSMenuItem, NSResponder, NSRunningApplication, NSSlider, NSStatusBar, NSTextField, NSView,
    NSWorkspace,
};
use objc2_foundation::{
    NSAutoreleasePool, NSData, NSDistributedNotificationCenter, NSNotification, NSNotificationName,
    NSPoint, NSRect, NSSize, NSString, NSTimeInterval,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::iter::once;
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;
use strum::{Display, EnumIter, IntoEnumIterator};
use tokio::select;
use tokio::sync::Notify;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep_until};

use goxlr_ipc::{GoXLRCommand, PathTypes};
use goxlr_types::ChannelName;

use crate::ICON_MAC;
use crate::events::EventTriggers::Open;
use crate::events::{DaemonState, EventTriggers};
use crate::platform::macos::app_audio::{AppAudioHandle, PLAYBACK_NAMES, playback_route_enabled};
use crate::primary_worker::DeviceCommand;
use crate::tray::macos::TrayOption::{
    Configure, OpenPathIcons, OpenPathLogs, OpenPathMicProfiles, OpenPathPresets, OpenPathProfiles,
    OpenPathSamples, Quit,
};

// The GoXLR channels behind the virtual playback routes, in route order.
const PLAYBACK_CHANNELS: [ChannelName; 5] = [
    ChannelName::System,
    ChannelName::Game,
    ChannelName::Chat,
    ChannelName::Music,
    ChannelName::Sample,
];

// App route items are tagged `app index * APP_TAG_STRIDE + route + 1`, 0 being the app's own output.
const APP_TAG_STRIDE: usize = 8;

// Per-app volumes run up to 200 %.
const APP_VOLUME_MAX: f64 = 200.;

// A mouse wheel notch moves a slider by this fraction of its range.
const SCROLL_STEP: f64 = 1. / 50.;

// Dragging a slider produces a stream of values, send at most one batch per interval.
const COMMAND_INTERVAL: Duration = Duration::from_millis(30);
const SAVE_DELAY: Duration = Duration::from_millis(300);

#[derive(Clone)]
struct TrayDevice {
    serial: String,
    volumes: EnumMap<ChannelName, u8>,
}

// Only the latest value of each control matters, so the menu overwrites these and the tray
// task applies whatever is there when it gets to it.
#[derive(Default)]
struct PendingChanges {
    volumes: EnumMap<ChannelName, Option<u8>>,
    app_routes: HashMap<String, Option<usize>>,
    app_volumes: HashMap<String, u16>,
    app_mutes: HashMap<String, bool>,
}

// Shared by the AppKit main thread and the tray task. The main thread only ever holds these
// locks briefly, it never waits on the daemon.
struct TrayLink {
    device: Mutex<Option<TrayDevice>>,
    pending: Mutex<PendingChanges>,
    changed: Notify,
    app_audio: AppAudioHandle,
}

// MacOS is similar to Windows, except it expects the App loop to exist on the main thread..
pub fn handle_tray(state: DaemonState, tx: Sender<EventTriggers>) -> anyhow::Result<()> {
    // Eventually, we're going to need to spawn a new thread which can cause a shutdown from cocoa,
    // but until then.. eh..
    let show_tray = state.show_tray.clone();

    let link = Arc::new(TrayLink {
        device: Mutex::default(),
        pending: Mutex::default(),
        changed: Notify::new(),
        app_audio: state.app_audio.clone(),
    });

    let (tray_tx, tray_rx) = channel(10);
    tokio::spawn(run_tray(RunParams {
        tray_receiver: tray_rx,
        event_sender: tx.clone(),
        state: state.clone(),
        link: link.clone(),
    }));

    debug!("Starting MacOS Tray Runtime..");
    App::create(AppParams {
        sender: tray_tx,
        show_tray,
        state,
        global_tx: tx.clone(),
        link,
    });
    debug!("MacOS Tray Runtime Stopped..");

    Ok(())
}

struct RunParams {
    tray_receiver: Receiver<TrayOption>,
    event_sender: Sender<EventTriggers>,
    state: DaemonState,
    link: Arc<TrayLink>,
}

async fn run_tray(mut p: RunParams) {
    let mut patches = p.state.broadcast_tx.subscribe();
    let mut save_at: Option<Instant> = None;
    refresh_device(&p).await;

    loop {
        select! {
            Ok(_) | Err(RecvError::Lagged(_)) = patches.recv() => {
                // Something changed on a device, fetch the status once for the whole burst.
                while matches!(patches.try_recv(), Ok(_) | Err(TryRecvError::Lagged(_))) {}
                refresh_device(&p).await;
            },
            () = p.link.changed.notified() => {
                let (toggles, volumes) = apply_changes(&p).await;
                if toggles {
                    save_at = None;
                    p.state.settings_handle.save().await;
                } else if volumes {
                    save_at = Some(Instant::now() + SAVE_DELAY);
                }
                tokio::time::sleep(COMMAND_INTERVAL).await;
            },
            () = sleep_until(save_at.unwrap_or_else(Instant::now)), if save_at.is_some() => {
                save_at = None;
                p.state.settings_handle.save().await;
            },
            Some(tray) = p.tray_receiver.recv() => {
                debug!("Received Tray Message! {:?}", tray);

                let tx = p.event_sender.clone();
                let _ = match tray {
                    Configure => tx.try_send(EventTriggers::Activate),
                    OpenPathProfiles => tx.try_send(Open(PathTypes::Profiles)),
                    OpenPathMicProfiles => tx.try_send(Open(PathTypes::MicProfiles)),
                    OpenPathPresets => tx.try_send(Open(PathTypes::Presets)),
                    OpenPathSamples => tx.try_send(Open(PathTypes::Samples)),
                    OpenPathIcons => tx.try_send(Open(PathTypes::Icons)),
                    OpenPathLogs => tx.try_send(Open(PathTypes::Logs)),
                    Quit => tx.try_send(EventTriggers::Stop(false))
                };
            },
            () = p.state.shutdown.recv() => {
               debug!("Shutting Down, Attempting to kill the NSApp..");
                unsafe {
                    stop_ns_application();
                    break;
                }
            }
        }
    }
}

async fn refresh_device(p: &RunParams) {
    let (tx, rx) = oneshot::channel();
    if p.state
        .usb_tx
        .send(DeviceCommand::SendDaemonStatus(tx))
        .await
        .is_err()
    {
        return;
    }
    let Ok(status) = rx.await else {
        return;
    };
    let device = status
        .mixers
        .iter()
        .min_by_key(|(serial, _)| *serial)
        .map(|(serial, mixer)| TrayDevice {
            serial: serial.clone(),
            volumes: mixer.levels.volumes,
        });
    *p.link.device.lock().unwrap() = device;
}

// Returns whether app routes or mutes, and app volumes were changed, so the caller can save them.
async fn apply_changes(p: &RunParams) -> (bool, bool) {
    let changes = mem::take(&mut *p.link.pending.lock().unwrap());

    let serial = p
        .link
        .device
        .lock()
        .unwrap()
        .as_ref()
        .map(|d| d.serial.clone());
    if let Some(serial) = serial {
        for (channel, volume) in changes.volumes {
            let Some(volume) = volume else {
                continue;
            };
            let (tx, rx) = oneshot::channel();
            let command = GoXLRCommand::SetVolume(channel, volume);
            let command = DeviceCommand::RunDeviceCommand(serial.clone(), command, tx);
            if p.state.usb_tx.send(command).await.is_ok() {
                let _ = rx.await;
            }
        }
    }

    let settings = &p.state.settings_handle;
    let rules = settings.get_macos_app_rules().await;
    let bundle_ids: HashSet<&String> = changes
        .app_routes
        .keys()
        .chain(changes.app_volumes.keys())
        .chain(changes.app_mutes.keys())
        .collect();
    for bundle_id in bundle_ids {
        let mut rule = rules.get(bundle_id).copied().unwrap_or_default();
        if let Some(route) = changes.app_routes.get(bundle_id) {
            rule.route = *route;
        }
        if let Some(volume) = changes.app_volumes.get(bundle_id) {
            rule.volume = *volume;
        }
        if let Some(muted) = changes.app_mutes.get(bundle_id) {
            rule.muted = *muted;
        }
        settings.set_macos_app_rule(bundle_id.clone(), rule).await;
        p.link.app_audio.rule_changed(bundle_id.clone(), rule);
    }
    (
        !changes.app_routes.is_empty() || !changes.app_mutes.is_empty(),
        !changes.app_volumes.is_empty(),
    )
}

unsafe fn stop_ns_application() {
    // First let the NSApplication know it's time to Stop..
    let main_queue = Queue::main();
    main_queue.exec_async(|| {
        let mtm = MainThreadMarker::new().unwrap();
        let app = NSApplication::sharedApplication(mtm);
        app.stop(None);

        // Next, we generate an Application Event..
        let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::new(0., 0.),
            NSEventModifierFlags::empty(),
            NSTimeInterval::default(),
            0,
            None,
            NSEventSubtype::WindowExposed.0,
            0,
            0,
        ).unwrap();

        // Then we send it to the NSApplication. The application RunLoop only stops after the 'next'
        // event, so we'll force one to ensure shutdown.
        app.postEvent_atStart(&event, true)
    });
}

#[derive(Display, Debug, Enum, EnumIter, Eq, PartialEq)]
enum TrayOption {
    Configure,
    OpenPathProfiles,
    OpenPathMicProfiles,
    OpenPathPresets,
    OpenPathSamples,
    OpenPathIcons,
    OpenPathLogs,
    Quit,
}

struct App {}

struct AppParams {
    sender: Sender<TrayOption>,
    show_tray: Arc<AtomicBool>,
    state: DaemonState,
    global_tx: Sender<EventTriggers>,
    link: Arc<TrayLink>,
}

impl App {
    pub fn create(p: AppParams) {
        debug!("Preparing Tray..");
        let mtm = MainThreadMarker::new().unwrap();

        // Step 1, create the initial release pool, and base menu..
        unsafe { NSAutoreleasePool::new() };

        // Configure the Application..
        let current = NSRunningApplication::currentApplication();
        current.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);

        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.activateIgnoringOtherApps(true);

        // Setting the App Delegate..
        let delegate = UtilityDelegate::new(
            mtm,
            p.sender.clone(),
            p.global_tx.clone(),
            p.state.shutdown_blocking.clone(),
            p.link,
        );
        let object = ProtocolObject::from_ref(&*delegate);
        app.setDelegate(Some(object));

        let status = if p.show_tray.load(Ordering::Relaxed) {
            debug!("Spawning Tray..");
            let status = NSStatusBar::systemStatusBar().statusItemWithLength(-1.);

            let button = status.button(mtm);
            let data = NSData::with_bytes(ICON_MAC);
            if let Some(icon) = NSImage::initWithData(NSImage::alloc(), &data) {
                icon.setSize(NSSize::new(18., 18.));
                icon.setTemplate(false);

                if let Some(button) = button {
                    button.setImage(Some(&*icon));
                    button.setImagePosition(NSCellImagePosition::ImageLeft)
                }
            }

            Some(status)
        } else {
            None
        };

        // The menu is rebuilt every time it opens, so it shows the current volumes and apps.
        if let Some(status) = status {
            debug!("Building Menu..");
            let menu = NSMenu::new(mtm);
            menu.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            delegate.rebuild_menu(&menu);
            status.setMenu(Some(&*menu));
        }

        // Before we run, register with the observer to see if shutdown is going to happen..
        let workspace = NSWorkspace::sharedWorkspace();
        let notification_center = workspace.notificationCenter();

        // Get the Distributed Notification Center (for Lock / Unlock Notifications)
        let dnc = NSDistributedNotificationCenter::defaultCenter();

        debug!("Registering Event..");
        let event = "NSWorkspaceWillPowerOffNotification";
        let event = NSNotificationName::from_str(event);

        debug!("Registering Class..");
        unsafe {
            notification_center.addObserver_selector_name_object(
                &delegate,
                sel!(computerWillShutDownNotification:),
                Some(&event),
                None,
            );
        }

        // We probably shouldn't share pointers to the senders, but seeing as MacOS locks
        // the entire NS runtime into a single thread, we should be safe here.
        let event = "NSWorkspaceWillSleepNotification";
        let event = NSNotificationName::from_str(event);

        unsafe {
            notification_center.addObserver_selector_name_object(
                &delegate,
                sel!(computerWillSleepNotification:),
                Some(&event),
                None,
            );
        }

        let event = "NSWorkspaceDidWakeNotification";
        let event = NSNotificationName::from_str(event);
        unsafe {
            notification_center.addObserver_selector_name_object(
                &delegate,
                sel!(computerWillWakeNotification:),
                Some(&event),
                None,
            );
        }

        let event = "com.apple.screenIsLocked";
        let event = NSNotificationName::from_str(event);

        unsafe {
            dnc.addObserver_selector_name_object(
                &delegate,
                sel!(screenIsLocked:),
                Some(&event),
                None,
            );
        }

        let event = "com.apple.screenIsUnlocked";
        let event = NSNotificationName::from_str(event);
        unsafe {
            dnc.addObserver_selector_name_object(
                &delegate,
                sel!(screenIsUnlocked:),
                Some(&event),
                None,
            );
        }

        debug!("Running..");
        app.run();
    }

    fn get_label(mtm: MainThreadMarker, label: &str, option: TrayOption) -> Retained<NSMenuItem> {
        unsafe {
            let title = NSString::from_str(label);

            let item = NSMenuItem::new(mtm);
            item.setAction(Some(sel!(menu_item:)));
            item.setTitle(&title);

            // Two approaches for storing enum data in NSMenuItem:
            //
            // 1. Store enum variant index in tag (safer, simpler):
            //    - Convert enum to its index: option as isize
            //    - Retrieve with: TrayOption::iter().nth(item.tag() as usize).unwrap()
            //    - Limited to simple enums without associated data
            //
            // 2. Store pointer to boxed data in tag (more flexible but unsafe):
            //    let data = Box::new(option);
            //    let ptr = Box::into_raw(data);
            //    let tag_value = ptr as usize as isize;
            //    item.setTag(tag_value);
            //
            //    // Retrieve with:
            //    let ptr = item.tag() as usize as *mut TrayOption;
            //    let option = &*ptr;  // Note: must not take ownership to avoid double-free
            //
            //    // WARNING: This causes memory leaks as boxed data is never freed
            //    // Only use when necessary for complex data that can't be encoded as an index

            item.setTag(option as isize);

            item
        }
    }

    fn add_static_items(mtm: MainThreadMarker, menu: &NSMenu) {
        let sub_title = NSString::from_str("Open Path");

        // Create the Main Tray Labels..
        let configure = App::get_label(mtm, "Configure GoXLR", Configure);
        let quit = App::get_label(mtm, "Quit", Quit);

        // Create SubMenu Items..
        let profiles = App::get_label(mtm, "Profiles", OpenPathProfiles);
        let mic_profiles = App::get_label(mtm, "Mic Profiles", OpenPathMicProfiles);
        let presets = App::get_label(mtm, "Presets", OpenPathPresets);
        let samples = App::get_label(mtm, "Samples", OpenPathSamples);
        let icons = App::get_label(mtm, "Icons", OpenPathIcons);
        let logs = App::get_label(mtm, "Logs", OpenPathLogs);

        let sub_menu = {
            let menu_item = NSMenuItem::new(mtm);
            let menu = NSMenu::new(mtm);

            menu.setTitle(&sub_title);
            menu_item.setTitle(&sub_title);
            menu_item.setSubmenu(Some(&menu));

            menu.addItem(&profiles);
            menu.addItem(&mic_profiles);
            menu.addItem(&App::get_separator(mtm));
            menu.addItem(&presets);
            menu.addItem(&samples);
            menu.addItem(&icons);
            menu.addItem(&App::get_separator(mtm));
            menu.addItem(&logs);

            menu_item
        };

        menu.addItem(&configure);
        menu.addItem(&App::get_separator(mtm));
        menu.addItem(&sub_menu);
        menu.addItem(&App::get_separator(mtm));
        menu.addItem(&quit);
    }

    // An item without an action, which the menu shows disabled.
    fn get_header(mtm: MainThreadMarker, title: &str) -> Retained<NSMenuItem> {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(title));
        item
    }

    fn get_separator(mtm: MainThreadMarker) -> Retained<NSMenuItem> {
        let separator = NSMenuItem::separatorItem(mtm);
        separator.retain();
        separator
    }
}

pub(crate) struct State {
    sender: Sender<TrayOption>,
    global_tx: Sender<EventTriggers>,
    shutdown_signal: Arc<AtomicBool>,
    link: Arc<TrayLink>,
    // The bundle IDs of the apps in the open menu, indexed by the app items' tags.
    menu_apps: RefCell<Vec<String>>,
}

define_class! {
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "UtilityDelegate"]
    #[ivars = State]
    pub(crate) struct UtilityDelegate;

    unsafe impl NSObjectProtocol for UtilityDelegate {}

    unsafe impl NSApplicationDelegate for UtilityDelegate {
        //Showcase function for now
        #[unsafe(method(menu_item:))]
        unsafe fn menu_item(&self, item: &NSMenuItem) {
            if let Some(option) = TrayOption::iter().nth(item.tag() as usize)
                && self.ivars().sender.try_send(option).is_err() {
                    warn!("Failed to send Tray Signal");
                }
        }
    }

    unsafe impl NSMenuDelegate for UtilityDelegate {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.rebuild_menu(menu);
        }
    }

    impl UtilityDelegate {
        #[unsafe(method(channelVolume:))]
        fn channel_volume(&self, slider: &ScrollSlider) {
            let channel = ChannelName::from_usize(slider.tag() as usize);
            let volume = slider.doubleValue().round() as u8;
            let link = &self.ivars().link;
            if let Some(device) = link.device.lock().unwrap().as_mut() {
                device.volumes[channel] = volume;
            }
            link.pending.lock().unwrap().volumes[channel] = Some(volume);
            link.changed.notify_one();
        }

        #[unsafe(method(appRoute:))]
        fn app_route(&self, item: &NSMenuItem) {
            let tag = item.tag() as usize;
            let Some(bundle_id) = self.menu_app(tag / APP_TAG_STRIDE) else {
                return;
            };
            let route = (tag % APP_TAG_STRIDE).checked_sub(1);
            let link = &self.ivars().link;
            link.pending.lock().unwrap().app_routes.insert(bundle_id, route);
            link.changed.notify_one();
        }

        #[unsafe(method(appVolume:))]
        fn app_volume(&self, slider: &ScrollSlider) {
            slider.update_label();
            let Some(bundle_id) = self.menu_app(slider.tag() as usize) else {
                return;
            };
            let volume = slider.doubleValue().round() as u16;
            let link = &self.ivars().link;
            link.pending.lock().unwrap().app_volumes.insert(bundle_id, volume);
            link.changed.notify_one();
        }

        #[unsafe(method(appMute:))]
        fn app_mute(&self, item: &NSMenuItem) {
            let Some(bundle_id) = self.menu_app(item.tag() as usize) else {
                return;
            };
            // The item shows the state the menu was built with, so toggle that.
            let muted = item.state() != NSControlStateValueOn;
            let link = &self.ivars().link;
            link.pending.lock().unwrap().app_mutes.insert(bundle_id, muted);
            link.changed.notify_one();
        }

        #[unsafe(method(computerWillShutDownNotification:))]
        unsafe fn computer_will_shutdown(&self, notification: &NSNotification) {
            debug!("Received Shutdown Notification! {:?}", notification);
                // This is pretty similar to Windows, we loop until we're ready to die..
                let _ = self.ivars().global_tx.try_send(EventTriggers::Stop(false));

                // Now wait (for up to 5 seconds) for the daemon to actually stop..
                let mut count = 0;
                while !self.ivars().shutdown_signal.load(Ordering::Relaxed) {
                    if count >= 50 {
                        warn!("Daemon did not stop in time, continuing shutdown");
                        break;
                    }
                    debug!("Waiting..");
                    sleep(Duration::from_millis(100));
                    count += 1;
                }
        }

        #[unsafe(method(computerWillSleepNotification:))]
        unsafe fn computer_will_sleep(&self, notification: &NSNotification) {
            debug!("Received Sleep Notification! {:?}", notification);
                // Pretty much copypasta from Windows which behaves in a similar way..
                let (tx, mut rx) = oneshot::channel();

                // Give a maximum of 1 second for a response..
                let milli_wait = 5;
                let max_wait = 1000 / milli_wait;
                let mut count = 0;

                if self.ivars().global_tx.try_send(EventTriggers::Sleep(tx)).is_ok() {
                    debug!("Awaiting Sleep Response..");
                    while rx.try_recv().is_err() {
                        sleep(Duration::from_millis(milli_wait));
                        count += 1;
                        if count > max_wait {
                            debug!("Timeout Exceeded, bailing.");
                            break;
                        }
                    }
                    debug!("Task Completed, allowing MacOS to Sleep");
                }
        }

        #[unsafe(method(computerWillWakeNotification:))]
        unsafe fn computer_will_wake(&self, notification: &NSNotification) {
            debug!("Received Wake Notification! {:?}", notification);
            let (tx, _rx) = oneshot::channel();
            let _ = self.ivars().global_tx.try_send(EventTriggers::Wake(tx));
        }

        #[unsafe(method(screenIsLocked:))]
        unsafe fn screen_is_locked(&self, notification: &NSNotification) {
            debug!("Received Lock Notification.. {:?}", notification);
            let _ = self.ivars().global_tx.try_send(EventTriggers::Lock);
        }

        #[unsafe(method(screenIsUnlocked:))]
        unsafe fn screen_is_unlocked(&self, notification: &NSNotification) {
            debug!("Received Unlock Notification.. {:?}", notification);
            let _ = self.ivars().global_tx.try_send(EventTriggers::Unlock);
        }
    }
}

impl UtilityDelegate {
    fn new(
        mtm: MainThreadMarker,
        sender: Sender<TrayOption>,
        global_tx: Sender<EventTriggers>,
        shutdown_signal: Arc<AtomicBool>,
        link: Arc<TrayLink>,
    ) -> Retained<Self> {
        let delegate = mtm.alloc().set_ivars(State {
            sender,
            global_tx,
            shutdown_signal,
            link,
            menu_apps: RefCell::default(),
        });

        unsafe { msg_send![super(delegate), init] }
    }

    fn menu_app(&self, index: usize) -> Option<String> {
        self.ivars().menu_apps.borrow().get(index).cloned()
    }

    fn rebuild_menu(&self, menu: &NSMenu) {
        let mtm = self.mtm();
        let device = self.ivars().link.device.lock().unwrap().clone();
        let snapshot = self.ivars().link.app_audio.snapshot();
        menu.removeAllItems();

        if let Some(device) = device {
            menu.addItem(&App::get_header(mtm, "Volume"));
            for (route, channel) in PLAYBACK_CHANNELS.into_iter().enumerate() {
                // Sample only has a virtual output while its route is enabled.
                if channel == ChannelName::Sample && !playback_route_enabled(snapshot.routes, route)
                {
                    continue;
                }
                menu.addItem(&self.slider_item(
                    Some(PLAYBACK_NAMES[route]),
                    device.volumes[channel].into(),
                    255.,
                    sel!(channelVolume:),
                    channel.into_usize(),
                ));
            }
            menu.addItem(&App::get_separator(mtm));
        }

        menu.addItem(&App::get_header(mtm, "Apps"));
        let apps: Vec<_> = snapshot
            .apps
            .iter()
            .filter(|app| {
                ((app.playing && app.device_route.is_some())
                    || snapshot.rules.contains_key(&app.bundle_id))
                    && !snapshot.hidden.contains(&app.bundle_id)
            })
            .collect();
        if apps.is_empty() {
            menu.addItem(&App::get_header(mtm, "No apps playing"));
        }
        for (index, app) in apps.iter().enumerate() {
            let rule = snapshot
                .rules
                .get(&app.bundle_id)
                .copied()
                .unwrap_or_default();
            let output = rule
                .route
                .or(app.device_route)
                .and_then(|route| PLAYBACK_NAMES.get(route))
                .map_or("Not GoXLR", |name| name);

            let submenu = NSMenu::new(mtm);
            let routes = (0..PLAYBACK_NAMES.len())
                .filter(|route| playback_route_enabled(snapshot.routes, *route))
                .map(Some);
            for choice in once(None).chain(routes) {
                let item = NSMenuItem::new(mtm);
                item.setTitle(&NSString::from_str(
                    choice.map_or("App's own output", |route| PLAYBACK_NAMES[route]),
                ));
                unsafe {
                    item.setTarget(Some(self.as_ref()));
                    item.setAction(Some(sel!(appRoute:)));
                }
                item.setTag(
                    (index * APP_TAG_STRIDE + choice.map_or(0, |route| route + 1)) as isize,
                );
                if choice == rule.route {
                    item.setState(NSControlStateValueOn);
                }
                submenu.addItem(&item);
            }
            submenu.addItem(&App::get_separator(mtm));
            let mute = NSMenuItem::new(mtm);
            mute.setTitle(&NSString::from_str("Mute"));
            unsafe {
                mute.setTarget(Some(self.as_ref()));
                mute.setAction(Some(sel!(appMute:)));
            }
            mute.setTag(index as isize);
            if rule.muted {
                mute.setState(NSControlStateValueOn);
            }
            submenu.addItem(&mute);
            submenu.addItem(&self.slider_item(
                None,
                rule.volume.into(),
                APP_VOLUME_MAX,
                sel!(appVolume:),
                index,
            ));

            let muted = if rule.muted { ", muted" } else { "" };
            let item = NSMenuItem::new(mtm);
            item.setTitle(&NSString::from_str(&format!(
                "{} — {output}{muted}",
                app.name
            )));
            item.setSubmenu(Some(&submenu));
            menu.addItem(&item);
        }
        *self.ivars().menu_apps.borrow_mut() =
            apps.iter().map(|app| app.bundle_id.clone()).collect();

        menu.addItem(&App::get_separator(mtm));
        App::add_static_items(mtm, menu);
    }

    // A menu row holding a label and a continuous slider which sends `action` to us. Without a
    // label, it shows the slider's value in percent.
    fn slider_item(
        &self,
        label: Option<&str>,
        value: f64,
        max: f64,
        action: Sel,
        tag: usize,
    ) -> Retained<NSMenuItem> {
        let mtm = self.mtm();
        let frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(260., 28.));
        let view = NSView::initWithFrame(NSView::alloc(mtm), frame);

        let text = NSTextField::labelWithString(&NSString::from_str(label.unwrap_or("")), mtm);
        text.setFont(Some(&NSFont::menuFontOfSize(0.)));
        text.setFrame(NSRect::new(NSPoint::new(20., 5.), NSSize::new(62., 18.)));

        let frame = NSRect::new(NSPoint::new(86., 4.), NSSize::new(158., 20.));
        let slider = ScrollSlider::new(mtm, frame, label.is_none().then(|| text.clone()));
        let target: &AnyObject = self.as_ref();
        slider.setMinValue(0.);
        slider.setMaxValue(max);
        slider.setDoubleValue(value);
        unsafe {
            slider.setTarget(Some(target));
            slider.setAction(Some(action));
        }
        slider.setContinuous(true);
        slider.setTag(tag as isize);
        slider.update_label();

        view.addSubview(&text);
        view.addSubview(&slider);
        let item = NSMenuItem::new(mtm);
        item.setView(Some(&view));
        item
    }
}

pub(crate) struct SliderState {
    // Shows the value in percent, for sliders without a name.
    percent_label: Option<Retained<NSTextField>>,
}

define_class! {
    // A menu slider that also follows the scroll wheel.
    #[unsafe(super(NSSlider, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GoXLRScrollSlider"]
    #[ivars = SliderState]
    pub(crate) struct ScrollSlider;

    impl ScrollSlider {
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            // Up (away from the user) raises the value, whatever the scroll direction setting.
            let mut delta = event.scrollingDeltaY();
            if event.isDirectionInvertedFromDevice() {
                delta = -delta;
            }
            // Trackpads report points rather than wheel notches.
            if event.hasPreciseScrollingDeltas() {
                delta /= 10.;
            }
            let (min, max) = (self.minValue(), self.maxValue());
            let current = self.doubleValue();
            let value = (current + delta * (max - min) * SCROLL_STEP).clamp(min, max);
            if value == current {
                return;
            }
            self.setDoubleValue(value);
            // Report it like a drag, so the change goes through the same path.
            unsafe {
                self.sendAction_to(self.action(), self.target().as_deref());
            }
        }
    }
}

impl ScrollSlider {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        percent_label: Option<Retained<NSTextField>>,
    ) -> Retained<Self> {
        let slider = mtm.alloc().set_ivars(SliderState { percent_label });
        unsafe { msg_send![super(slider), initWithFrame: frame] }
    }

    fn update_label(&self) {
        if let Some(label) = &self.ivars().percent_label {
            let percent = self.doubleValue().round();
            label.setStringValue(&NSString::from_str(&format!("{percent} %")));
        }
    }
}
