use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{self, Device as _, connector, crtc, dumbbuffer::DumbBuffer, framebuffer};

use super::cursor::Cursor;
use super::session::SeatDevice;
use crate::Frame;
use crate::backend::DisplayMetrics;

struct Card(SeatDevice);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl control::Device for Card {}

struct ScanoutBuffer {
    dumb: DumbBuffer,
    framebuffer: framebuffer::Handle,
}

#[derive(Clone)]
struct Snapshot {
    pixels: Arc<[u8]>,
    premultiplied: bool,
}

pub(super) struct Display {
    card: Card,
    connector: connector::Handle,
    mode: control::Mode,
    previous: crtc::Info,
    previous_connectors: Vec<connector::Handle>,
    buffers: Vec<ScanoutBuffer>,
    front: usize,
    pending_flip: Option<usize>,
    pending_at: Option<Instant>,
    installed: bool,
    latest: Option<Snapshot>,
    queued: bool,
    cursor: (u32, u32),
    cursor_theme: Cursor,
    scale: f32,
}

impl Display {
    pub fn open(device: SeatDevice, requested: Option<&str>, scale: f32) -> io::Result<Self> {
        let card = Card(device);
        let flags = rustix::fs::fcntl_getfl(&card)?;
        rustix::fs::fcntl_setfl(&card, flags | rustix::fs::OFlags::NONBLOCK)?;
        let resources = card.resource_handles()?;
        let connectors = resources
            .connectors()
            .iter()
            .map(|&handle| card.get_connector(handle, true))
            .collect::<io::Result<Vec<_>>>()?;
        let connected: Vec<_> = connectors
            .iter()
            .filter(|info| info.state() == connector::State::Connected && !info.modes().is_empty())
            .collect();
        let selected = match requested {
            Some(name) => connected
                .iter()
                .copied()
                .find(|info| connector_name(info) == name)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("connected DRM connector {name} was not found"),
                    )
                })?,
            None if connected.len() == 1 => connected[0],
            None => {
                return Err(io::Error::other(format!(
                    "select one connected output with --connector; available: {}",
                    connected
                        .iter()
                        .map(|info| connector_name(info))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        };
        let encoders = selected
            .encoders()
            .iter()
            .map(|&handle| card.get_encoder(handle))
            .collect::<io::Result<Vec<_>>>()?;
        let current_crtc = selected
            .current_encoder()
            .map(|encoder| card.get_encoder(encoder))
            .transpose()?
            .and_then(|encoder| encoder.crtc());
        let crtc = match current_crtc {
            Some(handle) => handle,
            None => encoders
                .iter()
                .flat_map(|encoder| resources.filter_crtcs(encoder.possible_crtcs()))
                .find(|&handle| {
                    card.get_crtc(handle)
                        .is_ok_and(|info| info.mode().is_none())
                })
                .ok_or_else(|| {
                    io::Error::other("no unused compatible CRTC for the selected connector")
                })?,
        };
        let previous = card.get_crtc(crtc)?;
        let mut previous_connectors = Vec::new();
        for info in &connectors {
            if let Some(encoder) = info.current_encoder()
                && card.get_encoder(encoder)?.crtc() == Some(crtc)
            {
                previous_connectors.push(info.handle());
            }
        }
        if previous_connectors.len() > 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the prototype does not take over cloned outputs",
            ));
        }
        let mode = previous
            .mode()
            .filter(|mode| selected.modes().contains(mode))
            .or_else(|| {
                selected
                    .modes()
                    .iter()
                    .copied()
                    .find(|mode| mode.mode_type().contains(control::ModeTypeFlags::PREFERRED))
            })
            .unwrap_or(selected.modes()[0]);
        let mut display = Self {
            card,
            connector: selected.handle(),
            mode,
            previous,
            previous_connectors,
            buffers: Vec::new(),
            front: 0,
            pending_flip: None,
            pending_at: None,
            installed: false,
            latest: None,
            queued: false,
            cursor: (0, 0),
            cursor_theme: Cursor::new(scale),
            scale,
        };
        let (width, height) = mode.size();
        for _ in 0..2 {
            let dumb = display.card.create_dumb_buffer(
                (width.into(), height.into()),
                DrmFourcc::Xrgb8888,
                32,
            )?;
            let framebuffer = match display.card.add_framebuffer(&dumb, 24, 32) {
                Ok(framebuffer) => framebuffer,
                Err(error) => {
                    let _ = display.card.destroy_dumb_buffer(dumb);
                    return Err(error);
                }
            };
            display.buffers.push(ScanoutBuffer { dumb, framebuffer });
        }
        Ok(display)
    }

    pub fn metrics(&self) -> DisplayMetrics {
        let (width, height) = self.mode.size();
        DisplayMetrics {
            width_px: width.into(),
            height_px: height.into(),
            scale_factor: self.scale,
        }
    }

    pub fn present(&mut self, frame: Frame<'_>) -> io::Result<usize> {
        let metrics = self.metrics();
        if frame.canvas.width != metrics.width_px || frame.canvas.height != metrics.height_px {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the composed frame does not match the DRM mode",
            ));
        }
        self.latest = Some(Snapshot {
            pixels: frame.canvas.pixels.clone().into(),
            premultiplied: frame.premultiplied,
        });
        self.queued = true;
        self.flush()?;
        Ok(frame.canvas.pixels.len())
    }

    pub fn set_cursor(&mut self, point: (u32, u32)) -> io::Result<()> {
        if self.cursor_theme.tick(Instant::now()) || self.cursor != point {
            self.cursor = point;
            self.queued = self.latest.is_some();
            self.flush()?;
        }
        Ok(())
    }

    pub fn set_pointer_shape(&mut self, shape: &str) -> io::Result<()> {
        if self.cursor_theme.set_shape(shape, Instant::now()) {
            self.queued = self.latest.is_some();
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.queued || self.pending_flip.is_some() {
            return Ok(());
        }
        let Some(snapshot) = &self.latest else {
            return Ok(());
        };
        let next = if self.installed { 1 - self.front } else { 0 };
        let metrics = self.metrics();
        let buffer = &mut self.buffers[next];
        let pitch = buffer.dumb.pitch() as usize;
        {
            let mut mapping = self.card.map_dumb_buffer(&mut buffer.dumb)?;
            write_xrgb(
                &snapshot.pixels,
                snapshot.premultiplied,
                (metrics.width_px, metrics.height_px),
                pitch,
                &mut mapping,
            )?;
            self.cursor_theme.draw(
                &mut mapping,
                pitch,
                (metrics.width_px, metrics.height_px),
                self.cursor,
            );
        }
        if !self.installed {
            self.card.set_crtc(
                self.previous.handle(),
                Some(buffer.framebuffer),
                (0, 0),
                &[self.connector],
                Some(self.mode),
            )?;
            self.installed = true;
            self.front = next;
        } else {
            self.card.page_flip(
                self.previous.handle(),
                buffer.framebuffer,
                control::PageFlipFlags::EVENT,
                None,
            )?;
            self.pending_flip = Some(next);
            self.pending_at = Some(Instant::now());
        }
        self.queued = false;
        Ok(())
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.flip_deadline()
            .into_iter()
            .chain(self.latest.as_ref().and(self.cursor_theme.deadline()))
            .min()
    }

    fn flip_deadline(&self) -> Option<Instant> {
        self.pending_at
            .map(|started| started + Duration::from_secs(2))
    }

    pub fn check_timeout(&self) -> io::Result<()> {
        if self
            .flip_deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "DRM page flip did not complete within two seconds",
            ));
        }
        Ok(())
    }

    pub fn receive_events(&mut self) -> io::Result<()> {
        for event in self.card.receive_events()? {
            if let control::Event::PageFlip(event) = event
                && event.crtc == self.previous.handle()
                && let Some(next) = self.pending_flip.take()
            {
                self.front = next;
                self.pending_at = None;
            }
        }
        self.flush()
    }

    pub fn release(&mut self, seat: &mut libseat::SeatRef) -> io::Result<()> {
        let restore = self.restore();
        self.free_buffers();
        let release = self.card.0.release(seat);
        restore.and(release)
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.installed {
            return Ok(());
        }
        self.installed = false;
        self.pending_flip = None;
        self.pending_at = None;
        let connectors = if self.previous.mode().is_some() {
            self.previous_connectors.as_slice()
        } else {
            &[]
        };
        self.card.set_crtc(
            self.previous.handle(),
            self.previous.framebuffer(),
            self.previous.position(),
            connectors,
            self.previous.mode(),
        )
    }

    fn free_buffers(&mut self) {
        for buffer in self.buffers.drain(..) {
            if let Err(error) = self.card.destroy_framebuffer(buffer.framebuffer) {
                crate::logging::warn("drm", format!("framebuffer cleanup failed: {error}"));
            }
            if let Err(error) = self.card.destroy_dumb_buffer(buffer.dumb) {
                crate::logging::warn("drm", format!("buffer cleanup failed: {error}"));
            }
        }
    }
}

impl AsFd for Display {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.card.as_fd()
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        if self.card.0.is_released() {
            return;
        }
        if let Err(error) = self.restore() {
            crate::logging::warn("drm", format!("display restoration failed: {error}"));
        }
        self.free_buffers();
    }
}

fn connector_name(info: &connector::Info) -> String {
    format!("{}-{}", info.interface().as_str(), info.interface_id())
}

fn write_xrgb(
    source: &[u8],
    premultiplied: bool,
    size: (u32, u32),
    pitch: usize,
    target: &mut [u8],
) -> io::Result<()> {
    let (width, height) = (size.0 as usize, size.1 as usize);
    let row_bytes = width.checked_mul(4).ok_or(io::ErrorKind::InvalidInput)?;
    if pitch < row_bytes
        || source.len()
            != row_bytes
                .checked_mul(height)
                .ok_or(io::ErrorKind::InvalidInput)?
        || target.len()
            < pitch
                .checked_mul(height)
                .ok_or(io::ErrorKind::InvalidInput)?
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid scanout buffer dimensions or pitch",
        ));
    }
    for row in 0..height {
        let input = &source[row * row_bytes..(row + 1) * row_bytes];
        let output = &mut target[row * pitch..row * pitch + row_bytes];
        for (rgba, bgra) in input.chunks_exact(4).zip(output.chunks_exact_mut(4)) {
            let channel = |value: u8| {
                if premultiplied {
                    value
                } else {
                    ((u32::from(value) * u32::from(rgba[3]) + 127) / 255) as u8
                }
            };
            bgra.copy_from_slice(&[channel(rgba[2]), channel(rgba[1]), channel(rgba[0]), 255]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanout_conversion_respects_pitch_alpha_and_channel_order() {
        let mut output = [99; 24];
        let source = [
            200, 100, 50, 128, 10, 20, 30, 255, 40, 50, 60, 255, 0, 0, 0, 0,
        ];
        write_xrgb(&source, false, (2, 2), 12, &mut output).unwrap();
        assert_eq!(&output[..8], &[25, 50, 100, 255, 30, 20, 10, 255]);
        assert_eq!(&output[8..12], &[99; 4]);
        assert_eq!(&output[12..20], &[60, 50, 40, 255, 0, 0, 0, 255]);
        write_xrgb(&[25, 50, 100, 128], true, (1, 1), 4, &mut output).unwrap();
        assert_eq!(&output[..4], &[100, 50, 25, 255]);
    }

    #[test]
    fn invalid_buffers_fail_before_scanout_writes() {
        assert!(write_xrgb(&[0; 16], false, (2, 2), 7, &mut [0; 16]).is_err());
        assert!(write_xrgb(&[0; 15], false, (2, 2), 8, &mut [0; 16]).is_err());
        assert!(write_xrgb(&[0; 16], false, (2, 2), 8, &mut [0; 15]).is_err());
        write_xrgb(&[255; 16], false, (2, 2), 8, &mut [0; 16]).unwrap();
    }
}
