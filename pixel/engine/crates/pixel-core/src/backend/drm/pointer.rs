use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use ::input::event::DeviceEvent;
use ::input::event::pointer::{Axis, ButtonState, PointerEvent, PointerScrollEvent};
use ::input::{DeviceCapability, Event, Libinput, LibinputInterface};

use super::input::{InputAction, InputState};
use super::session::SeatDevice;

pub(super) enum PointerUpdate {
    Motion(f64, f64),
    Button(u32, bool),
    Scroll(f64, f64),
}

struct RestrictedDevice {
    path: PathBuf,
    fd: OwnedFd,
}

impl LibinputInterface for RestrictedDevice {
    fn open_restricted(&mut self, path: &Path, _flags: i32) -> Result<OwnedFd, i32> {
        if path != self.path {
            return Err(libc::EACCES);
        }
        self.fd
            .try_clone()
            .map_err(|error| error.raw_os_error().unwrap_or(libc::EIO))
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(fd);
    }
}

pub(super) struct PointerInput {
    context: Libinput,
}

impl PointerInput {
    pub fn new(owner: &SeatDevice, path: &Path) -> io::Result<Self> {
        let fd = owner.as_fd().try_clone_to_owned()?;
        rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK)?;
        let mut context = Libinput::new_from_path(RestrictedDevice {
            path: path.into(),
            fd,
        });
        let path = path
            .to_str()
            .ok_or_else(|| io::Error::other("input path is not UTF-8"))?;
        let mut device = context.path_add_device(path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "libinput could not initialize this device",
            )
        })?;
        if !device.has_capability(DeviceCapability::Pointer) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "select a mouse or touchpad with pointer capability",
            ));
        }
        if device.config_tap_finger_count() > 0 {
            device.config_tap_set_enabled(true).map_err(|error| {
                io::Error::other(format!("could not enable touchpad tapping: {error:?}"))
            })?;
        }
        Ok(Self { context })
    }

    pub fn read(
        &mut self,
        state: &mut InputState,
        actions: &mut Vec<InputAction>,
    ) -> io::Result<()> {
        self.context.dispatch()?;
        for event in &mut self.context {
            let event = match event {
                Event::Pointer(event) => event,
                Event::Device(DeviceEvent::Removed(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "pointer device disconnected",
                    ));
                }
                _ => continue,
            };
            let update = match event {
                PointerEvent::Motion(event) => PointerUpdate::Motion(event.dx(), event.dy()),
                PointerEvent::Button(event) => PointerUpdate::Button(
                    event.button(),
                    event.button_state() == ButtonState::Pressed,
                ),
                PointerEvent::ScrollWheel(event) => PointerUpdate::Scroll(
                    if event.has_axis(Axis::Horizontal) {
                        wheel_delta(event.scroll_value_v120(Axis::Horizontal))
                    } else {
                        0.0
                    },
                    if event.has_axis(Axis::Vertical) {
                        wheel_delta(event.scroll_value_v120(Axis::Vertical))
                    } else {
                        0.0
                    },
                ),
                PointerEvent::ScrollFinger(event) => PointerUpdate::Scroll(
                    event.scroll_value(Axis::Horizontal),
                    event.scroll_value(Axis::Vertical),
                ),
                PointerEvent::ScrollContinuous(event) => PointerUpdate::Scroll(
                    event.scroll_value(Axis::Horizontal),
                    event.scroll_value(Axis::Vertical),
                ),
                _ => continue,
            };
            state.pointer(update, actions);
        }
        Ok(())
    }
}

impl AsFd for PointerInput {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.context.as_fd()
    }
}

impl Drop for PointerInput {
    fn drop(&mut self) {
        self.context.suspend();
    }
}

fn wheel_delta(value: f64) -> f64 {
    value / 120.0 * 40.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_interface_only_duplicates_the_explicit_device_and_closes_its_copies() {
        let (read, _write) = rustix::pipe::pipe().unwrap();
        let mut interface = RestrictedDevice {
            path: "/dev/input/event4".into(),
            fd: read,
        };
        assert_eq!(
            interface
                .open_restricted(Path::new("/dev/input/event5"), 0)
                .unwrap_err(),
            libc::EACCES
        );
        let fd = interface
            .open_restricted(Path::new("/dev/input/event4"), 0)
            .unwrap();
        interface.close_restricted(fd);
        assert!(
            interface
                .open_restricted(Path::new("/dev/input/event4"), 0)
                .is_ok()
        );
    }

    #[test]
    fn wheel_units_preserve_partial_detents() {
        assert_eq!(wheel_delta(120.0), 40.0);
        assert_eq!(wheel_delta(-60.0), -20.0);
    }
}
