mod clipboard;
mod compositor;
mod doc;
mod embed;
mod frame;
mod hover;
mod input;
mod keys;
mod native_pairing;
mod overlay;
mod pointer;
mod scroll;

use std::io;
use std::time::{Duration, Instant};

use clipboard::ClipboardFlows;
use compositor::Compositor;
use hover::HoverOracle;
use native_pairing::NativePairing;
use pointer::DragTarget;

pub use overlay::HighlightArea;

use crate::backend::{Backend, BackendEvent as Event, DisplayMetrics, Waker};
pub use crate::backend::HostConfig;
use crate::logging;
use crate::menu::MenuController;
use crate::native::NativeScroll;
use crate::profiler::ProfileData;
use crate::scroll::ScrollProfile;
use crate::scroll::profiles::Smooth;
use crate::style::Color;
use crate::terminal::{
    Handoff, KeyEvent, Mods, Mouse, MouseButton, MouseKind, Retarget, Terminal,
    TerminalColors,
};
use crate::text_input::InputReply;
use crate::throttle::CpuThrottle;
use crate::tree::{NodeId, PxRect};

const PAINT_INTERVAL_WITH_INPUT_PENDING: Duration = Duration::from_millis(16);

static DEFAULT_PROFILE: Smooth = Smooth {
    tau: 0.08,
    brake: 0.025,
};

pub struct EngineConfig {
    pub fonts: crate::fonts::FontRegistry,
    pub cell_metrics_font: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MarkRef {
    pub id: u64,
    pub offset: usize,
    pub data: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeSource {
    Type,
    Paste,
    Edit,
}

impl ChangeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeSource::Type => "type",
            ChangeSource::Paste => "paste",
            ChangeSource::Edit => "edit",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    CloseRequested,
    HostClosed,
    Handoff {
        tty: Option<String>,
        socket: Option<String>,
    },
    Visible {
        visible: bool,
    },
    Click {
        view: usize,
        node: NodeId,
        key: Option<String>,
        x: f32,
        y: f32,
        offset: Option<usize>,
    },
    ClickOutside {
        view: usize,
        node: NodeId,
        key: Option<String>,
        x: f32,
        y: f32,
    },
    RightClick {
        view: usize,
        x: f32,
        y: f32,
    },
    Change {
        view: usize,
        node: NodeId,
        key: Option<String>,
        text: String,
        marks: Vec<MarkRef>,
        cursor: usize,
        caret: PxRect,
        source: ChangeSource,
    },
    Caret {
        view: usize,
        node: NodeId,
        key: Option<String>,
        cursor: usize,
        caret: PxRect,
    },
    Submit {
        view: usize,
        node: NodeId,
        key: Option<String>,
        text: String,
        marks: Vec<MarkRef>,
    },
    Scroll {
        view: usize,
        node: NodeId,
        key: Option<String>,
        offset: f32,
        max: f32,
    },
    Resize {
        view: usize,
        width: u32,
        height: u32,
        cell: Option<(u32, u32)>,
        base_px: f32,
        scale_factor: f32,
    },
    Colors {
        colors: TerminalColors,
    },
    Devtools,
    Inspect {
        view: usize,
        node: NodeId,
        key: Option<String>,
        x: f32,
        y: f32,
    },
    Key {
        view: usize,
        event: KeyEvent,
    },
    Paste {
        view: usize,
        text: String,
    },
    Focus {
        focused: bool,
    },
    PasteImage {
        view: usize,
        node: NodeId,
        key: Option<String>,
        path: String,
        width: u32,
        height: u32,
        source: crate::clipboard_image::PasteSource,
    },
    SerializeMarks {
        view: usize,
        token: u64,
        marks: Vec<(NodeId, u64, usize)>,
    },
    Wheel {
        view: usize,
        node: NodeId,
        key: Option<String>,
        x: f32,
        y: f32,
        delta_x: f32,
        delta_y: f32,
        precise: bool,
        mods: Mods,
    },
    MouseMove {
        view: usize,
        node: NodeId,
        key: Option<String>,
        x: f32,
        y: f32,
    },
    Pointer {
        view: usize,
        node: NodeId,
        key: Option<String>,
        kind: MouseKind,
        button: MouseButton,
        mods: crate::terminal::Mods,
        x: f32,
        y: f32,
    },
    HoverEnter {
        view: usize,
        node: NodeId,
        key: Option<String>,
    },
    HoverLeave {
        view: usize,
        node: NodeId,
        key: Option<String>,
    },
    Drag {
        view: usize,
        node: NodeId,
        key: Option<String>,
        phase: DragPhase,
        x: f32,
        y: f32,
        mods: Mods,
    },
    Selection {
        view: usize,
        node: NodeId,
        key: Option<String>,
        text: String,
        rect: PxRect,
        parts: Vec<(String, usize, usize)>,
    },
    Log(logging::LogEntry),
    Profile(ProfileData),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FrameStats {
    pub frame_ms: f32,
    pub fps: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragPhase {
    Start,
    Move,
    End,
}

pub struct Engine {
    backend: Box<dyn Backend>,
    pub display: DisplayMetrics,
    pub comp: Compositor,
    pub fonts: crate::fonts::FontRegistry,
    cell_metrics_font: usize,
    pub cell: Option<(u32, u32)>,
    pub base_px: f32,
    pub colors: TerminalColors,
    cursor: Option<(f32, f32)>,
    hover: Option<(usize, NodeId)>,
    focus_view: usize,
    active_view: usize,
    term_focused: bool,
    pub native: Option<NativeScroll>,
    pub use_native: bool,
    pixel_mouse: bool,
    last_native_scroll: Option<Instant>,
    pairing: NativePairing,
    hover_oracle: HoverOracle,
    pub profile: &'static dyn ScrollProfile,
    pub(super) last_wheel_tick:
        Option<(Instant, crate::terminal::MouseKind, u32, u32, crate::terminal::Mods)>,
    pub(super) wheel_echo_consumed: bool,
    default_menu: bool,
    menu: MenuController,
    inspect_mode: bool,
    inspect_view: usize,
    inspect_hover: Option<NodeId>,
    highlight: Option<(usize, NodeId, HighlightArea)>,
    hover_target: Option<(usize, NodeId)>,
    pointer_shape: Option<String>,
    emit_logs: bool,
    log_cursor: u64,
    drag: Option<(usize, DragTarget)>,
    pending_click: Option<(usize, NodeId)>,
    pointer_capture: Option<(usize, NodeId)>,
    key_passthrough: bool,
    last_selection: Option<(
        usize,
        NodeId,
        crate::selection::DocPos,
        crate::selection::DocPos,
        u32,
    )>,
    bar_hover: Option<(usize, NodeId)>,
    bar_drag: Option<(usize, NodeId, f32)>,
    reveal: bool,
    pub key_capture: Vec<String>,
    pub cpu_throttle: CpuThrottle,
    throttle_registered: bool,
    scroll_burst: u32,
    last_scroll_mark: Option<Instant>,
    clipboard: ClipboardFlows,
    focus_click: Option<(Instant, (f32, f32))>,
    last_pointer_activity: Option<Instant>,
    last_pointer_click: Option<Instant>,
    next_pasted_mark: u64,
    pending: Vec<EngineEvent>,
    last_step: Instant,
    last_frame: Instant,
    last_frame_bytes: usize,
    frame_deferred: bool,
    max_fps: f32,
    frame_due: Option<Instant>,
    pub stats: FrameStats,
}

impl Engine {
    pub fn new(config: EngineConfig, mut backend: Box<dyn Backend>) -> io::Result<Self> {
        assert!(!config.fonts.is_empty());
        let display = backend.metrics()?.validate()?;
        let window = (display.width_px, display.height_px);
        let cell = backend.cell_size();
        let base_px = match cell {
            Some(cell) => {
                px_for_cell_height(&config.fonts[config.cell_metrics_font], cell.1 as f32)
            }
            None => 16.0 * display.scale_factor,
        };
        let colors = backend.colors();
        let native = match backend.terminal_mut() {
            Some(term) if !term.is_hosted() || term.is_embedded() => {
                NativeScroll::spawn(term.waker().ok())
            }
            _ => None,
        };
        let use_native = native.is_some();
        let pixel_mouse = backend.capabilities().pixel_pointer;
        logging::info(
            "engine",
            format!(
                "started {}x{}px, cell {cell:?}, base {base_px:.1}px, native scroll {use_native}",
                window.0, window.1
            ),
        );
        Ok(Self {
            backend,
            display,
            comp: Compositor::new(window),
            fonts: config.fonts,
            cell_metrics_font: config.cell_metrics_font,
            cell,
            base_px,
            colors,
            cursor: None,
            hover: None,
            focus_view: 0,
            active_view: 0,
            term_focused: true,
            native,
            use_native,
            pixel_mouse,
            last_native_scroll: None,
            pairing: NativePairing::new(),
            hover_oracle: HoverOracle::new(),
            profile: &DEFAULT_PROFILE,
            last_wheel_tick: None,
            wheel_echo_consumed: false,
            default_menu: false,
            menu: MenuController::default(),
            inspect_mode: false,
            inspect_view: 0,
            inspect_hover: None,
            highlight: None,
            hover_target: None,
            pointer_shape: None,
            emit_logs: false,
            log_cursor: 0,
            drag: None,
            pending_click: None,
            pointer_capture: None,
            key_passthrough: false,
            last_selection: None,
            bar_hover: None,
            bar_drag: None,
            reveal: false,
            key_capture: Vec::new(),
            cpu_throttle: CpuThrottle::new(),
            throttle_registered: false,
            scroll_burst: 0,
            last_scroll_mark: None,
            clipboard: ClipboardFlows::new(),
            focus_click: None,
            last_pointer_activity: None,
            last_pointer_click: None,
            next_pasted_mark: 1 << 48,
            pending: Vec::new(),
            last_step: Instant::now(),
            last_frame: Instant::now(),
            last_frame_bytes: 0,
            frame_deferred: false,
            max_fps: frame::DEFAULT_MAX_FPS,
            frame_due: None,
            stats: FrameStats::default(),
        })
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    pub fn backend_capabilities(&self) -> crate::backend::BackendCapabilities {
        self.backend.capabilities()
    }

    pub fn terminal(&self) -> Option<&Terminal> {
        self.backend.terminal()
    }

    pub fn terminal_mut(&mut self) -> Option<&mut Terminal> {
        self.backend.terminal_mut()
    }

    pub fn waker(&mut self) -> io::Result<Waker> {
        self.backend.waker()
    }

    pub fn add_font_face(&mut self, family: &str, weight: u16, italic: bool, face: fontdue::Font, source: &str) {
        if !self.fonts.add_face(family, weight, italic, face, source) {
            return;
        }
        for view in &mut self.comp.views {
            view.tree.mark_layout();
        }
    }

    pub fn add_view(&mut self) -> usize {
        let view = self.comp.add_view();
        logging::info("engine", format!("view {view} created"));
        view
    }

    pub fn set_pane(&mut self, slot: usize, view: usize) {
        if self.comp.set_pane(slot, view) {
            logging::info("engine", format!("pane {slot} shows view {view}"));
            let resized = self.comp.apply_layout(true);
            self.push_resizes(resized);
        }
    }

    pub fn set_inspect_view(&mut self, view: usize) {
        if view < self.comp.views.len() {
            self.inspect_view = view;
        }
    }

    pub fn set_clear_color(&mut self, view: usize, color: Color) {
        let Some(v) = self.comp.views.get_mut(view) else {
            return;
        };
        if v.clear_color != color {
            v.clear_color = color;
            v.tree.mark_paint();
        }
    }

    pub fn set_default_menu(&mut self, enabled: bool) {
        self.default_menu = enabled;
        if !enabled {
            self.close_menu();
        }
    }

    pub fn set_inspect_mode(&mut self, enabled: bool) {
        if self.inspect_mode != enabled {
            self.inspect_mode = enabled;
            self.inspect_hover = None;
            self.comp.dirty = true;
        }
    }

    pub fn set_highlight(&mut self, target: Option<(usize, NodeId, HighlightArea)>) {
        if self.highlight != target {
            self.highlight = target;
            self.comp.dirty = true;
        }
    }

    pub fn set_split(&mut self, split: Option<f32>) {
        if !self.comp.set_split(split) {
            return;
        }
        logging::info(
            "engine",
            match self.comp.split {
                Some(f) => format!("split screen at {:.0}%", f * 100.0),
                None => "split screen closed".into(),
            },
        );
        let resized = self.comp.apply_layout(false);
        self.push_resizes(resized);
    }

    fn push_resizes(&mut self, resized: Vec<(usize, (u32, u32))>) {
        for (view, size) in resized {
            self.pending.push(EngineEvent::Resize {
                view,
                width: size.0,
                height: size.1,
                cell: self.cell,
                base_px: self.base_px,
                scale_factor: self.display.scale_factor,
            });
        }
    }

    pub fn native_scroll_active(&self) -> bool {
        self.use_native
            && self.native.is_some()
            && self
                .last_native_scroll
                .is_some_and(|at| at.elapsed() < Duration::from_millis(1500))
    }

    pub fn profile_start(&mut self) {
        if !crate::profiler::is_recording() {
            logging::info("profiler", "recording started");
            crate::profiler::start();
        }
    }

    pub fn profile_stop(&mut self) {
        if let Some(data) = self.stop_recording() {
            self.pending.push(EngineEvent::Profile(data));
        }
    }

    pub fn profile_stop_to_file(&mut self) -> io::Result<Option<std::path::PathBuf>> {
        self.stop_recording().map(|data| crate::profiler::write_report(&data)).transpose()
    }

    fn stop_recording(&mut self) -> Option<ProfileData> {
        crate::image_cache::emit_pending_waits();
        let data = crate::profiler::stop()?;
        logging::info("profiler", format!("recording stopped, {} spans", data.spans.len()));
        Some(data)
    }

    pub fn set_cpu_throttle(&mut self, rate: f32) {
        if !CpuThrottle::supported() && rate > 1.0 {
            logging::warn("engine", "cpu throttle is only supported on macOS");
            return;
        }
        self.cpu_throttle.set_rate(rate);
        let applied = self.cpu_throttle.rate();
        logging::info("engine", format!("cpu throttle {applied}x"));
        crate::profiler::mark("throttle", 0, || format!("cpu throttle {applied}x"));
    }

    pub fn flush_view_layout(&mut self, view: usize) {
        let base_px = self.base_px;
        let fonts = &mut self.fonts;
        if let Some(v) = self.comp.views.get_mut(view) {
            v.tree.flush_layout(fonts, base_px);
        }
    }

    pub fn set_focus(&mut self, view: usize, id: Option<NodeId>) {
        if view >= self.comp.views.len() {
            return;
        }
        for (i, v) in self.comp.views.iter_mut().enumerate() {
            if i != view {
                v.tree.set_focus(None);
            }
        }
        self.comp.views[view].tree.set_focus(id);
        if id.is_some() {
            self.key_passthrough = false;
        }
        self.focus_view = view;
    }

    fn focused(&self) -> Option<(usize, NodeId)> {
        self.comp.views[self.focus_view]
            .tree
            .focus()
            .map(|id| (self.focus_view, id))
    }

    fn paint_then_wait(
        &mut self,
        out_empty: bool,
        wait: Option<Duration>,
    ) -> io::Result<Option<Event>> {
        self.frame()?;
        self.sync_pointer_shape();
        let mut first_wait = if self.frame_deferred && let Some(due) = self.frame_due {
            Some(due.saturating_duration_since(Instant::now()).max(Duration::from_millis(1)))
        } else if self.animating() || self.frame_deferred {
            Some(Duration::from_millis(6))
        } else if !out_empty {
            Some(Duration::ZERO)
        } else {
            wait
        };
        let deadlines = [
            self.clipboard.osc_deadline(),
            self.focus_click.as_ref().map(|(deadline, _)| *deadline),
            self.backend.terminal().and_then(Terminal::idle_flatten_at),
            self.backend.terminal().and_then(Terminal::overlay_due),
        ];
        for deadline in deadlines.into_iter().flatten() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            first_wait = Some(first_wait.map_or(remaining, |w| w.min(remaining)));
        }
        self.backend.poll_event(first_wait)
    }

    pub fn pump(&mut self, wait: Option<Duration>) -> io::Result<Vec<EngineEvent>> {
        if !self.throttle_registered {
            self.throttle_registered = true;
            self.cpu_throttle.register_current_thread();
            if let Ok(waker) = self.backend.waker() {
                crate::image_cache::set_waker(move || waker.wake());
            }
        }

        self.drain_images();
        let mut out = Vec::new();
        out.append(&mut self.pending);
        if !out.is_empty() {
            self.drain_logs(&mut out);
            return Ok(out);
        }
        self.check_resize(&mut out)?;
        let mut event = self.backend.poll_event(Some(Duration::ZERO))?;
        let input_pending = event.is_some();
        if !input_pending {
            event = self.paint_then_wait(out.is_empty(), wait)?;
        }
        while let Some(current) = event {
            let closing = matches!(&current, Event::CloseRequested);
            self.handle_event(current, &mut out)?;
            if closing {
                self.drain_logs(&mut out);
                return Ok(out);
            }
            event = self.backend.poll_event(Some(Duration::ZERO))?;
        }
        if let Some((deadline, point)) = self.focus_click
            && Instant::now() >= deadline
        {
            self.focus_click = None;
            for kind in [MouseKind::Down, MouseKind::Up] {
                self.handle_mouse(
                    Mouse {
                        kind,
                        button: MouseButton::Left,
                        mods: Mods::default(),
                        x: point.0 as u32,
                        y: point.1 as u32,
                    },
                    &mut out,
                )?;
            }
        }
        self.check_resize(&mut out)?;
        self.drain_native(&mut out);
        let now = Instant::now();
        let dt = now.duration_since(self.last_step).as_secs_f32().min(0.05);
        self.last_step = now;
        self.step_scrolls(dt);
        self.step_bars(dt);
        if self.reveal {
            self.reveal = false;
            self.reveal_caret();
        }
        self.emit_scroll_events(&mut out);
        self.drain_images();
        out.append(&mut self.pending);
        if !input_pending || self.last_frame.elapsed() >= PAINT_INTERVAL_WITH_INPUT_PENDING {
            self.frame()?;
        }
        self.sync_pointer_shape();
        self.drain_logs(&mut out);
        Ok(out)
    }

    fn drain_images(&mut self) {
        let drained = crate::image_cache::drain_completed();
        if drained.landed {
            for view in &mut self.comp.views {
                view.tree.mark_layout();
            }
        }
        for (view, node, image) in self
            .clipboard
            .resolve_pastes(&mut *self.backend, drained.pastes)
        {
            let mut out = std::mem::take(&mut self.pending);
            self.push_paste_image(view, node, image, &mut out);
            self.pending = out;
        }
    }

    pub fn set_log_capture(&mut self, on: bool) {
        logging::keep_in_memory(on);
        self.emit_logs = on;
    }

    pub fn set_log_file(&mut self, path: Option<std::path::PathBuf>) {
        logging::write_to_file(path);
    }

    fn drain_logs(&mut self, out: &mut Vec<EngineEvent>) {
        if !self.emit_logs {
            return;
        }
        let entries = logging::entries_after(self.log_cursor);
        if let Some(last) = entries.last() {
            self.log_cursor = last.seq + 1;
        }
        out.extend(entries.into_iter().map(EngineEvent::Log));
    }

    fn apply_colors(&mut self, colors: TerminalColors, out: &mut Vec<EngineEvent>) {
        if colors == self.colors {
            return;
        }
        self.colors = colors;
        logging::info(
            "engine",
            format!("colors changed, background {:?}", colors.background),
        );
        for view in &mut self.comp.views {
            view.tree.mark_paint();
        }
        self.comp.dirty = true;
        out.push(EngineEvent::Colors { colors });
    }

    fn check_resize(&mut self, out: &mut Vec<EngineEvent>) -> io::Result<()> {
        let display = self.backend.metrics()?.validate()?;
        let cell = self.backend.cell_size();
        if display == self.display && cell == self.cell {
            return Ok(());
        }
        let base_px = match cell {
            Some(cell) => px_for_cell_height(
                &self.fonts[self.cell_metrics_font.min(self.fonts.len() - 1)],
                cell.1 as f32,
            ),
            None => 16.0 * display.scale_factor,
        };
        logging::info(
            "engine",
            format!(
                "resize {}x{}px scale {}, cell {cell:?}, base {base_px:.1}px",
                display.width_px, display.height_px, display.scale_factor
            ),
        );
        let base_changed = (base_px - self.base_px).abs() > 0.01;
        self.hover_oracle.invalidate();
        self.comp.window = (display.width_px, display.height_px);
        self.display = display;
        self.cell = cell;
        self.base_px = base_px;
        let resized = self.comp.apply_layout(base_changed);
        self.push_resizes(resized);
        out.append(&mut self.pending);
        Ok(())
    }

    pub fn retarget(&mut self, target: Retarget) -> io::Result<()> {
        self.backend.retarget(target)?;
        self.pointer_shape = None;
        self.native = match self.backend.terminal_mut() {
            Some(term) if !term.is_hosted() || term.is_embedded() => {
                NativeScroll::spawn(term.waker().ok())
            }
            _ => None,
        };
        self.use_native = self.native.is_some();
        self.pixel_mouse = self.backend.capabilities().pixel_pointer;
        let mut out = Vec::new();
        self.apply_colors(self.backend.colors(), &mut out);
        self.check_resize(&mut out)?;
        self.comp.dirty = true;
        self.pending.append(&mut out);
        Ok(())
    }

    fn handle_event(&mut self, event: Event, out: &mut Vec<EngineEvent>) -> io::Result<()> {
        match event {
            Event::HostClosed => out.push(EngineEvent::HostClosed),
            Event::Wheel {
                x,
                y,
                delta_x,
                delta_y,
                mods,
            } => self.forwarded_wheel((x as f32, y as f32), (delta_x, delta_y), mods, out),
            Event::Handoff(Handoff::Adopt { tty }) => out.push(EngineEvent::Handoff {
                tty: Some(tty),
                socket: None,
            }),
            Event::Handoff(Handoff::Rejoin { socket }) => out.push(EngineEvent::Handoff {
                tty: None,
                socket: Some(socket),
            }),
            Event::Key(key) => self.handle_key(key, out)?,
            Event::Paste(text) => {
                crate::profiler::mark("paste", self.active_view as u32, || {
                    format!("paste ({} chars)", text.chars().count())
                });
                if let Some((view, focus)) = self.focused() {
                    if let Some(image) = crate::clipboard_image::image_path_from_paste(&text) {
                        self.push_paste_image(view, focus, image, out);
                    } else {
                        let rich = clipboard::parse_rich_paste(&text);
                        if let Some((_, marks)) = &rich {
                            self.next_pasted_mark += marks.len() as u64;
                        }
                        let first_id = self.next_pasted_mark
                            - rich.as_ref().map_or(0, |(_, m)| m.len() as u64);
                        if let Some(input) = self.comp.views[view].tree.input_mut(focus) {
                            match rich {
                                Some((rich_text, marks)) => {
                                    input.insert_rich(&rich_text, &marks, first_id)
                                }
                                None => input.insert(&text),
                            }
                            self.finish_reply(
                                view,
                                focus,
                                InputReply::Edited,
                                ChangeSource::Paste,
                                out,
                            )?;
                        }
                    }
                    // todo: review this better
                } else if let Some(image) = crate::clipboard_image::image_path_from_paste(&text) {
                    let view = self.active_view;
                    let root = self.comp.views[view].tree.root();
                    self.push_paste_image(view, root, image, out);
                } else {
                    out.push(EngineEvent::Paste {
                        view: self.active_view,
                        text,
                    });
                }
            }
            Event::Focus(focused) => {
                let gained = focused && !self.term_focused;
                self.term_focused = focused;
                if !focused {
                    self.focus_click = None;
                } else if gained
                    && let Some(at) = self.last_pointer_activity
                    && at.elapsed() <= Duration::from_millis(1000)
                    && self
                        .last_pointer_click
                        .is_none_or(|click| click.elapsed() > Duration::from_millis(1000))
                    && let Some(point) = self.cursor
                {
                    self.focus_click = Some((Instant::now() + Duration::from_millis(75), point));
                }
                out.push(EngineEvent::Focus { focused });
            }
            Event::Resize => self.check_resize(out)?,
            Event::RedrawRequested => {
                self.check_resize(out)?;
                self.comp.dirty = true;
            }
            Event::CloseRequested => out.push(EngineEvent::CloseRequested),
            Event::Mouse(mouse) => self.handle_mouse(mouse, out)?,
            Event::ClipboardData { items, ok } => {
                self.clipboard
                    .handle_clipboard_data(&mut *self.backend, items, ok)
            }
            Event::Colors(colors) => self.apply_colors(colors, out),
            Event::Devtools => out.push(EngineEvent::Devtools),
            Event::Visible(visible) => out.push(EngineEvent::Visible { visible }),
        }
        self.emit_selection_change(out);
        Ok(())
    }

    pub fn set_clipboard(&mut self, text: &str) {
        if let Err(error) = self.backend.set_clipboard(text) {
            logging::warn("engine", format!("clipboard write failed: {error}"));
        }
    }

    pub fn set_title(&mut self, text: &str) {
        if let Err(error) = self.backend.set_title(text) {
            logging::warn("engine", format!("title write failed: {error}"));
        }
    }

    pub fn request_clipboard_image(&mut self, view: usize) {
        let root = self.comp.views[view].tree.root();
        self.clipboard.request_paste(view, root);
    }

    fn begin_rich_capture(
        &mut self,
        view: usize,
        text: String,
        marks: Vec<(NodeId, crate::text_input::Mark)>,
    ) -> io::Result<()> {
        let event = self
            .clipboard
            .begin_rich_capture(&mut *self.backend, view, text, marks)?;
        self.pending.extend(event);
        Ok(())
    }

    pub fn attach_rich_clipboard(&mut self, token: u64, marks: Vec<(usize, String)>) {
        self.clipboard.attach_rich(&mut *self.backend, token, marks);
    }

}


pub fn px_for_cell_height(font: &crate::fonts::Font, cell_height: f32) -> f32 {
    let probe = font
        .horizontal_line_metrics(100.0)
        .expect("font has horizontal metrics");
    (cell_height * 100.0 / probe.new_line_size).clamp(6.0, 512.0)
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Err(error) = self.backend.shutdown() {
            logging::warn("engine", format!("backend shutdown failed: {error}"));
        }
    }
}
