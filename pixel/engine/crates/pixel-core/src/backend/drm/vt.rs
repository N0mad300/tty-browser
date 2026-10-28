#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

use super::session::{SessionInfo, session_info};
use crate::backend::Waker;

const VT_GETMODE: libc::c_ulong = 0x5601;
const VT_SETMODE: libc::c_ulong = 0x5602;
const VT_GETSTATE: libc::c_ulong = 0x5603;
const VT_RELDISP: libc::c_ulong = 0x5605;
const KDSETMODE: libc::c_ulong = 0x4b3a;
const KDGETMODE: libc::c_ulong = 0x4b3b;
const KDGKBMODE: libc::c_ulong = 0x4b44;
const KDSKBMODE: libc::c_ulong = 0x4b45;

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct VtMode {
    mode: u8,
    wait: u8,
    release_signal: i16,
    acquire_signal: i16,
    force_signal: i16,
}

#[derive(Default)]
#[repr(C)]
struct VtState {
    active: u16,
    signal: u16,
    state: u16,
}

pub(super) struct Console {
    tty: File,
    pub info: SessionInfo,
    previous_vt: VtMode,
    previous_graphics: i32,
    previous_keyboard: i32,
    previous_termios: rustix::termios::Termios,
    changed: bool,
}

impl Console {
    pub fn check() -> io::Result<Self> {
        if std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty())
            || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "DRM cannot start in a desktop terminal; log in on a separate Linux VT",
            ));
        }
        let tty = File::options()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("DRM requires a controlling Linux VT: {error}"),
                )
            })?;
        if unsafe { libc::tcgetpgrp(tty.as_raw_fd()) } != unsafe { libc::getpgrp() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "DRM must run in the foreground of the local VT",
            ));
        }
        let mut device: u32 = 0;
        ioctl_ptr(&tty, libc::TIOCGDEV as _, &mut device)?;
        let mut state = VtState::default();
        ioctl_ptr(&tty, VT_GETSTATE, &mut state)?;
        let info = session_info()?;
        let tty_vt = libc::minor(u64::from(device)) as u32;
        if libc::major(u64::from(device)) != 4
            || tty_vt != info.vt
            || u32::from(state.active) != info.vt
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the controlling TTY must be the active local session VT",
            ));
        }
        if std::env::var("XDG_SESSION_TYPE").is_ok_and(|value| value != "tty") {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "XDG_SESSION_TYPE must describe the local TTY session",
            ));
        }
        if std::env::var("XDG_SESSION_ID").is_ok_and(|value| value != info.session) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "XDG_SESSION_ID does not identify this process's logind session",
            ));
        }
        let mut previous_vt = VtMode::default();
        ioctl_ptr(&tty, VT_GETMODE, &mut previous_vt)?;
        let mut previous_graphics = 0;
        ioctl_ptr(&tty, KDGETMODE, &mut previous_graphics)?;
        let mut previous_keyboard = 0;
        ioctl_ptr(&tty, KDGKBMODE, &mut previous_keyboard)?;
        if previous_vt.mode != 0 || previous_graphics != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "this VT already has a graphics or session controller",
            ));
        }
        let previous_termios = rustix::termios::tcgetattr(&tty)?;
        Ok(Self {
            tty,
            info,
            previous_vt,
            previous_graphics,
            previous_keyboard,
            previous_termios,
            changed: false,
        })
    }

    pub fn activate(&mut self, signals: &Signals) -> io::Result<()> {
        self.ensure_active()?;
        let mut mode = VtMode {
            mode: 1,
            release_signal: signals.release_signal as i16,
            acquire_signal: signals.acquire_signal as i16,
            ..VtMode::default()
        };
        self.changed = true;
        ioctl_ptr(&self.tty, VT_SETMODE, &mut mode)?;
        ioctl_value(&self.tty, KDSKBMODE, 4)?;
        ioctl_value(&self.tty, KDSETMODE, 1)?;
        let mut raw = self.previous_termios.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(&self.tty, rustix::termios::OptionalActions::Now, &raw)?;
        Ok(())
    }

    pub fn ensure_active(&self) -> io::Result<()> {
        let mut state = VtState::default();
        ioctl_ptr(&self.tty, VT_GETSTATE, &mut state)?;
        if u32::from(state.active) != self.info.vt {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the DRM VT is no longer active",
            ));
        }
        Ok(())
    }

    pub fn acknowledge_release(&self) -> io::Result<()> {
        ioctl_value(&self.tty, VT_RELDISP, 1)
    }

    pub fn acknowledge_acquire(&self) -> io::Result<()> {
        ioctl_value(&self.tty, VT_RELDISP, 2)
    }

    pub fn restore(&mut self) -> io::Result<()> {
        if !self.changed {
            return Ok(());
        }
        self.changed = false;
        let results = [
            ioctl_value(&self.tty, KDSKBMODE, self.previous_keyboard),
            ioctl_value(&self.tty, KDSETMODE, self.previous_graphics),
            rustix::termios::tcsetattr(
                &self.tty,
                rustix::termios::OptionalActions::Now,
                &self.previous_termios,
            )
            .map_err(Into::into),
            ioctl_ptr(&self.tty, VT_SETMODE, &mut self.previous_vt),
        ];
        results
            .into_iter()
            .find_map(Result::err)
            .map_or(Ok(()), Err)
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            crate::logging::warn("drm", format!("console restoration failed: {error}"));
        }
    }
}

fn ioctl_ptr<T>(tty: &File, request: libc::c_ulong, value: &mut T) -> io::Result<()> {
    if unsafe { libc::ioctl(tty.as_raw_fd(), request as _, value) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn ioctl_value(tty: &File, request: libc::c_ulong, value: i32) -> io::Result<()> {
    if unsafe { libc::ioctl(tty.as_raw_fd(), request as _, value) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

static SIGNALS_IN_USE: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
pub(super) struct SignalFlags {
    pub close: AtomicBool,
    pub release: AtomicBool,
    pub acquire: AtomicBool,
}

pub(super) struct Signals {
    pub flags: Arc<SignalFlags>,
    registrations: Vec<signal_hook::SigId>,
    release_signal: i32,
    acquire_signal: i32,
    previous_mask: Option<libc::sigset_t>,
}

impl Signals {
    pub fn new(waker: Waker) -> io::Result<Self> {
        if SIGNALS_IN_USE.swap(true, Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "only one DRM backend can control process signals",
            ));
        }
        let mut signals = Self {
            flags: Arc::new(SignalFlags::default()),
            registrations: Vec::new(),
            release_signal: 0,
            acquire_signal: 0,
            previous_mask: None,
        };
        let mut available = Vec::new();
        for signal in libc::SIGRTMIN()..=libc::SIGRTMAX() {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            if unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) } == 0
                && action.sa_sigaction == libc::SIG_DFL
            {
                available.push(signal);
                if available.len() == 2 {
                    break;
                }
            }
        }
        if available.len() != 2 {
            return Err(io::Error::other(
                "DRM needs two unused real-time signals for VT switching",
            ));
        }
        let release_signal = available[0];
        let acquire_signal = available[1];
        signals.release_signal = release_signal;
        signals.acquire_signal = acquire_signal;
        for signal in [SIGINT, SIGTERM, SIGHUP, release_signal, acquire_signal] {
            let flags = signals.flags.clone();
            let waker = waker.clone();
            let id = unsafe {
                signal_hook::low_level::register(signal, move || {
                    let flag = if signal == release_signal {
                        &flags.release
                    } else if signal == acquire_signal {
                        &flags.acquire
                    } else {
                        &flags.close
                    };
                    flag.store(true, Ordering::Relaxed);
                    waker.wake();
                })
            }?;
            signals.registrations.push(id);
        }
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut previous_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, release_signal);
            libc::sigaddset(&mut mask, acquire_signal);
        }
        let result = unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &mask, &mut previous_mask) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        signals.previous_mask = Some(previous_mask);
        Ok(signals)
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        if let Some(mask) = self.previous_mask.take() {
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
            }
        }
        for id in self.registrations.drain(..) {
            signal_hook::low_level::unregister(id);
        }
        SIGNALS_IN_USE.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vt_signals_wake_the_owner_without_using_node_inspector_signals() {
        let (_read, waker) = Waker::pipe().unwrap();
        let signals = Signals::new(waker).unwrap();
        assert!(signals.release_signal >= libc::SIGRTMIN());
        assert!(signals.acquire_signal >= libc::SIGRTMIN());
        assert_ne!(signals.release_signal, signals.acquire_signal);
        assert_eq!(unsafe { libc::raise(signals.release_signal) }, 0);
        assert!(signals.flags.release.load(Ordering::Relaxed));
        assert_eq!(unsafe { libc::raise(signals.acquire_signal) }, 0);
        assert!(signals.flags.acquire.load(Ordering::Relaxed));
    }
}
