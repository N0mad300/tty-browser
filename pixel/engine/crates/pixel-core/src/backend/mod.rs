use std::io;
use std::time::Duration;

use crate::canvas::Frame;
use crate::terminal::{Handoff, KeyEvent, Mods, Mouse, Terminal, TerminalColors};

mod kitty;

#[cfg(all(target_os = "linux", feature = "drm"))]
pub mod drm;

pub use kitty::{HostConfig, KittyBackend, KittyConfig};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayMetrics {
    pub width_px: u32,
    pub height_px: u32,
    pub scale_factor: f32,
}

impl DisplayMetrics {
    pub fn validate(self) -> io::Result<Self> {
        if self.width_px == 0
            || self.height_px == 0
            || !self.scale_factor.is_finite()
            || self.scale_factor <= 0.0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid display dimensions or scale",
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackendCapabilities {
    pub damage: bool,
    pub transparency: bool,
    pub pixel_pointer: bool,
    pub key_releases: bool,
    pub super_key: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BackendEvent {
    Key(KeyEvent),
    Mouse(Mouse),
    Wheel {
        x: u32,
        y: u32,
        delta_x: f32,
        delta_y: f32,
        mods: Mods,
    },
    Paste(String),
    Focus(bool),
    Resize,
    RedrawRequested,
    CloseRequested,
    HostClosed,
    Handoff(Handoff),
    ClipboardData {
        items: Vec<(String, Vec<u8>)>,
        ok: bool,
    },
    Colors(TerminalColors),
    Devtools,
    Visible(bool),
}

pub trait Backend {
    fn metrics(&mut self) -> io::Result<DisplayMetrics>;
    fn present(&mut self, frame: Frame<'_>) -> io::Result<usize>;
    fn poll_event(&mut self, timeout: Option<Duration>) -> io::Result<Option<BackendEvent>>;
    fn waker(&mut self) -> io::Result<Waker>;

    fn name(&self) -> &'static str {
        "custom"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
    }

    fn cell_size(&self) -> Option<(u32, u32)> {
        None
    }

    fn colors(&self) -> TerminalColors {
        TerminalColors::default()
    }

    fn terminal(&self) -> Option<&Terminal> {
        None
    }

    fn terminal_mut(&mut self) -> Option<&mut Terminal> {
        None
    }

    fn retarget(&mut self, _target: crate::Retarget) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this backend cannot retarget a terminal",
        ))
    }

    fn set_pointer_shape(&mut self, _shape: &str) -> io::Result<()> {
        Ok(())
    }

    fn set_title(&mut self, _text: &str) -> io::Result<()> {
        Ok(())
    }

    fn set_clipboard(&mut self, _text: &str) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn request_clipboard(&mut self) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct Waker {
    pub(crate) fd: std::sync::Arc<rustix::fd::OwnedFd>,
}

impl Waker {
    pub fn pipe() -> io::Result<(rustix::fd::OwnedFd, Self)> {
        let (rx, tx) = rustix::pipe::pipe()?;
        rustix::fs::fcntl_setfl(&rx, rustix::fs::OFlags::NONBLOCK)?;
        rustix::fs::fcntl_setfl(&tx, rustix::fs::OFlags::NONBLOCK)?;
        Ok((
            rx,
            Self {
                fd: std::sync::Arc::new(tx),
            },
        ))
    }

    pub fn wake(&self) {
        let _ = rustix::io::write(&*self.fd, &[1]);
    }
}
