#![allow(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::ptr::NonNull;

use evdev::{Device, EventSummary, KeyCode, RelativeAxisCode, SynchronizationCode};
use xkbcommon_dl::{self as xkb, keysyms};

use super::session::SeatDevice;
use crate::backend::BackendEvent;
use crate::{Key, KeyEvent, KeyKind, Mods, Mouse, MouseButton, MouseKind};

pub(super) enum InputAction {
    Event(BackendEvent),
    SwitchVt(u8),
}

pub(super) struct InputDevice {
    device: Option<Device>,
    owner: SeatDevice,
    high_resolution_x: bool,
    high_resolution_y: bool,
    pub keyboard: bool,
}

impl InputDevice {
    pub fn new(owner: SeatDevice) -> io::Result<Self> {
        let device = Device::from_fd(owner.as_fd().try_clone_to_owned()?)?;
        device.set_nonblocking(true)?;
        let keyboard = device
            .supported_keys()
            .is_some_and(|keys| keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_ENTER));
        let relative = device.supported_relative_axes();
        let pointer = relative.is_some_and(|axes| {
            axes.contains(RelativeAxisCode::REL_X) && axes.contains(RelativeAxisCode::REL_Y)
        });
        if !keyboard && !pointer {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "select a keyboard or relative mouse; absolute devices and touchpads are not supported",
            ));
        }
        let high_resolution_x =
            relative.is_some_and(|axes| axes.contains(RelativeAxisCode::REL_HWHEEL_HI_RES));
        let high_resolution_y =
            relative.is_some_and(|axes| axes.contains(RelativeAxisCode::REL_WHEEL_HI_RES));
        Ok(Self {
            device: Some(device),
            owner,
            high_resolution_x,
            high_resolution_y,
            keyboard,
        })
    }

    pub fn read(
        &mut self,
        input: &mut InputState,
        actions: &mut Vec<InputAction>,
    ) -> io::Result<()> {
        let Some(device) = &mut self.device else {
            return Ok(());
        };
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    input.event(
                        event.destructure(),
                        self.high_resolution_x,
                        self.high_resolution_y,
                        actions,
                    );
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }

    pub fn release(&mut self, seat: &mut libseat::SeatRef) -> io::Result<()> {
        self.device.take();
        self.owner.release(seat)
    }
}

impl AsFd for InputDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.device.as_ref().expect("input device is open").as_fd()
    }
}

struct Keyboard {
    api: &'static xkb::XkbCommon,
    context: NonNull<xkb::xkb_context>,
    keymap: NonNull<xkb::xkb_keymap>,
    state: NonNull<xkb::xkb_state>,
    pressed: BTreeMap<u16, Key>,
}

impl Keyboard {
    fn new() -> io::Result<Self> {
        Self::with_layout(None)
    }

    fn with_layout(layout: Option<&CStr>) -> io::Result<Self> {
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
                rules: std::ptr::null(),
                model: std::ptr::null(),
                layout: layout.map_or(std::ptr::null(), CStr::as_ptr),
                variant: std::ptr::null(),
                options: std::ptr::null(),
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
            Ok(Self {
                api,
                context,
                keymap,
                state,
                pressed: BTreeMap::new(),
            })
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
        unsafe {
            (self.api.xkb_state_unref)(self.state.as_ptr());
            (self.api.xkb_keymap_unref)(self.keymap.as_ptr());
            (self.api.xkb_context_unref)(self.context.as_ptr());
        }
    }
}

pub(super) struct InputState {
    keyboard: Keyboard,
    point: (u32, u32),
    size: (u32, u32),
    buttons: BTreeSet<u16>,
    moved: bool,
}

impl InputState {
    pub fn new(size: (u32, u32)) -> io::Result<Self> {
        Ok(Self {
            keyboard: Keyboard::new()?,
            point: (size.0 / 2, size.1 / 2),
            size,
            buttons: BTreeSet::new(),
            moved: false,
        })
    }

    pub fn point(&self) -> (u32, u32) {
        self.point
    }

    fn mouse(&self, kind: MouseKind, button: MouseButton) -> BackendEvent {
        BackendEvent::Mouse(Mouse {
            kind,
            button,
            mods: self.keyboard.mods(),
            x: self.point.0,
            y: self.point.1,
        })
    }

    fn event(
        &mut self,
        event: EventSummary,
        hires_x: bool,
        hires_y: bool,
        actions: &mut Vec<InputAction>,
    ) {
        let push = |actions: &mut Vec<InputAction>, event| actions.push(InputAction::Event(event));
        match event {
            EventSummary::Key(_, code, value) if code.0 < 0x100 => {
                let event = self.keyboard.key(code.0, value);
                if event.mods.ctrl
                    && event.mods.alt
                    && let Key::Function(vt @ 1..=12) = event.key
                {
                    if event.kind == KeyKind::Press {
                        actions.push(InputAction::SwitchVt(vt));
                    }
                } else {
                    push(actions, BackendEvent::Key(event));
                }
            }
            EventSummary::Key(
                _,
                code @ (KeyCode::BTN_LEFT | KeyCode::BTN_RIGHT | KeyCode::BTN_MIDDLE),
                value,
            ) if value != 2 => {
                if value == 0 {
                    self.buttons.remove(&code.0);
                } else {
                    self.buttons.insert(code.0);
                }
                push(
                    actions,
                    self.mouse(
                        if value == 0 {
                            MouseKind::Up
                        } else {
                            MouseKind::Down
                        },
                        mouse_button(code.0),
                    ),
                );
            }
            EventSummary::RelativeAxis(_, RelativeAxisCode::REL_X, value) => {
                self.point.0 = move_axis(self.point.0, value, self.size.0);
                self.moved = true;
            }
            EventSummary::RelativeAxis(_, RelativeAxisCode::REL_Y, value) => {
                self.point.1 = move_axis(self.point.1, value, self.size.1);
                self.moved = true;
            }
            EventSummary::RelativeAxis(_, axis, value) => {
                let delta = match axis {
                    RelativeAxisCode::REL_WHEEL_HI_RES => Some((0.0, -(value as f32) / 3.0)),
                    RelativeAxisCode::REL_HWHEEL_HI_RES => Some((value as f32 / 3.0, 0.0)),
                    RelativeAxisCode::REL_WHEEL if !hires_y => Some((0.0, -(value as f32) * 40.0)),
                    RelativeAxisCode::REL_HWHEEL if !hires_x => Some((value as f32 * 40.0, 0.0)),
                    _ => None,
                };
                if let Some((delta_x, delta_y)) = delta {
                    push(
                        actions,
                        BackendEvent::Wheel {
                            x: self.point.0,
                            y: self.point.1,
                            delta_x,
                            delta_y,
                            mods: self.keyboard.mods(),
                        },
                    );
                }
            }
            EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) if self.moved => {
                self.moved = false;
                let button = self
                    .buttons
                    .first()
                    .copied()
                    .map_or(MouseButton::None, mouse_button);
                push(actions, self.mouse(MouseKind::Move, button));
            }
            _ => {}
        }
    }
}

fn move_axis(position: u32, delta: i32, extent: u32) -> u32 {
    (i64::from(position) + i64::from(delta)).clamp(0, i64::from(extent.saturating_sub(1))) as u32
}

fn mouse_button(code: u16) -> MouseButton {
    match KeyCode(code) {
        KeyCode::BTN_LEFT => MouseButton::Left,
        KeyCode::BTN_RIGHT => MouseButton::Right,
        KeyCode::BTN_MIDDLE => MouseButton::Middle,
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
        assert_eq!(move_axis(5, -100, 101), 0);
        assert_eq!(move_axis(5, 500, 101), 100);
        assert_eq!(move_axis(5, 0, 101), 5);
    }

    #[test]
    fn function_and_navigation_keys_keep_pixel_key_identity() {
        assert_eq!(key_from_symbol(keysyms::F12, 0), Key::Function(12));
        assert_eq!(key_from_symbol(0x1008fe03, 0), Key::Function(3));
        assert_eq!(key_from_symbol(keysyms::Return, 13), Key::Enter);
        assert_eq!(key_from_symbol(0, 0xe9), Key::Char('é'));
    }

    #[test]
    fn pointer_batches_drag_at_pixel_coordinates_and_filter_duplicate_wheel_ticks() {
        let mut input = InputState {
            keyboard: Keyboard::with_layout(Some(c"us")).unwrap(),
            point: (10, 10),
            size: (101, 57),
            buttons: BTreeSet::new(),
            moved: false,
        };
        let mut actions = Vec::new();
        let events = [
            evdev::InputEvent::new(evdev::EventType::RELATIVE.0, RelativeAxisCode::REL_X.0, 8),
            evdev::InputEvent::new(evdev::EventType::RELATIVE.0, RelativeAxisCode::REL_Y.0, 5),
            evdev::InputEvent::new(evdev::EventType::KEY.0, KeyCode::BTN_LEFT.0, 1),
            evdev::InputEvent::new(
                evdev::EventType::SYNCHRONIZATION.0,
                SynchronizationCode::SYN_REPORT.0,
                0,
            ),
            evdev::InputEvent::new(
                evdev::EventType::RELATIVE.0,
                RelativeAxisCode::REL_WHEEL.0,
                1,
            ),
            evdev::InputEvent::new(
                evdev::EventType::RELATIVE.0,
                RelativeAxisCode::REL_WHEEL_HI_RES.0,
                120,
            ),
        ];
        for event in events {
            input.event(event.destructure(), false, true, &mut actions);
        }
        assert_eq!(input.point(), (18, 15));
        assert_eq!(actions.len(), 3);
        assert!(matches!(
            &actions[0],
            InputAction::Event(BackendEvent::Mouse(Mouse {
                kind: MouseKind::Down,
                x: 18,
                y: 15,
                button: MouseButton::Left,
                ..
            }))
        ));
        assert!(matches!(
            &actions[1],
            InputAction::Event(BackendEvent::Mouse(Mouse {
                kind: MouseKind::Move,
                x: 18,
                y: 15,
                button: MouseButton::Left,
                ..
            }))
        ));
        assert!(matches!(
            &actions[2],
            InputAction::Event(BackendEvent::Wheel { delta_y: -40.0, .. })
        ));
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
