// Global hotkeys through Carbon's RegisterEventHotKey, which needs no Accessibility permission.
// Carbon expects all of this on the main thread, where the tray runs NSApp, so registration is
// dispatched to the main queue and the handler only hands the press over to the tray task.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::OnceLock;
use std::{mem, ptr};

use dispatch2::DispatchQueue;
use goxlr_ipc::{Binding, HotkeyAction, HotkeyModifiers};
use log::{debug, warn};
use objc2_app_kit::NSWorkspace;
use tokio::sync::mpsc::{Receiver, Sender, channel};

type OSStatus = i32;
type EventTargetRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type EventHotKeyRef = *mut c_void;
type EventHandlerUPP = extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Default)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerUPP,
        num_types: usize,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> OSStatus;
    fn GetEventParameter(
        event: EventRef,
        name: u32,
        desired_type: u32,
        actual_type: *mut u32,
        buffer_size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> OSStatus;
}

const NO_ERR: OSStatus = 0;
const EVENT_NOT_HANDLED_ERR: OSStatus = -9874;
const K_EVENT_CLASS_KEYBOARD: u32 = 0x6b657962; // 'keyb'
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const K_EVENT_PARAM_DIRECT_OBJECT: u32 = 0x2d2d2d2d; // '----'
const TYPE_EVENT_HOT_KEY_ID: u32 = 0x686b6964; // 'hkid'
const HOTKEY_SIGNATURE: u32 = 0x47584c52; // 'GXLR'

// Carbon modifier masks, from HIToolbox's Events.h.
const CMD_KEY: u32 = 1 << 8;
const SHIFT_KEY: u32 = 1 << 9;
const OPTION_KEY: u32 = 1 << 11;
const CONTROL_KEY: u32 = 1 << 12;

/// A hotkey press, with the frontmost app's bundle ID when the action needs it.
#[derive(Debug)]
pub struct HotkeyPress {
    pub action: HotkeyAction,
    pub frontmost: Option<String>,
}

static PRESSES: OnceLock<Sender<HotkeyPress>> = OnceLock::new();

// The registered hotkeys and their actions, indexed by hotkey ID. Only used on the main thread.
#[derive(Default)]
struct Registry {
    handler_installed: bool,
    hotkeys: Vec<EventHotKeyRef>,
    actions: Vec<HotkeyAction>,
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::default();
}

/// Returns the receiver for hotkey presses, there's only one.
pub fn listen() -> Receiver<HotkeyPress> {
    let (tx, rx) = channel(16);
    if PRESSES.set(tx).is_err() {
        warn!("Hotkey presses already have a listener");
    }
    rx
}

/// Replaces the registered hotkeys, on the main thread.
pub fn register_hotkeys(bindings: Vec<Binding>) {
    DispatchQueue::main()
        .exec_async(move || REGISTRY.with_borrow_mut(|registry| registry.set(bindings)));
}

impl Registry {
    fn set(&mut self, bindings: Vec<Binding>) {
        let target = unsafe { GetApplicationEventTarget() };
        if !self.handler_installed {
            let spec = EventTypeSpec {
                event_class: K_EVENT_CLASS_KEYBOARD,
                event_kind: K_EVENT_HOT_KEY_PRESSED,
            };
            let status = unsafe {
                InstallEventHandler(
                    target,
                    on_hotkey,
                    1,
                    &spec,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if status != NO_ERR {
                warn!("Unable to install the hotkey handler: {status}");
                return;
            }
            self.handler_installed = true;
        }

        for hotkey in self.hotkeys.drain(..) {
            unsafe { UnregisterEventHotKey(hotkey) };
        }
        self.actions.clear();

        for binding in bindings {
            let Some(key_code) = key_code(&binding.code) else {
                warn!("Skipping hotkey with unknown key {:?}", binding.code);
                continue;
            };
            if !is_valid(&binding.code, binding.modifiers) {
                warn!("Skipping hotkey {:?} without a modifier", binding.code);
                continue;
            }
            let id = EventHotKeyID {
                signature: HOTKEY_SIGNATURE,
                id: self.actions.len() as u32,
            };
            let mut hotkey = ptr::null_mut();
            let modifiers = modifier_mask(binding.modifiers);
            let status =
                unsafe { RegisterEventHotKey(key_code, modifiers, id, target, 0, &mut hotkey) };
            if status != NO_ERR {
                warn!("Unable to register hotkey {binding:?}, it may be taken: {status}");
                continue;
            }
            self.hotkeys.push(hotkey);
            self.actions.push(binding.action);
        }
        debug!("Registered {} hotkeys", self.hotkeys.len());
    }
}

extern "C" fn on_hotkey(
    _next: EventHandlerCallRef,
    event: EventRef,
    _user_data: *mut c_void,
) -> OSStatus {
    let mut id = EventHotKeyID::default();
    let status = unsafe {
        GetEventParameter(
            event,
            K_EVENT_PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            ptr::null_mut(),
            mem::size_of::<EventHotKeyID>(),
            ptr::null_mut(),
            (&mut id as *mut EventHotKeyID).cast(),
        )
    };
    if status != NO_ERR || id.signature != HOTKEY_SIGNATURE {
        return EVENT_NOT_HANDLED_ERR;
    }
    let Some(action) =
        REGISTRY.with_borrow(|registry| registry.actions.get(id.id as usize).copied())
    else {
        return EVENT_NOT_HANDLED_ERR;
    };

    // Read here while we're on the main thread, AppKit keeps it up to date from this run loop.
    let frontmost = if action == HotkeyAction::ToggleFrontmostAppMute {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .and_then(|app| app.bundleIdentifier())
            .map(|id| id.to_string())
    } else {
        None
    };
    if let Some(presses) = PRESSES.get()
        && presses.try_send(HotkeyPress { action, frontmost }).is_err()
    {
        warn!("Dropped a hotkey press");
    }
    NO_ERR
}

fn modifier_mask(modifiers: HotkeyModifiers) -> u32 {
    [
        (modifiers.command, CMD_KEY),
        (modifiers.option, OPTION_KEY),
        (modifiers.control, CONTROL_KEY),
        (modifiers.shift, SHIFT_KEY),
    ]
    .into_iter()
    .filter(|(set, _)| *set)
    .fold(0, |mask, (_, key)| mask | key)
}

// Every hotkey needs a modifier, except on the F13 - F19 keys that nothing else uses.
fn is_valid(code: &str, modifiers: HotkeyModifiers) -> bool {
    modifiers != HotkeyModifiers::default()
        || matches!(code, "F13" | "F14" | "F15" | "F16" | "F17" | "F18" | "F19")
}

// Maps a web `KeyboardEvent.code` to a macOS virtual key code (kVK_* in HIToolbox's Events.h).
fn key_code(code: &str) -> Option<u32> {
    Some(match code {
        "KeyA" => 0x00,
        "KeyS" => 0x01,
        "KeyD" => 0x02,
        "KeyF" => 0x03,
        "KeyH" => 0x04,
        "KeyG" => 0x05,
        "KeyZ" => 0x06,
        "KeyX" => 0x07,
        "KeyC" => 0x08,
        "KeyV" => 0x09,
        "IntlBackslash" => 0x0A,
        "KeyB" => 0x0B,
        "KeyQ" => 0x0C,
        "KeyW" => 0x0D,
        "KeyE" => 0x0E,
        "KeyR" => 0x0F,
        "KeyY" => 0x10,
        "KeyT" => 0x11,
        "Digit1" => 0x12,
        "Digit2" => 0x13,
        "Digit3" => 0x14,
        "Digit4" => 0x15,
        "Digit6" => 0x16,
        "Digit5" => 0x17,
        "Equal" => 0x18,
        "Digit9" => 0x19,
        "Digit7" => 0x1A,
        "Minus" => 0x1B,
        "Digit8" => 0x1C,
        "Digit0" => 0x1D,
        "BracketRight" => 0x1E,
        "KeyO" => 0x1F,
        "KeyU" => 0x20,
        "BracketLeft" => 0x21,
        "KeyI" => 0x22,
        "KeyP" => 0x23,
        "Enter" => 0x24,
        "KeyL" => 0x25,
        "KeyJ" => 0x26,
        "Quote" => 0x27,
        "KeyK" => 0x28,
        "Semicolon" => 0x29,
        "Backslash" => 0x2A,
        "Comma" => 0x2B,
        "Slash" => 0x2C,
        "KeyN" => 0x2D,
        "KeyM" => 0x2E,
        "Period" => 0x2F,
        "Tab" => 0x30,
        "Space" => 0x31,
        "Backquote" => 0x32,
        "Backspace" => 0x33,
        "Escape" => 0x35,
        "F17" => 0x40,
        "NumpadDecimal" => 0x41,
        "NumpadMultiply" => 0x43,
        "NumpadAdd" => 0x45,
        "NumLock" => 0x47,
        "NumpadDivide" => 0x4B,
        "NumpadEnter" => 0x4C,
        "NumpadSubtract" => 0x4E,
        "F18" => 0x4F,
        "F19" => 0x50,
        "NumpadEqual" => 0x51,
        "Numpad0" => 0x52,
        "Numpad1" => 0x53,
        "Numpad2" => 0x54,
        "Numpad3" => 0x55,
        "Numpad4" => 0x56,
        "Numpad5" => 0x57,
        "Numpad6" => 0x58,
        "Numpad7" => 0x59,
        "F20" => 0x5A,
        "Numpad8" => 0x5B,
        "Numpad9" => 0x5C,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F3" => 0x63,
        "F8" => 0x64,
        "F9" => 0x65,
        "F11" => 0x67,
        "F13" => 0x69,
        "F16" => 0x6A,
        "F14" => 0x6B,
        "F10" => 0x6D,
        "F12" => 0x6F,
        "F15" => 0x71,
        "Home" => 0x73,
        "PageUp" => 0x74,
        "Delete" => 0x75,
        "F4" => 0x76,
        "End" => 0x77,
        "F2" => 0x78,
        "PageDown" => 0x79,
        "F1" => 0x7A,
        "ArrowLeft" => 0x7B,
        "ArrowRight" => 0x7C,
        "ArrowDown" => 0x7D,
        "ArrowUp" => 0x7E,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{CMD_KEY, CONTROL_KEY, OPTION_KEY, SHIFT_KEY, is_valid, key_code, modifier_mask};
    use goxlr_ipc::HotkeyModifiers;
    use std::collections::HashSet;

    fn modifiers(command: bool, option: bool, control: bool, shift: bool) -> HotkeyModifiers {
        HotkeyModifiers {
            command,
            option,
            control,
            shift,
        }
    }

    #[test]
    fn maps_web_key_codes() {
        assert_eq!(key_code("KeyA"), Some(0x00));
        assert_eq!(key_code("KeyM"), Some(0x2E));
        assert_eq!(key_code("KeyZ"), Some(0x06));
        assert_eq!(key_code("Digit0"), Some(0x1D));
        assert_eq!(key_code("Digit1"), Some(0x12));
        assert_eq!(key_code("F1"), Some(0x7A));
        assert_eq!(key_code("F13"), Some(0x69));
        assert_eq!(key_code("F19"), Some(0x50));
        assert_eq!(key_code("F20"), Some(0x5A));
        assert_eq!(key_code("ArrowUp"), Some(0x7E));
        assert_eq!(key_code("ArrowLeft"), Some(0x7B));
        assert_eq!(key_code("Space"), Some(0x31));
        assert_eq!(key_code("Enter"), Some(0x24));
        assert_eq!(key_code("Escape"), Some(0x35));
        assert_eq!(key_code("Minus"), Some(0x1B));
        assert_eq!(key_code("Backquote"), Some(0x32));
        assert_eq!(key_code("Numpad0"), Some(0x52));
        assert_eq!(key_code("Numpad8"), Some(0x5B));
        assert_eq!(key_code("NumpadEnter"), Some(0x4C));
        assert_eq!(key_code("MetaLeft"), None);
        assert_eq!(key_code("keya"), None);
        assert_eq!(key_code(""), None);
    }

    #[test]
    fn covers_every_letter_digit_and_function_key_once() {
        let codes: Vec<String> = ('A'..='Z')
            .map(|letter| format!("Key{letter}"))
            .chain((0..=9).map(|digit| format!("Digit{digit}")))
            .chain((0..=9).map(|digit| format!("Numpad{digit}")))
            .chain((1..=20).map(|key| format!("F{key}")))
            .collect();
        let mapped: HashSet<u32> = codes
            .iter()
            .map(|code| key_code(code).unwrap_or_else(|| panic!("{code} isn't mapped")))
            .collect();
        assert_eq!(mapped.len(), codes.len());
    }

    #[test]
    fn maps_modifiers() {
        assert_eq!(modifier_mask(modifiers(false, false, false, false)), 0);
        assert_eq!(modifier_mask(modifiers(true, false, false, false)), 0x100);
        assert_eq!(modifier_mask(modifiers(false, false, false, true)), 0x200);
        assert_eq!(modifier_mask(modifiers(false, true, false, false)), 0x800);
        assert_eq!(modifier_mask(modifiers(false, false, true, false)), 0x1000);
        assert_eq!(
            modifier_mask(modifiers(true, true, true, true)),
            CMD_KEY | OPTION_KEY | CONTROL_KEY | SHIFT_KEY
        );
    }

    #[test]
    fn requires_a_modifier_except_on_f13_to_f19() {
        let none = HotkeyModifiers::default();
        assert!(!is_valid("KeyM", none));
        assert!(!is_valid("F12", none));
        assert!(!is_valid("F20", none));
        assert!(is_valid("F13", none));
        assert!(is_valid("F19", none));
        assert!(is_valid("KeyM", modifiers(false, false, false, true)));
        assert!(is_valid("ArrowUp", modifiers(false, true, true, false)));
    }
}
