use std::io;
use std::time::{Duration, Instant};

use super::{Backend, BackendCapabilities, BackendEvent, DisplayMetrics, Waker};
use crate::canvas::Frame;
use crate::terminal::{Event, Retarget, SessionEnv, Terminal, TerminalColors, WindowSize};
use crate::wrapper::Wrapper;

const CELL_REPLY_WAIT: Duration = Duration::from_millis(150);
const COLOR_SETTLE_DELAY: Duration = Duration::from_millis(50);
const COLOR_REQUEST_INTERVAL: Duration = Duration::from_secs(1);
const RELAYED_RESIZE_POLL: Duration = Duration::from_millis(500);

pub struct KittyConfig {
    pub watch_resize: bool,
    pub tty: Option<String>,
    pub host: Option<HostConfig>,
    pub wrapper: Wrapper,
    pub session_env: SessionEnv,
}

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub socket: String,
    pub pane: String,
    pub name: String,
    pub tty: Option<String>,
}

pub struct KittyBackend {
    term: Terminal,
    session_env: SessionEnv,
    metrics: DisplayMetrics,
    cell: (u32, u32),
    cell_estimate: Option<(u32, u32)>,
    awaiting_cell: Option<(WindowSize, Instant)>,
    cell_exact: bool,
    colors: TerminalColors,
    focused: bool,
    color_request_at: Option<Instant>,
    last_color_request: Option<Instant>,
}

impl KittyBackend {
    pub fn new(config: KittyConfig) -> io::Result<Self> {
        let mut term = match (&config.host, &config.tty) {
            (Some(host), _) => match &host.tty {
                Some(tty) => Terminal::join_embedded(&host.socket, &host.pane, &host.name, tty)?,
                None => Terminal::join_host(&host.socket, &host.pane, &host.name)?,
            },
            (None, Some(path)) => Terminal::open(path, config.wrapper, config.session_env.clone())?,
            (None, None) => Terminal::new(config.wrapper, config.session_env.clone())?,
        };
        if config.watch_resize {
            term.watch_resize()?;
        }
        let colors = term.query_colors()?;
        let ws = term.size()?;
        let cell = term.cell_size()?.unwrap_or(crate::terminal::DEFAULT_CELL);
        let (width_px, height_px) = window_from(&ws, cell, false);
        Ok(Self {
            term,
            session_env: config.session_env,
            metrics: DisplayMetrics {
                width_px,
                height_px,
                scale_factor: 1.0,
            },
            cell,
            cell_estimate: ws.cell_size(),
            awaiting_cell: None,
            cell_exact: false,
            colors,
            focused: true,
            color_request_at: None,
            last_color_request: None,
        })
    }

    fn apply_window(&mut self, ws: WindowSize) -> io::Result<()> {
        let estimate = ws.cell_size();
        if estimate != self.cell_estimate {
            self.cell_estimate = estimate;
            if let Some(cell) = self.term.ask_cell_size()? {
                self.awaiting_cell = None;
                self.cell_exact = true;
                self.resize_to(ws, cell);
                return Ok(());
            }
            self.awaiting_cell = Some((ws, Instant::now() + CELL_REPLY_WAIT));
        }
        match &mut self.awaiting_cell {
            Some((pending, _)) => *pending = ws,
            None => self.resize_to(ws, self.cell),
        }
        Ok(())
    }

    fn apply_overdue_window(&mut self) {
        if let Some((ws, at)) = self.awaiting_cell
            && Instant::now() >= at
        {
            self.awaiting_cell = None;
            self.cell_exact = false;
            self.resize_to(ws, ws.cell_size().unwrap_or(self.cell));
        }
    }

    fn resize_to(&mut self, ws: WindowSize, cell: (u32, u32)) {
        self.term.set_cell_size(cell);
        let (width_px, height_px) = window_from(&ws, cell, self.cell_exact);
        self.cell = cell;
        self.metrics = DisplayMetrics {
            width_px,
            height_px,
            scale_factor: 1.0,
        };
    }

    fn schedule_color_request(&mut self, delay: Duration) {
        let at = Instant::now() + delay;
        self.color_request_at = Some(self.color_request_at.map_or(at, |queued| queued.min(at)));
    }

    fn send_due_color_request(&mut self) -> io::Result<()> {
        if self.color_request_at.is_some_and(|at| Instant::now() >= at) {
            self.color_request_at = None;
            self.last_color_request = Some(Instant::now());
            self.term.request_colors()?;
        }
        Ok(())
    }

    fn wait_time(&self, timeout: Option<Duration>) -> Option<Duration> {
        let mut wait = timeout;
        for deadline in [self.color_request_at, self.awaiting_cell.map(|(_, at)| at)]
            .into_iter()
            .flatten()
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            wait = Some(wait.map_or(remaining, |w| w.min(remaining)));
        }
        if self.term.relayed() {
            wait = Some(wait.map_or(RELAYED_RESIZE_POLL, |w| w.min(RELAYED_RESIZE_POLL)));
        }
        wait
    }
}

impl Backend for KittyBackend {
    fn metrics(&mut self) -> io::Result<DisplayMetrics> {
        let ws = self.term.size()?;
        if ws.cols != 0 || ws.width_px != 0 {
            self.apply_window(ws)?;
        }
        self.apply_overdue_window();
        Ok(self.metrics)
    }

    fn present(&mut self, frame: Frame<'_>) -> io::Result<usize> {
        self.term.draw(frame)
    }

    fn poll_event(&mut self, timeout: Option<Duration>) -> io::Result<Option<BackendEvent>> {
        let deadline = timeout.map(|wait| Instant::now() + wait);
        loop {
            self.send_due_color_request()?;
            let wait =
                self.wait_time(deadline.map(|at| at.saturating_duration_since(Instant::now())));
            let Some(event) = self.term.poll_event(wait)? else {
                self.send_due_color_request()?;
                return Ok(None);
            };
            return Ok(Some(match event {
                Event::Key(key) => BackendEvent::Key(key),
                Event::Mouse(mouse) => BackendEvent::Mouse(mouse),
                Event::Wheel {
                    x,
                    y,
                    delta_x,
                    delta_y,
                    mods,
                } => BackendEvent::Wheel {
                    x,
                    y,
                    delta_x,
                    delta_y,
                    mods,
                },
                Event::Paste(text) => BackendEvent::Paste(text),
                Event::Focus(focused) => {
                    if focused
                        && !self.focused
                        && !self.term.reports_color_scheme()
                        && self
                            .last_color_request
                            .is_none_or(|at| at.elapsed() >= COLOR_REQUEST_INTERVAL)
                    {
                        self.schedule_color_request(Duration::ZERO);
                    }
                    self.focused = focused;
                    BackendEvent::Focus(focused)
                }
                Event::WindowSize(ws) => {
                    self.apply_window(ws)?;
                    if self.term.is_embedded() {
                        BackendEvent::RedrawRequested
                    } else {
                        BackendEvent::Resize
                    }
                }
                Event::CellSize(cell) => {
                    let ws = match self.awaiting_cell.take() {
                        Some((ws, _)) => ws,
                        None => self.term.size()?,
                    };
                    self.cell_exact = true;
                    self.resize_to(ws, cell);
                    BackendEvent::Resize
                }
                Event::Colors(colors) => {
                    self.colors = colors;
                    BackendEvent::Colors(colors)
                }
                Event::ColorSchemeChanged => {
                    self.schedule_color_request(COLOR_SETTLE_DELAY);
                    continue;
                }
                Event::ClipboardData { items, ok } => BackendEvent::ClipboardData { items, ok },
                Event::HostClosed => BackendEvent::HostClosed,
                Event::Handoff(handoff) => BackendEvent::Handoff(handoff),
                Event::Devtools => BackendEvent::Devtools,
                Event::Visible(visible) => BackendEvent::Visible(visible),
            }));
        }
    }

    fn waker(&mut self) -> io::Result<Waker> {
        self.term.waker()
    }

    fn name(&self) -> &'static str {
        "kitty"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            damage: true,
            transparency: true,
            pixel_pointer: self.term.reports_pixel_mouse(),
            key_releases: self.term.kitty_keyboard(),
            super_key: self.term.kitty_keyboard(),
        }
    }

    fn cell_size(&self) -> Option<(u32, u32)> {
        Some(self.cell)
    }

    fn colors(&self) -> TerminalColors {
        self.colors
    }

    fn terminal(&self) -> Option<&Terminal> {
        Some(&self.term)
    }

    fn terminal_mut(&mut self) -> Option<&mut Terminal> {
        Some(&mut self.term)
    }

    fn retarget(&mut self, target: Retarget) -> io::Result<()> {
        self.term.retarget(target, self.session_env.clone())?;
        self.colors = self.term.query_colors()?;
        self.cell_estimate = None;
        self.apply_window(self.term.size()?)
    }

    fn set_pointer_shape(&mut self, shape: &str) -> io::Result<()> {
        self.term.set_pointer_shape(shape)
    }

    fn set_title(&mut self, text: &str) -> io::Result<()> {
        self.term.set_title(text)
    }

    fn set_clipboard(&mut self, text: &str) -> io::Result<()> {
        self.term.set_clipboard(text)
    }

    fn request_clipboard(&mut self) -> io::Result<()> {
        self.term.request_clipboard()
    }
}

fn window_from(
    ws: &crate::terminal::WindowSize,
    cell: (u32, u32),
    cell_is_exact: bool,
) -> (u32, u32) {
    let cols = if ws.cols > 0 { ws.cols } else { 80 };
    let rows = if ws.rows > 0 { ws.rows } else { 24 };
    let mut width = cols * cell.0;
    let mut height = rows * cell.1;
    if cell_is_exact {
        return (width, height);
    }
    if ws.width_px > 0 {
        width = width.min(ws.width_px / cell.0 * cell.0);
    }
    if ws.height_px > 0 {
        height = height.min(ws.height_px / cell.1 * cell.1);
    }
    (width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::WindowSize;

    #[test]
    fn window_uses_grid_when_pixels_missing() {
        let ws = WindowSize {
            cols: 80,
            rows: 24,
            width_px: 0,
            height_px: 0,
        };
        assert_eq!(window_from(&ws, (16, 32), false), (80 * 16, 24 * 32));
    }

    #[test]
    fn window_is_whole_cells_within_the_reported_pixels() {
        let ws = WindowSize {
            cols: 100,
            rows: 40,
            width_px: 1007,
            height_px: 845,
        };
        assert_eq!(window_from(&ws, (10, 20), false), (1000, 800));
        let ws = WindowSize {
            cols: 100,
            rows: 40,
            width_px: 1050,
            height_px: 800,
        };
        assert_eq!(window_from(&ws, (11, 21), false), (1045, 798));
    }

    #[test]
    fn a_cell_the_terminal_reported_outranks_relay_pixels() {
        let ws = WindowSize {
            cols: 94,
            rows: 51,
            width_px: 1504,
            height_px: 1734,
        };
        assert_eq!(window_from(&ws, (19, 42), true), (94 * 19, 51 * 42));
    }
}
