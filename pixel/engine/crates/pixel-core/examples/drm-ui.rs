#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use pixel_core::backend::drm::{DrmBackend, DrmConfig};
    use pixel_core::backend::{Backend, BackendEvent};
    use pixel_core::surfaces::Rect;
    use pixel_core::{
        Canvas, Desc, Dimension, Edges, Engine, EngineConfig, EngineEvent, Frame, InputProps, Key,
        KeyKind, Paint, Style,
    };

    enum Command {
        Help,
        Check,
        Run { config: DrmConfig, pattern: bool },
    }

    fn arguments(arguments: impl IntoIterator<Item = String>) -> io::Result<Command> {
        let mut arguments = arguments.into_iter();
        let mut card = None;
        let mut connector = None;
        let mut input_devices = Vec::new();
        let mut scale_factor = 1.0;
        let mut pattern = false;
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--help" | "-h" => return Ok(Command::Help),
                "--check" => {
                    if arguments.next().is_some()
                        || card.is_some()
                        || connector.is_some()
                        || !input_devices.is_empty()
                        || pattern
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--check must be used alone",
                        ));
                    }
                    return Ok(Command::Check);
                }
                "--pattern" => pattern = true,
                "--card" | "--connector" | "--input" | "--scale" => {
                    let value = arguments.next().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("{argument} needs a value"),
                        )
                    })?;
                    match argument.as_str() {
                        "--card" => card = Some(PathBuf::from(value)),
                        "--connector" => connector = Some(value),
                        "--input" => input_devices.push(PathBuf::from(value)),
                        "--scale" => {
                            scale_factor = value.parse().map_err(|_| {
                                io::Error::new(io::ErrorKind::InvalidInput, "invalid scale")
                            })?
                        }
                        _ => unreachable!(),
                    }
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown argument {argument}"),
                    ));
                }
            }
        }
        let card = card.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "choose a primary DRM device with --card /dev/dri/cardN; use --help for usage",
            )
        })?;
        Ok(Command::Run {
            config: DrmConfig {
                card,
                connector,
                input_devices,
                scale_factor,
            },
            pattern,
        })
    }

    pub fn run() -> io::Result<()> {
        match arguments(std::env::args().skip(1))? {
            Command::Help => {
                println!(
                    "usage: drm-ui --check\n       drm-ui --card /dev/dri/cardN --input /dev/input/eventN [--input /dev/input/eventN] [--connector HDMI-A-1] [--scale 1] [--pattern]\n\nRun from your own active local Linux VT with LIBSEAT_BACKEND=logind.\n--check only checks the session and VT; it does not acquire the display.\n--pattern shows moving color bars before testing the native UI.\nEscape exits. Ctrl+Alt+F1..F12 switches VTs. Keyboard plus relative mouse only."
                );
                Ok(())
            }
            Command::Check => {
                let info = DrmBackend::check_session()?;
                println!(
                    "eligible local session: {}, seat {}, VT {}",
                    info.session, info.seat, info.vt
                );
                Ok(())
            }
            Command::Run {
                config,
                pattern: true,
            } => pattern_demo(DrmBackend::new(config)?),
            Command::Run {
                config,
                pattern: false,
            } => native_demo(DrmBackend::new(config)?),
        }
    }

    fn pattern_demo(mut backend: DrmBackend) -> io::Result<()> {
        let started = Instant::now();
        let mut due = Instant::now();
        loop {
            if Instant::now() >= due {
                let metrics = backend.metrics()?;
                let mut canvas = Canvas::new(metrics.width_px, metrics.height_px);
                let colors = [
                    [255, 0, 0, 255],
                    [0, 255, 0, 255],
                    [0, 0, 255, 255],
                    [255, 255, 255, 255],
                ];
                for (index, color) in colors.into_iter().enumerate() {
                    let start = metrics.width_px * index as u32 / 4;
                    let end = metrics.width_px * (index as u32 + 1) / 4;
                    canvas.fill_rect(start, 0, end - start, metrics.height_px, color);
                }
                let x = ((started.elapsed().as_secs_f32() * 180.0) as u32) % metrics.width_px;
                canvas.fill_rect(x, metrics.height_px / 2, 64, 64, [10, 10, 10, 255]);
                let damage = [Rect::sized(metrics.width_px, metrics.height_px)];
                backend.present(Frame {
                    canvas: &canvas,
                    premultiplied: false,
                    changed: &damage,
                    repainted: &[],
                    opaque: &[],
                    ui_over_surfaces: &[],
                })?;
                due = Instant::now() + Duration::from_millis(100);
            }
            if let Some(event) =
                backend.poll_event(Some(due.saturating_duration_since(Instant::now())))?
            {
                match event {
                    BackendEvent::CloseRequested => break,
                    BackendEvent::Key(key)
                        if key.key == Key::Escape && key.kind == KeyKind::Press =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
        }
        backend.shutdown()
    }

    fn fonts() -> pixel_core::fonts::FontRegistry {
        let face = |bytes: &[u8]| {
            pixel_core::fontdue::Font::from_bytes(
                bytes,
                pixel_core::fontdue::FontSettings::default(),
            )
            .expect("bundled font parses")
        };
        pixel_core::fonts::FontRegistry::new(
            face(include_bytes!(
                "../../../assets/fonts/JetBrainsMono-Regular.ttf"
            )),
            face(include_bytes!("../../../assets/fonts/InterVariable.ttf")),
        )
    }

    fn native_demo(backend: DrmBackend) -> io::Result<()> {
        let mut engine = Engine::new(
            EngineConfig {
                fonts: fonts(),
                cell_metrics_font: 0,
            },
            Box::new(backend),
        )?;
        engine.set_max_fps(60.0);
        let mut counter = 0;
        let mut typed = String::new();
        engine.comp.views[0]
            .tree
            .reconcile(scene(counter, &typed, engine.display.scale_factor));
        loop {
            let mut changed = false;
            for event in engine.pump(None)? {
                match event {
                    EngineEvent::CloseRequested => return Ok(()),
                    EngineEvent::Key { event, .. }
                        if event.kind == KeyKind::Press && event.key == Key::Escape =>
                    {
                        return Ok(());
                    }
                    EngineEvent::Key { event, .. }
                        if event.kind != KeyKind::Release && event.key == Key::Char(' ') =>
                    {
                        counter += 1;
                        changed = true;
                    }
                    EngineEvent::Click { key: Some(key), .. } if key == "increment" => {
                        counter += 1;
                        changed = true;
                    }
                    EngineEvent::Change { text, .. } => {
                        typed = text;
                        changed = true;
                    }
                    _ => {}
                }
            }
            if changed {
                engine.comp.views[0].tree.reconcile(scene(
                    counter,
                    &typed,
                    engine.display.scale_factor,
                ));
            }
        }
    }

    fn scene(counter: usize, typed: &str, scale: f32) -> Desc {
        let label = |text: String, size| Desc {
            text: Some(text),
            style: Style {
                font_size: Some(size * scale),
                ..Style::default()
            },
            ..Desc::default()
        };
        Desc {
            style: Style {
                background: Some(Paint::Solid([16, 23, 36, 255])),
                color: Some([235, 240, 250, 255]),
                padding: Edges::all(32.0 * scale),
                gap: 20.0 * scale,
                ..Style::default()
            },
            children: vec![
                label("Pixel native UI · DRM/KMS".into(), 32.0),
                label(
                    "Click the button or press Space. Type in the field. Escape exits.".into(),
                    18.0,
                ),
                label(
                    "Ctrl+Alt+F1..F12 switches VTs. Output must restore when you exit.".into(),
                    16.0,
                ),
                Desc {
                    key: Some("increment".into()),
                    clickable: true,
                    style: Style {
                        width: Dimension::Px(360.0 * scale),
                        height: Dimension::Px(88.0 * scale),
                        padding: Edges::all(24.0 * scale),
                        background: Some(Paint::Solid(if counter % 2 == 0 {
                            [40, 95, 200, 255]
                        } else {
                            [25, 135, 95, 255]
                        })),
                        hover_background: Some([70, 115, 220, 255]),
                        corner_radius: [10.0 * scale; 4],
                        ..Style::default()
                    },
                    children: vec![label(format!("Count: {counter}"), 24.0)],
                    ..Desc::default()
                },
                Desc {
                    key: Some("typing".into()),
                    input: Some(InputProps::default()),
                    style: Style {
                        width: Dimension::Px(500.0 * scale),
                        height: Dimension::Px(56.0 * scale),
                        padding: Edges::all(12.0 * scale),
                        background: Some(Paint::Solid([33, 44, 62, 255])),
                        ..Style::default()
                    },
                    ..Desc::default()
                },
                label(format!("Typed: {typed}"), 18.0),
            ],
            ..Desc::default()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn help_and_check_do_not_construct_a_backend() {
            assert!(matches!(
                arguments(["--help".into()]).unwrap(),
                Command::Help
            ));
            assert!(matches!(
                arguments(["--check".into()]).unwrap(),
                Command::Check
            ));
            assert!(arguments(["--check".into(), "--pattern".into()]).is_err());
            assert!(arguments(Vec::<String>::new()).is_err());
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = linux::run() {
        eprintln!("pixel DRM: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the DRM demo requires Linux");
    std::process::exit(1);
}
