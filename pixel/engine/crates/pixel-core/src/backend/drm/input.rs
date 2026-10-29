#![allow(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::ptr::NonNull;

use evdev::{Device, EventSummary, KeyCode};
use std::path::Path;
use xkbcommon_dl::{self as xkb, keysyms};

use super::compose::{Compose, ComposedText};
use super::keymap::KeymapConfig;
use super::pointer::{PointerInput, PointerUpdate};
use super::session::SeatDevice;
use crate::backend::BackendEvent;
use crate::{Key, KeyEvent, KeyKind, Mods, Mouse, MouseButton, MouseKind};

pub(super) enum InputAction {
    Event(BackendEvent),
    SwitchVt(u8),
}

enum DeviceInput {
    Keyboard(Device),
    Pointer(PointerInput),
}

pub(super) struct InputDevice {
    device: Option<DeviceInput>,
    owner: SeatDevice,
    pub keyboard: bool,
}

impl InputDevice {
    pub fn new(owner: SeatDevice, path: &Path) -> io::Result<Self> {
        let device = Device::from_fd(owner.as_fd().try_clone_to_owned()?)?;
        let keyboard = device
            .supported_keys()
            .is_some_and(|keys| keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_ENTER));
        let device = if keyboard {
            device.set_nonblocking(true)?;
            DeviceInput::Keyboard(device)
        } else {
            drop(device);
            DeviceInput::Pointer(PointerInput::new(&owner, path)?)
        };
        Ok(Self {
            device: Some(device),
            owner,
            keyboard,
        })
    }

    pub fn read(
        &mut self,
        input: &mut InputState,
        actions: &mut Vec<InputAction>,
    ) -> io::Result<()> {
        match self.device.as_mut() {
            Some(DeviceInput::Pointer(pointer)) => pointer.read(input, actions),
            Some(DeviceInput::Keyboard(device)) => {
                match device.fetch_events() {
                    Ok(events) => {
                        for event in events {
                            if let EventSummary::Key(_, code, value) = event.destructure()
                                && code.0 < 0x100
                            {
                                input.key(code.0, value, actions);
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error),
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub fn release(&mut self, seat: &mut libseat::SeatRef) -> io::Result<()> {
        self.device.take();
        self.owner.release(seat)
    }
}

impl AsFd for InputDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self.device.as_ref().expect("input device is open") {
            DeviceInput::Keyboard(device) => device.as_fd(),
            DeviceInput::Pointer(pointer) => pointer.as_fd(),
        }
    }
}

struct Keyboard {
    api: &'static xkb::XkbCommon,
    context: NonNull<xkb::xkb_context>,
    keymap: NonNull<xkb::xkb_keymap>,
    state: NonNull<xkb::xkb_state>,
    pressed: BTreeMap<u16, Key>,
    compose: Option<Compose>,
}

impl Keyboard {
    fn new() -> io::Result<Self> {
        Self::with_config(&KeymapConfig::load()?)
    }

    #[cfg(test)]
    fn with_layout(layout: Option<&CStr>) -> io::Result<Self> {
        Self::with_config(&KeymapConfig {
            layout: layout.map(CStr::to_owned),
            ..KeymapConfig::default()
        })
    }

    fn with_config(config: &KeymapConfig) -> io::Result<Self> {
        let api = xkb::xkbcommon_option().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "libxkbcommon is required for DRM keyboard input",
            )
        })?;
        unsafe {
            let context = NonNull::new((api.xkb_context_new)(
                xkb::xkb_context_flags::XKB_CONTEXT_NO_FLAGS,
            ))
            .ok_or_else(|| io::Error::other("could not create XKB context"))?;
            let names = xkb::xkb_rule_names {
                rules: config
                    .rules
                    .as_deref()
                    .map_or(std::ptr::null(), CStr::as_ptr),
                model: config
                    .model
                    .as_deref()
                    .map_or(std::ptr::null(), CStr::as_ptr),
                layout: config
                    .layout
                    .as_deref()
                    .map_or(std::ptr::null(), CStr::as_ptr),
                variant: config
                    .variant
                    .as_deref()
                    .map_or(std::ptr::null(), CStr::as_ptr),
                options: config
                    .options
                    .as_deref()
                    .map_or(std::ptr::null(), CStr::as_ptr),
            };
            let keymap = NonNull::new((api.xkb_keymap_new_from_names)(
                context.as_ptr(),
                &names,
                xkb::xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            ));
            let Some(keymap) = keymap else {
                (api.xkb_context_unref)(context.as_ptr());
                return Err(io::Error::other("could not load the XKB keymap"));
            };
            let state = NonNull::new((api.xkb_state_new)(keymap.as_ptr()));
            let Some(state) = state else {
                (api.xkb_keymap_unref)(keymap.as_ptr());
                (api.xkb_context_unref)(context.as_ptr());
                return Err(io::Error::other("could not create XKB state"));
            };
            let mut keyboard = Self {
                api,
                context,
                keymap,
                state,
                pressed: BTreeMap::new(),
                compose: None,
            };
            keyboard.compose = Some(Compose::new(context)?);
            Ok(keyboard)
        }
    }

    fn mods(&self) -> Mods {
        let active = |name: &[u8]| unsafe {
            (self.api.xkb_state_mod_name_is_active)(
                self.state.as_ptr(),
                name.as_ptr().cast(),
                xkb::xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
            ) > 0
        };
        Mods {
            shift: active(xkb::XKB_MOD_NAME_SHIFT),
            ctrl: active(xkb::XKB_MOD_NAME_CTRL),
            alt: active(xkb::XKB_MOD_NAME_ALT),
            sup: active(xkb::XKB_MOD_NAME_LOGO),
        }
    }

    fn key(&mut self, code: u16, value: i32) -> KeyEvent {
        let keycode = u32::from(code) + 8;
        unsafe {
            if value != 2 {
                (self.api.xkb_state_update_key)(
                    self.state.as_ptr(),
                    keycode,
                    if value == 0 {
                        xkb::xkb_key_direction::XKB_KEY_UP
                    } else {
                        xkb::xkb_key_direction::XKB_KEY_DOWN
                    },
                );
            }
            let symbol = (self.api.xkb_state_key_get_one_sym)(self.state.as_ptr(), keycode);
            let mut key = key_from_symbol(symbol, (self.api.xkb_keysym_to_utf32)(symbol));
            if value == 0 {
                key = self.pressed.remove(&code).unwrap_or(key);
            } else if value == 1 {
                self.pressed.insert(code, key);
            }
            let mods = self.mods();
            let mut text = [0u8; 128];
            let length = (self.api.xkb_state_key_get_utf8)(
                self.state.as_ptr(),
                keycode,
                text.as_mut_ptr().cast(),
                text.len(),
            );
            let text = (value != 0
                && !mods.ctrl
                && !mods.alt
                && !mods.sup
                && length > 0
                && (length as usize) < text.len())
            .then(|| String::from_utf8_lossy(&text[..length as usize]).into_owned())
            .filter(|text| text.chars().all(|ch| !ch.is_control()));
            let text = if value != 0 && !mods.ctrl && !mods.alt && !mods.sup {
                match self
                    .compose
                    .as_mut()
                    .expect("keyboard compose state")
                    .feed(symbol)
                {
                    ComposedText::Unchanged => text,
                    ComposedText::Pending => None,
                    ComposedText::Text(text) => Some(text),
                }
            } else {
                None
            };
            KeyEvent {
                key,
                mods,
                kind: match value {
                    0 => KeyKind::Release,
                    2 => KeyKind::Repeat,
                    _ => KeyKind::Press,
                },
                text,
            }
        }
    }
}

impl Drop for Keyboard {
    fn drop(&mut self) {
        self.compose.take();
        unsafe {
            (self.api.xkb_state_unref)(self.state.as_ptr());
            (self.api.xkb_keymap_unref)(self.keymap.as_ptr());
            (self.api.xkb_context_unref)(self.context.as_ptr());
        }
    }
}

pub(super) struct InputState {
    keyboard: Keyboard,
    point: (f64, f64),
    size: (u32, u32),
    buttons: BTreeSet<u32>,
}

impl InputState {
    pub fn new(size: (u32, u32)) -> io::Result<Self> {
        Ok(Self {
            keyboard: Keyboard::new()?,
            point: (f64::from(size.0 / 2), f64::from(size.1 / 2)),
            size,
            buttons: BTreeSet::new(),
        })
    }

    pub fn point(&self) -> (u32, u32) {
        (self.point.0.round() as u32, self.point.1.round() as u32)
    }

    fn mouse(&self, kind: MouseKind, button: MouseButton) -> BackendEvent {
        let (x, y) = self.point();
        BackendEvent::Mouse(Mouse {
            kind,
            button,
            mods: self.keyboard.mods(),
            x,
            y,
        })
    }

    fn key(&mut self, code: u16, value: i32, actions: &mut Vec<InputAction>) {
        let event = self.keyboard.key(code, value);
        if event.mods.ctrl
            && event.mods.alt
            && let Key::Function(vt @ 1..=12) = event.key
        {
            if event.kind == KeyKind::Press {
                actions.push(InputAction::SwitchVt(vt));
            }
        } else {
            actions.push(InputAction::Event(BackendEvent::Key(event)));
        }
    }

    pub fn pointer(&mut self, update: PointerUpdate, actions: &mut Vec<InputAction>) {
        let event = match update {
            PointerUpdate::Motion(dx, dy) => {
                let previous = self.point();
                self.point.0 = move_axis(self.point.0, dx, self.size.0);
                self.point.1 = move_axis(self.point.1, dy, self.size.1);
                if self.point() == previous {
                    return;
                }
                let button = self
                    .buttons
                    .first()
                    .copied()
                    .map_or(MouseButton::None, mouse_button);
                self.mouse(MouseKind::Move, button)
            }
            PointerUpdate::Button(code, pressed) => {
                let button = mouse_button(code);
                if button == MouseButton::None {
                    return;
                }
                if pressed {
                    self.buttons.insert(code);
                } else {
                    self.buttons.remove(&code);
                }
                self.mouse(
                    if pressed {
                        MouseKind::Down
                    } else {
                        MouseKind::Up
                    },
                    button,
                )
            }
            PointerUpdate::Scroll(dx, dy) => {
                if !dx.is_finite() || !dy.is_finite() || (dx == 0.0 && dy == 0.0) {
                    return;
                }
                let (x, y) = self.point();
                BackendEvent::Wheel {
                    x,
                    y,
                    delta_x: dx as f32,
                    delta_y: dy as f32,
                    mods: self.keyboard.mods(),
                }
            }
        };
        actions.push(InputAction::Event(event));
    }
}

fn move_axis(position: f64, delta: f64, extent: u32) -> f64 {
    if !delta.is_finite() {
        return position;
    }
    (position + delta).clamp(0.0, f64::from(extent.saturating_sub(1)))
}

fn mouse_button(code: u32) -> MouseButton {
    match code {
        0x110 => MouseButton::Left,
        0x111 => MouseButton::Right,
        0x112 => MouseButton::Middle,
        _ => MouseButton::None,
    }
}

fn key_from_symbol(symbol: u32, unicode: u32) -> Key {
    match symbol {
        keysyms::Return | keysyms::KP_Enter => Key::Enter,
        keysyms::Escape => Key::Escape,
        keysyms::BackSpace => Key::Backspace,
        keysyms::Tab | keysyms::ISO_Left_Tab => Key::Tab,
        keysyms::Up => Key::Up,
        keysyms::Down => Key::Down,
        keysyms::Left => Key::Left,
        keysyms::Right => Key::Right,
        keysyms::Home => Key::Home,
        keysyms::End => Key::End,
        keysyms::Insert => Key::Insert,
        keysyms::Delete => Key::Delete,
        keysyms::Page_Up => Key::PageUp,
        keysyms::Page_Down => Key::PageDown,
        keysyms::Shift_L => Key::LeftShift,
        keysyms::Shift_R => Key::RightShift,
        keysyms::Control_L => Key::LeftControl,
        keysyms::Control_R => Key::RightControl,
        keysyms::Alt_L => Key::LeftAlt,
        keysyms::Alt_R => Key::RightAlt,
        keysyms::Super_L | keysyms::Meta_L => Key::LeftSuper,
        keysyms::Super_R | keysyms::Meta_R => Key::RightSuper,
        keysyms::F1..=keysyms::F35 => Key::Function((symbol - keysyms::F1 + 1) as u8),
        0x1008fe01..=0x1008fe0c => Key::Function((symbol - 0x1008fe01 + 1) as u8),
        _ => char::from_u32(unicode)
            .filter(|ch| !ch.is_control())
            .map_or(Key::Unknown, Key::Char),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_pointer_stays_inside_physical_pixels() {
        assert_eq!(move_axis(5.0, -100.0, 101), 0.0);
        assert_eq!(move_axis(5.0, 500.0, 101), 100.0);
        assert_eq!(move_axis(5.0, 0.0, 101), 5.0);
    }

    #[test]
    fn function_and_navigation_keys_keep_pixel_key_identity() {
        assert_eq!(key_from_symbol(keysyms::F12, 0), Key::Function(12));
        assert_eq!(key_from_symbol(0x1008fe03, 0), Key::Function(3));
        assert_eq!(key_from_symbol(keysyms::Return, 13), Key::Enter);
        assert_eq!(key_from_symbol(0, 0xe9), Key::Char('é'));
    }

    fn input() -> InputState {
        InputState {
            keyboard: Keyboard::with_layout(Some(c"us")).unwrap(),
            point: (10.0, 10.0),
            size: (101, 57),
            buttons: BTreeSet::new(),
        }
    }

    #[test]
    fn touchpad_motion_preserves_fractions_drag_and_scrolling_at_pixel_coordinates() {
        let mut input = input();
        let mut actions = Vec::new();
        input.pointer(PointerUpdate::Motion(0.2, 0.1), &mut actions);
        assert!(actions.is_empty());
        input.pointer(PointerUpdate::Motion(0.4, 0.5), &mut actions);
        assert_eq!(input.point(), (11, 11));
        input.pointer(PointerUpdate::Button(0x110, true), &mut actions);
        input.pointer(PointerUpdate::Motion(7.0, 4.0), &mut actions);
        input.pointer(PointerUpdate::Scroll(2.5, 15.0), &mut actions);
        input.pointer(PointerUpdate::Button(0x110, false), &mut actions);
        assert_eq!(actions.len(), 5);
        assert!(matches!(
            &actions[2],
            InputAction::Event(BackendEvent::Mouse(Mouse {
                kind: MouseKind::Move,
                x: 18,
                y: 15,
                button: MouseButton::Left,
                ..
            }))
        ));
        assert!(matches!(
            &actions[3],
            InputAction::Event(BackendEvent::Wheel {
                x: 18,
                y: 15,
                delta_x: 2.5,
                delta_y: 15.0,
                ..
            })
        ));
        assert!(input.buttons.is_empty());
        input.pointer(PointerUpdate::Motion(-500.0, 500.0), &mut actions);
        assert_eq!(input.point(), (0, 56));
        input.pointer(PointerUpdate::Scroll(0.0, 0.0), &mut actions);
        input.pointer(PointerUpdate::Motion(f64::NAN, f64::INFINITY), &mut actions);
        input.pointer(PointerUpdate::Button(0x150, true), &mut actions);
        assert_eq!(actions.len(), 6);
    }

    #[test]
    fn french_layout_produces_azerty_accents_altgr_and_matching_releases() {
        let mut keyboard = Keyboard::with_layout(Some(c"fr")).unwrap();
        let key = keyboard.key(KeyCode::KEY_Q.0, 1);
        assert_eq!(key.key, Key::Char('a'));
        assert_eq!(key.text.as_deref(), Some("a"));
        assert_eq!(keyboard.key(KeyCode::KEY_Q.0, 0).key, Key::Char('a'));
        assert_eq!(keyboard.key(KeyCode::KEY_2.0, 1).text.as_deref(), Some("é"));
        keyboard.key(KeyCode::KEY_2.0, 0);
        keyboard.key(KeyCode::KEY_RIGHTALT.0, 1);
        assert_eq!(keyboard.key(KeyCode::KEY_0.0, 1).text.as_deref(), Some("@"));
        keyboard.key(KeyCode::KEY_0.0, 0);
        keyboard.key(KeyCode::KEY_RIGHTALT.0, 0);
        keyboard.key(KeyCode::KEY_LEFTCTRL.0, 1);
        let key = keyboard.key(KeyCode::KEY_C.0, 1);
        assert!(key.mods.ctrl);
        assert!(key.text.is_none());
    }

    #[test]
    fn french_dead_keys_compose_once_and_cancel_cleanly() {
        let mut keyboard = Keyboard::with_layout(Some(c"fr")).unwrap();
        assert!(keyboard.key(KeyCode::KEY_LEFTBRACE.0, 1).text.is_none());
        keyboard.key(KeyCode::KEY_LEFTBRACE.0, 0);
        assert_eq!(keyboard.key(KeyCode::KEY_E.0, 1).text.as_deref(), Some("ê"));
        assert!(keyboard.key(KeyCode::KEY_E.0, 0).text.is_none());
        assert_eq!(keyboard.key(KeyCode::KEY_E.0, 1).text.as_deref(), Some("e"));
        keyboard.key(KeyCode::KEY_E.0, 0);
        keyboard.key(KeyCode::KEY_LEFTBRACE.0, 1);
        keyboard.key(KeyCode::KEY_LEFTBRACE.0, 0);
        keyboard.key(KeyCode::KEY_ESC.0, 1);
        keyboard.key(KeyCode::KEY_ESC.0, 0);
        assert_eq!(keyboard.key(KeyCode::KEY_E.0, 1).text.as_deref(), Some("e"));
    }

    #[test]
    fn vt_shortcut_is_consumed_and_pointer_uses_keyboard_modifiers() {
        let mut input = input();
        let mut actions = Vec::new();
        input.key(KeyCode::KEY_LEFTCTRL.0, 1, &mut actions);
        input.key(KeyCode::KEY_LEFTALT.0, 1, &mut actions);
        actions.clear();
        input.key(KeyCode::KEY_F3.0, 1, &mut actions);
        input.key(KeyCode::KEY_F3.0, 0, &mut actions);
        assert!(matches!(&actions[..], [InputAction::SwitchVt(3)]));
        input.pointer(PointerUpdate::Button(0x110, true), &mut actions);
        assert!(
            matches!(&actions[1], InputAction::Event(BackendEvent::Mouse(Mouse { mods, .. })) if mods.ctrl && mods.alt)
        );
    }

    #[test]
    fn xkb_modifiers_and_release_identity_survive_shift_changes() {
        let mut keyboard = Keyboard::with_layout(Some(c"us")).unwrap();
        assert_eq!(
            keyboard.key(KeyCode::KEY_LEFTSHIFT.0, 1).kind,
            KeyKind::Press
        );
        let letter = keyboard.key(KeyCode::KEY_A.0, 1);
        assert!(letter.mods.shift);
        assert_eq!(letter.key, Key::Char('A'));
        assert_eq!(letter.text.as_deref(), Some("A"));
        keyboard.key(KeyCode::KEY_LEFTSHIFT.0, 0);
        assert_eq!(keyboard.key(KeyCode::KEY_A.0, 0).key, Key::Char('A'));
        keyboard.key(KeyCode::KEY_LEFTCTRL.0, 1);
        let letter = keyboard.key(KeyCode::KEY_C.0, 1);
        assert!(letter.mods.ctrl);
        assert_eq!(letter.key, Key::Char('c'));
        assert!(letter.text.is_none());
    }
}
