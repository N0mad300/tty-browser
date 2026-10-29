mod compose;
mod cursor;
mod cursor_settings;
mod display;
mod input;
mod keymap;
mod pointer;
mod session;
mod vt;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use display::Display;
use input::{InputAction, InputDevice, InputState};
use session::{SeatDevice, SharedSeat, seat_error};
use vt::{Console, Signals};

use super::{Backend, BackendCapabilities, BackendEvent, DisplayMetrics, Waker};
use crate::Frame;

pub use session::SessionInfo;

#[derive(Debug, Clone)]
pub struct DrmConfig {
    pub card: PathBuf,
    pub connector: Option<String>,
    pub input_devices: Vec<PathBuf>,
    pub scale_factor: f32,
}

impl DrmConfig {
    fn validate(&mut self) -> io::Result<()> {
        validate_scale(self.scale_factor)?;
        self.card = validate_device(&self.card, true)?;
        if self.input_devices.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "select a keyboard with --input, and optionally a relative mouse",
            ));
        }
        for device in &mut self.input_devices {
            *device = validate_device(device, false)?;
        }
        self.input_devices.sort();
        self.input_devices.dedup();
        Ok(())
    }
}

struct Resources {
    display: Display,
    devices: Vec<InputDevice>,
    input: InputState,
}

impl Resources {
    fn open(config: &DrmConfig, seat: &SharedSeat) -> io::Result<Self> {
        let mut display = Display::open(
            SeatDevice::open(seat, &config.card)?,
            config.connector.as_deref(),
            config.scale_factor,
        )?;
        let metrics = display.metrics();
        let mut devices = Vec::new();
        for path in &config.input_devices {
            let device = SeatDevice::open(seat, path)
                .and_then(|owner| InputDevice::new(owner, path))
                .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
            devices.push(device);
        }
        if devices.iter().filter(|device| device.keyboard).count() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "select exactly one keyboard, and optionally mice or touchpads",
            ));
        }
        let input = InputState::new((metrics.width_px, metrics.height_px))?;
        display.set_cursor(input.point())?;
        Ok(Self {
            display,
            devices,
            input,
        })
    }

    fn release(mut self, seat: &mut libseat::SeatRef) -> io::Result<()> {
        let mut error = self.display.release(seat).err();
        for device in &mut self.devices {
            if let Err(next) = device.release(seat) {
                error.get_or_insert(next);
            }
        }
        error.map_or(Ok(()), Err)
    }
}

type ResourceSlot = Rc<RefCell<Option<Resources>>>;

pub struct DrmBackend {
    config: DrmConfig,
    console: Console,
    signals: Signals,
    seat: SharedSeat,
    seat_poll: OwnedFd,
    enabled: Rc<Cell<bool>>,
    callback_error: Rc<RefCell<Option<io::Error>>>,
    resources: ResourceSlot,
    wake_rx: OwnedFd,
    waker: Waker,
    metrics: DisplayMetrics,
    events: VecDeque<BackendEvent>,
    vt_acquired: bool,
    visible: bool,
    closed: bool,
    clipboard: String,
    pointer_shape: String,
}

impl DrmBackend {
    pub fn check_session() -> io::Result<SessionInfo> {
        Ok(Console::check()?.info.clone())
    }

    pub fn new(mut config: DrmConfig) -> io::Result<Self> {
        let console = Console::check()?;
        if std::env::var("LIBSEAT_BACKEND").as_deref() != Ok("logind") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this DRM prototype requires LIBSEAT_BACKEND=logind",
            ));
        }
        config.validate()?;
        let (wake_rx, waker) = Waker::pipe()?;
        let signals = Signals::new(waker.clone())?;
        let enabled = Rc::new(Cell::new(false));
        let callback_error = Rc::new(RefCell::new(None));
        let resources: ResourceSlot = Rc::new(RefCell::new(None));
        let seat = {
            let enabled = enabled.clone();
            let callback_error = callback_error.clone();
            let resources = resources.clone();
            libseat::Seat::open(move |seat, event| match event {
                libseat::SeatEvent::Enable => enabled.set(true),
                libseat::SeatEvent::Disable => {
                    enabled.set(false);
                    let current = resources.borrow_mut().take();
                    if let Some(current) = current
                        && let Err(error) = current.release(seat)
                    {
                        *callback_error.borrow_mut() = Some(error);
                    }
                    if let Err(error) = seat.disable() {
                        *callback_error.borrow_mut() = Some(seat_error(error));
                    }
                }
            })
            .map_err(seat_error)?
        };
        let seat = Rc::new(RefCell::new(seat));
        if seat.borrow_mut().name() != console.info.seat {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "libseat selected a different seat from this VT session",
            ));
        }
        let seat_poll = seat
            .borrow_mut()
            .get_fd()
            .map_err(seat_error)?
            .try_clone_to_owned()?;
        let mut backend = Self {
            config,
            console,
            signals,
            seat,
            seat_poll,
            enabled,
            callback_error,
            resources,
            wake_rx,
            waker,
            metrics: DisplayMetrics {
                width_px: 0,
                height_px: 0,
                scale_factor: 1.0,
            },
            events: VecDeque::new(),
            vt_acquired: true,
            visible: false,
            closed: false,
            clipboard: String::new(),
            pointer_shape: "default".into(),
        };
        backend.seat.borrow_mut().dispatch(0).map_err(seat_error)?;
        if !backend.enabled.get() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the logind seat is not enabled",
            ));
        }
        backend.console.activate(&backend.signals)?;
        backend.service()?;
        if backend.resources.borrow().is_none() {
            return Err(io::Error::other(
                "the VT lost access during DRM initialization",
            ));
        }
        backend.events.clear();
        Ok(backend)
    }

    fn release_resources(&mut self) -> io::Result<()> {
        let current = self.resources.borrow_mut().take();
        if let Some(current) = current {
            current.release(&mut self.seat.borrow_mut())
        } else {
            Ok(())
        }
    }

    fn service(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let flags = &self.signals.flags;
        if flags.close.swap(false, Ordering::Relaxed) {
            self.events.push_back(BackendEvent::CloseRequested);
            return Ok(());
        }
        let release = flags.release.swap(false, Ordering::Relaxed);
        let acquire = flags.acquire.swap(false, Ordering::Relaxed);
        if release {
            self.vt_acquired = false;
            self.enabled.set(false);
            let cleanup = self.release_resources();
            let acknowledge = self.console.acknowledge_release();
            cleanup.and(acknowledge)?;
        }
        if acquire {
            self.console.acknowledge_acquire()?;
            self.vt_acquired = true;
        }
        self.seat.borrow_mut().dispatch(0).map_err(seat_error)?;
        if let Some(error) = self.callback_error.borrow_mut().take() {
            return Err(error);
        }
        let active = self.enabled.get() && self.vt_acquired;
        if active && self.resources.borrow().is_none() {
            self.console.ensure_active()?;
            let mut current = Resources::open(&self.config, &self.seat)?;
            current.display.set_pointer_shape(&self.pointer_shape)?;
            self.metrics = current.display.metrics();
            *self.resources.borrow_mut() = Some(current);
            if !self.enabled.get() {
                self.release_resources()?;
                return Err(io::Error::other(
                    "the DRM seat was revoked while opening devices",
                ));
            }
            self.events.push_back(BackendEvent::Resize);
            self.events.push_back(BackendEvent::RedrawRequested);
        }
        if active != self.visible {
            self.visible = active;
            self.events.push_back(BackendEvent::Focus(active));
            self.events.push_back(BackendEvent::Visible(active));
        }
        Ok(())
    }

    fn poll_devices(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let (card_ready, input_ready, disconnected) = {
            let resources = self.resources.borrow();
            let mut descriptors = vec![
                rustix::event::PollFd::new(&self.wake_rx, rustix::event::PollFlags::IN),
                rustix::event::PollFd::new(&self.seat_poll, rustix::event::PollFlags::IN),
            ];
            if let Some(current) = resources.as_ref() {
                descriptors.push(rustix::event::PollFd::new(
                    &current.display,
                    rustix::event::PollFlags::IN,
                ));
                for device in &current.devices {
                    descriptors.push(rustix::event::PollFd::new(
                        device,
                        rustix::event::PollFlags::IN,
                    ));
                }
            }
            let deadline = resources
                .as_ref()
                .and_then(|current| current.display.deadline());
            let timeout = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    Some(timeout.map_or(remaining, |wait| wait.min(remaining)))
                }
                None => timeout,
            };
            let timeout = timeout
                .map(rustix::event::Timespec::try_from)
                .transpose()
                .map_err(io::Error::other)?;
            match rustix::event::poll(&mut descriptors, timeout.as_ref()) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
            (
                descriptors
                    .get(2)
                    .is_some_and(|fd| fd.revents().contains(rustix::event::PollFlags::IN)),
                descriptors
                    .iter()
                    .skip(3)
                    .map(|fd| fd.revents().contains(rustix::event::PollFlags::IN))
                    .collect::<Vec<_>>(),
                descriptors.iter().any(|fd| {
                    fd.revents().intersects(
                        rustix::event::PollFlags::ERR
                            | rustix::event::PollFlags::HUP
                            | rustix::event::PollFlags::NVAL,
                    )
                }),
            )
        };
        let mut bytes = [0; 64];
        while rustix::io::read(&self.wake_rx, &mut bytes).is_ok_and(|count| count > 0) {}
        self.service()?;
        if !self.events.is_empty() {
            return Ok(());
        }
        if disconnected {
            return Err(io::Error::other(
                "a DRM, session or input device disconnected",
            ));
        }
        let mut actions = Vec::new();
        {
            let mut resources = self.resources.borrow_mut();
            if let Some(current) = resources.as_mut() {
                if card_ready {
                    current.display.receive_events()?;
                }
                for (device, ready) in current.devices.iter_mut().zip(input_ready) {
                    if ready {
                        device.read(&mut current.input, &mut actions)?;
                    }
                }
                current.display.check_timeout()?;
                current.display.set_cursor(current.input.point())?;
            }
        }
        for action in actions {
            match action {
                InputAction::Event(event) => self.events.push_back(event),
                InputAction::SwitchVt(vt) => self
                    .seat
                    .borrow_mut()
                    .switch_session(vt.into())
                    .map_err(seat_error)?,
            }
        }
        self.service()
    }
}

impl Backend for DrmBackend {
    fn metrics(&mut self) -> io::Result<DisplayMetrics> {
        Ok(self.metrics)
    }

    fn present(&mut self, frame: Frame<'_>) -> io::Result<usize> {
        self.service()?;
        if self
            .events
            .iter()
            .any(|event| matches!(event, BackendEvent::CloseRequested | BackendEvent::Resize))
        {
            return Ok(0);
        }
        match self.resources.borrow_mut().as_mut() {
            Some(current) => current.display.present(frame),
            None => Ok(0),
        }
    }

    fn poll_event(&mut self, timeout: Option<Duration>) -> io::Result<Option<BackendEvent>> {
        self.service()?;
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        if self.closed {
            return Ok(None);
        }
        self.poll_devices(timeout)?;
        Ok(self.events.pop_front())
    }

    fn waker(&mut self) -> io::Result<Waker> {
        Ok(self.waker.clone())
    }

    fn name(&self) -> &'static str {
        "drm"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            damage: false,
            transparency: false,
            pixel_pointer: true,
            key_releases: true,
            super_key: true,
        }
    }

    fn set_clipboard(&mut self, text: &str) -> io::Result<()> {
        self.clipboard = text.to_owned();
        Ok(())
    }

    fn set_pointer_shape(&mut self, shape: &str) -> io::Result<()> {
        self.pointer_shape = shape.into();
        if let Some(current) = self.resources.borrow_mut().as_mut() {
            current.display.set_pointer_shape(shape)?;
        }
        Ok(())
    }

    fn request_clipboard(&mut self) -> io::Result<()> {
        self.events
            .push_back(BackendEvent::Paste(self.clipboard.clone()));
        Ok(())
    }

    fn shutdown(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let release = self.release_resources();
        let acknowledge = if self.signals.flags.release.swap(false, Ordering::Relaxed) {
            self.console.acknowledge_release()
        } else if self.signals.flags.acquire.swap(false, Ordering::Relaxed) {
            self.console.acknowledge_acquire()
        } else {
            Ok(())
        };
        let restore = self.console.restore();
        release.and(acknowledge).and(restore)
    }
}

impl Drop for DrmBackend {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            crate::logging::warn("drm", format!("DRM shutdown failed: {error}"));
        }
    }
}

fn validate_scale(scale: f32) -> io::Result<()> {
    if !scale.is_finite() || scale <= 0.0 || scale > 4.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DRM scale must be finite and greater than zero, up to 4",
        ));
    }
    Ok(())
}

fn validate_device(path: &std::path::Path, card: bool) -> io::Result<PathBuf> {
    let path = path.canonicalize()?;
    let metadata = path.metadata()?;
    let device = metadata.rdev();
    let allowed = if card {
        path.parent() == Some(std::path::Path::new("/dev/dri"))
            && libc::major(device) == 226
            && libc::minor(device) < 128
    } else {
        path.parent() == Some(std::path::Path::new("/dev/input"))
            && libc::major(device) == 13
            && libc::minor(device) >= 64
    };
    if !metadata.file_type().is_char_device() || !allowed {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "select a DRM primary card or evdev input device",
        ));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_scale_and_regular_files_are_rejected() {
        for scale in [0.0, -1.0, f32::NAN, f32::INFINITY, 5.0] {
            assert!(validate_scale(scale).is_err());
        }
        assert!(validate_scale(1.5).is_ok());
        assert!(validate_device(std::path::Path::new("/proc/self/stat"), true).is_err());
        assert!(validate_device(std::path::Path::new("/dev/null"), false).is_err());
    }
}
