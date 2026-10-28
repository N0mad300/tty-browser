use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pixel_core::backend::{Backend, BackendCapabilities, BackendEvent, DisplayMetrics, Waker};
use pixel_core::surfaces::Rect;
use pixel_core::{
    Desc, Dimension, Engine, EngineConfig, EngineEvent, Frame, InputProps, Key, KeyEvent, KeyKind,
    Mods, Mouse, MouseButton, MouseKind, Paint, Style,
};

struct Presented {
    size: (u32, u32),
    pixels: Vec<u8>,
    damage: Vec<Rect>,
}

impl Presented {
    fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let start = ((y * self.size.0 + x) * 4) as usize;
        self.pixels[start..start + 4].try_into().unwrap()
    }
}

struct State {
    metrics: DisplayMetrics,
    events: VecDeque<BackendEvent>,
    frames: Vec<Presented>,
    fail_present: bool,
    shutdowns: usize,
    drops: usize,
}

struct MockBackend {
    state: Arc<Mutex<State>>,
    wake_rx: rustix::fd::OwnedFd,
    waker: Waker,
}

impl MockBackend {
    fn new(metrics: DisplayMetrics) -> (Self, Arc<Mutex<State>>) {
        let state = Arc::new(Mutex::new(State {
            metrics,
            events: VecDeque::new(),
            frames: Vec::new(),
            fail_present: false,
            shutdowns: 0,
            drops: 0,
        }));
        let (wake_rx, waker) = Waker::pipe().unwrap();
        (
            Self {
                state: state.clone(),
                wake_rx,
                waker,
            },
            state,
        )
    }
}

impl Backend for MockBackend {
    fn metrics(&mut self) -> io::Result<DisplayMetrics> {
        Ok(self.state.lock().unwrap().metrics)
    }

    fn present(&mut self, frame: Frame<'_>) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap();
        if state.fail_present {
            return Err(io::Error::other("presentation failed"));
        }
        state.frames.push(Presented {
            size: (frame.canvas.width, frame.canvas.height),
            pixels: frame.canvas.pixels.clone(),
            damage: frame
                .changed
                .iter()
                .chain(frame.repainted)
                .copied()
                .collect(),
        });
        Ok(frame.canvas.pixels.len())
    }

    fn poll_event(&mut self, timeout: Option<Duration>) -> io::Result<Option<BackendEvent>> {
        if let Some(event) = self.state.lock().unwrap().events.pop_front() {
            return Ok(Some(event));
        }
        let mut fds = [rustix::event::PollFd::new(
            &self.wake_rx,
            rustix::event::PollFlags::IN,
        )];
        let timeout = timeout
            .map(rustix::event::Timespec::try_from)
            .transpose()
            .map_err(io::Error::other)?;
        rustix::event::poll(&mut fds, timeout.as_ref())?;
        let mut bytes = [0; 64];
        while rustix::io::read(&self.wake_rx, &mut bytes).is_ok_and(|count| count > 0) {}
        Ok(self.state.lock().unwrap().events.pop_front())
    }

    fn waker(&mut self) -> io::Result<Waker> {
        Ok(self.waker.clone())
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            damage: true,
            transparency: true,
            pixel_pointer: true,
        }
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .shutdowns += 1;
        Ok(())
    }
}

impl Drop for MockBackend {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drops += 1;
    }
}

fn config() -> EngineConfig {
    let face = |bytes: &[u8]| {
        pixel_core::fontdue::Font::from_bytes(bytes, pixel_core::fontdue::FontSettings::default())
            .unwrap()
    };
    EngineConfig {
        fonts: pixel_core::fonts::FontRegistry::new(
            face(include_bytes!(
                "../../../assets/fonts/JetBrainsMono-Regular.ttf"
            )),
            face(include_bytes!("../../../assets/fonts/InterVariable.ttf")),
        ),
        cell_metrics_font: 0,
    }
}

fn engine() -> (Engine, Arc<Mutex<State>>) {
    let (backend, state) = MockBackend::new(DisplayMetrics {
        width_px: 101,
        height_px: 57,
        scale_factor: 1.0,
    });
    (Engine::new(config(), Box::new(backend)).unwrap(), state)
}

fn pump(engine: &mut Engine) -> Vec<EngineEvent> {
    engine.pump(Some(Duration::ZERO)).unwrap()
}

#[test]
fn native_ui_renders_without_a_terminal_and_updates_after_a_click() {
    let (mut engine, state) = engine();
    assert!(engine.terminal().is_none());
    assert!(engine.cell.is_none());
    assert!(engine.native.is_none());
    engine.comp.views[0].tree.reconcile(Desc {
        key: Some("button".into()),
        clickable: true,
        style: Style {
            background: Some(Paint::Solid([40, 100, 220, 255])),
            ..Style::default()
        },
        ..Desc::default()
    });
    pump(&mut engine);
    assert_eq!(
        state.lock().unwrap().frames.last().unwrap().pixel(100, 56),
        [40, 100, 220, 255]
    );
    state
        .lock()
        .unwrap()
        .events
        .extend([MouseKind::Down, MouseKind::Up].map(|kind| {
            BackendEvent::Mouse(Mouse {
                kind,
                button: MouseButton::Left,
                mods: Mods::default(),
                x: 100,
                y: 56,
            })
        }));
    let events = pump(&mut engine);
    assert!(events.iter().any(|event| matches!(event, EngineEvent::Click { key: Some(key), x: 100.0, y: 56.0, .. } if key == "button")));
    engine.comp.views[0].tree.reconcile(Desc {
        style: Style {
            background: Some(Paint::Solid([20, 180, 70, 255])),
            ..Style::default()
        },
        ..Desc::default()
    });
    pump(&mut engine);
    let state = state.lock().unwrap();
    let frame = state.frames.last().unwrap();
    assert_eq!(frame.size, (101, 57));
    assert_eq!(frame.pixel(100, 56), [20, 180, 70, 255]);
    assert!(!frame.damage.is_empty());
}

#[test]
fn resize_and_scale_keep_exact_pixels_without_cell_rounding() {
    let (mut engine, state) = engine();
    pump(&mut engine);
    state.lock().unwrap().metrics = DisplayMetrics {
        width_px: 113,
        height_px: 73,
        scale_factor: 1.5,
    };
    state.lock().unwrap().events.push_back(BackendEvent::Resize);
    let events = pump(&mut engine);
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::Resize {
            width: 113,
            height: 73,
            base_px: 24.0,
            scale_factor: 1.5,
            cell: None,
            ..
        }
    )));
    assert_eq!(engine.comp.window, (113, 73));
    pump(&mut engine);
    assert_eq!(state.lock().unwrap().frames.last().unwrap().size, (113, 73));
    state.lock().unwrap().metrics.scale_factor = 2.0;
    let events = pump(&mut engine);
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::Resize {
            width: 113,
            height: 73,
            base_px: 32.0,
            scale_factor: 2.0,
            cell: None,
            ..
        }
    )));
}

#[test]
fn redraw_requests_restore_an_unchanged_output_without_repainting_on_resize_reports() {
    let (mut engine, state) = engine();
    pump(&mut engine);
    let frames = state.lock().unwrap().frames.len();
    state.lock().unwrap().events.push_back(BackendEvent::Resize);
    pump(&mut engine);
    pump(&mut engine);
    assert_eq!(state.lock().unwrap().frames.len(), frames);
    state
        .lock()
        .unwrap()
        .events
        .push_back(BackendEvent::RedrawRequested);
    pump(&mut engine);
    pump(&mut engine);
    assert_eq!(state.lock().unwrap().frames.len(), frames + 1);
}

#[test]
fn normalized_keyboard_paste_focus_and_wheel_reach_the_existing_ui() {
    let (mut engine, state) = engine();
    engine.comp.views[0].tree.reconcile(Desc {
        key: Some("input".into()),
        input: Some(InputProps {
            auto_focus: true,
            ..InputProps::default()
        }),
        wheel_events: true,
        ..Desc::default()
    });
    let input = engine.comp.views[0].tree.root();
    engine.set_focus(0, Some(input));
    pump(&mut engine);
    state.lock().unwrap().events.extend([
        BackendEvent::Key(KeyEvent {
            key: Key::Char('a'),
            mods: Mods::default(),
            kind: KeyKind::Press,
            text: Some("a".into()),
        }),
        BackendEvent::Paste("bc".into()),
        BackendEvent::Focus(false),
        BackendEvent::Wheel {
            x: 10,
            y: 10,
            delta_x: 1.25,
            delta_y: 2.5,
            mods: Mods::default(),
        },
    ]);
    let events = pump(&mut engine);
    let root = engine.comp.views[0].tree.root();
    assert_eq!(engine.comp.views[0].tree.input_text(root), Some("abc"));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::Change { text, .. } if text == "abc"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::Focus { focused: false }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::Wheel {
            delta_x: 1.25,
            delta_y: 2.5,
            precise: true,
            ..
        }
    )));
}

#[test]
fn browser_surfaces_use_the_same_compositor_and_damage_path() {
    let (mut engine, state) = engine();
    let surface = 19_000_001;
    engine.comp.views[0].tree.reconcile(Desc {
        children: vec![Desc {
            surface: Some(surface),
            style: Style {
                width: Dimension::Px(4.0),
                height: Dimension::Px(4.0),
                ..Style::default()
            },
            ..Desc::default()
        }],
        ..Desc::default()
    });
    let mut bgra = [0, 0, 255, 255].repeat(16);
    engine
        .ingest_surface(surface, 4, 4, &bgra, 16, None)
        .unwrap();
    pump(&mut engine);
    assert_eq!(
        state.lock().unwrap().frames.last().unwrap().pixel(1, 1),
        [255, 0, 0, 255]
    );
    bgra[20..24].copy_from_slice(&[255, 0, 0, 255]);
    engine
        .ingest_surface(
            surface,
            4,
            4,
            &bgra,
            16,
            Some(&[Rect {
                x: 1,
                y: 1,
                w: 1,
                h: 1,
            }]),
        )
        .unwrap();
    pump(&mut engine);
    let state = state.lock().unwrap();
    let frame = state.frames.last().unwrap();
    assert_eq!(frame.pixel(1, 1), [0, 0, 255, 255]);
    assert_eq!(frame.pixel(0, 0), [255, 0, 0, 255]);
    assert!(
        frame
            .damage
            .iter()
            .any(|rect| rect.x <= 1 && rect.y <= 1 && rect.x + rect.w > 1 && rect.y + rect.h > 1)
    );
    engine.delete_surface(surface).unwrap();
}

#[test]
fn wake_interrupts_the_backend_wait_without_an_input_event() {
    let (mut engine, _) = engine();
    pump(&mut engine);
    let waker = engine.waker().unwrap();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        waker.wake();
    });
    let started = Instant::now();
    engine.pump(Some(Duration::from_secs(5))).unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    thread.join().unwrap();
}

#[test]
fn shutdown_runs_on_close_and_after_presentation_errors() {
    for fail in [false, true] {
        let (mut engine, state) = engine();
        if fail {
            state.lock().unwrap().fail_present = true;
            assert!(engine.pump(Some(Duration::ZERO)).is_err());
        } else {
            state
                .lock()
                .unwrap()
                .events
                .push_back(BackendEvent::CloseRequested);
            assert!(
                pump(&mut engine)
                    .iter()
                    .any(|event| matches!(event, EngineEvent::CloseRequested))
            );
        }
        drop(engine);
        let state = state.lock().unwrap();
        assert_eq!(state.shutdowns, 1);
        assert_eq!(state.drops, 1);
    }
}

#[test]
fn invalid_metrics_fail_before_rendering_and_drop_the_backend() {
    for metrics in [
        DisplayMetrics {
            width_px: 0,
            height_px: 57,
            scale_factor: 1.0,
        },
        DisplayMetrics {
            width_px: 101,
            height_px: 57,
            scale_factor: f32::NAN,
        },
        DisplayMetrics {
            width_px: 101,
            height_px: 57,
            scale_factor: 0.0,
        },
    ] {
        let (backend, state) = MockBackend::new(metrics);
        assert!(Engine::new(config(), Box::new(backend)).is_err());
        let state = state.lock().unwrap();
        assert!(state.frames.is_empty());
        assert_eq!(state.drops, 1);
    }
}
