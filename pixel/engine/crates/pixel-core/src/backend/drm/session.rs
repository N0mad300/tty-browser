#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::rc::Rc;

use libseat::{Seat, SeatRef};

pub(super) type SharedSeat = Rc<RefCell<Seat>>;

pub(super) struct SeatDevice {
    seat: SharedSeat,
    token: Option<libseat::Device>,
    fd: Option<OwnedFd>,
}

impl SeatDevice {
    pub fn open(seat: &SharedSeat, path: &Path) -> io::Result<Self> {
        let token = seat.borrow_mut().open_device(&path).map_err(seat_error)?;
        // SAFETY: libseat gives us this descriptor and does not close it with the token.
        let fd = unsafe { OwnedFd::from_raw_fd(token.as_fd().as_raw_fd()) };
        Ok(Self {
            seat: seat.clone(),
            token: Some(token),
            fd: Some(fd),
        })
    }

    pub fn is_released(&self) -> bool {
        self.fd.is_none()
    }

    pub fn release(&mut self, seat: &mut SeatRef) -> io::Result<()> {
        let result = self
            .token
            .take()
            .map_or(Ok(()), |token| seat.close_device(token).map_err(seat_error));
        self.fd.take();
        result
    }
}

impl AsFd for SeatDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_ref().expect("seat device is open").as_fd()
    }
}

impl Drop for SeatDevice {
    fn drop(&mut self) {
        if let Some(token) = self.token.take()
            && let Err(error) = self.seat.borrow_mut().close_device(token)
        {
            crate::logging::warn("drm", format!("device release failed: {error}"));
        }
    }
}

pub(super) fn seat_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("seat access: {error}"))
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session: String,
    pub seat: String,
    pub vt: u32,
}

pub(super) fn session_info() -> io::Result<SessionInfo> {
    let library =
        unsafe { libloading::Library::new("libsystemd.so.0") }.map_err(io::Error::other)?;
    unsafe {
        let get_session = library
            .get::<unsafe extern "C" fn(libc::pid_t, *mut *mut c_char) -> i32>(
                b"sd_pid_get_session\0",
            )
            .map_err(io::Error::other)?;
        let get_type = library
            .get::<unsafe extern "C" fn(*const c_char, *mut *mut c_char) -> i32>(
                b"sd_session_get_type\0",
            )
            .map_err(io::Error::other)?;
        let get_seat = library
            .get::<unsafe extern "C" fn(*const c_char, *mut *mut c_char) -> i32>(
                b"sd_session_get_seat\0",
            )
            .map_err(io::Error::other)?;
        let get_vt = library
            .get::<unsafe extern "C" fn(*const c_char, *mut u32) -> i32>(b"sd_session_get_vt\0")
            .map_err(io::Error::other)?;
        let get_uid = library
            .get::<unsafe extern "C" fn(*const c_char, *mut libc::uid_t) -> i32>(
                b"sd_session_get_uid\0",
            )
            .map_err(io::Error::other)?;
        let is_active = library
            .get::<unsafe extern "C" fn(*const c_char) -> i32>(b"sd_session_is_active\0")
            .map_err(io::Error::other)?;
        let is_remote = library
            .get::<unsafe extern "C" fn(*const c_char) -> i32>(b"sd_session_is_remote\0")
            .map_err(io::Error::other)?;
        let mut name = std::ptr::null_mut();
        let result = get_session(0, &mut name);
        let session = login_string(result, name)?;
        let name = CString::new(session.as_str()).map_err(io::Error::other)?;
        let mut kind = std::ptr::null_mut();
        let result = get_type(name.as_ptr(), &mut kind);
        let kind = login_string(result, kind)?;
        let active = login_result(is_active(name.as_ptr()))? != 0;
        let remote = login_result(is_remote(name.as_ptr()))? != 0;
        let mut uid = 0;
        login_result(get_uid(name.as_ptr(), &mut uid))?;
        let mut vt = 0;
        login_result(get_vt(name.as_ptr(), &mut vt))?;
        validate_session(&kind, active, remote, vt, uid == libc::geteuid())?;
        let mut seat = std::ptr::null_mut();
        let result = get_seat(name.as_ptr(), &mut seat);
        let seat = login_string(result, seat)?;
        Ok(SessionInfo { session, seat, vt })
    }
}

fn login_result(result: i32) -> io::Result<i32> {
    if result < 0 {
        Err(io::Error::from_raw_os_error(-result))
    } else {
        Ok(result)
    }
}

unsafe fn login_string(result: i32, pointer: *mut c_char) -> io::Result<String> {
    login_result(result)?;
    if pointer.is_null() {
        return Err(io::Error::other("logind returned no session data"));
    }
    let value = unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned();
    unsafe { libc::free(pointer.cast()) };
    Ok(value)
}

fn validate_session(
    kind: &str,
    active: bool,
    remote: bool,
    vt: u32,
    owner_matches: bool,
) -> io::Result<()> {
    if kind != "tty" || !active || remote || vt == 0 || !owner_matches {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "DRM requires your own active local logind TTY session; log in on a separate Linux VT",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_owned_active_local_tty_session_is_accepted() {
        assert!(validate_session("tty", true, false, 3, true).is_ok());
        for (kind, active, remote, vt, owner) in [
            ("wayland", true, false, 3, true),
            ("x11", true, false, 3, true),
            ("tty", false, false, 3, true),
            ("tty", true, true, 3, true),
            ("tty", true, false, 0, true),
            ("tty", true, false, 3, false),
        ] {
            assert_eq!(
                validate_session(kind, active, remote, vt, owner)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }
}
