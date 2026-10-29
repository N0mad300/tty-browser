use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use xcursor::{CursorTheme, parser::Image};

use super::cursor_settings::CursorSettings;

struct CursorFrame {
    width: u32,
    height: u32,
    hotspot: (u32, u32),
    pixels: Vec<u32>,
    delay: Duration,
}

pub(super) struct Cursor {
    themes: Vec<CursorTheme>,
    size: u32,
    cache: BTreeMap<String, Arc<[CursorFrame]>>,
    frames: Arc<[CursorFrame]>,
    shape: String,
    frame: usize,
    due: Option<Instant>,
}

impl Cursor {
    pub fn new(scale: f32) -> Self {
        Self::from_settings(CursorSettings::load(), scale)
    }

    fn from_settings(settings: CursorSettings, scale: f32) -> Self {
        let mut cursor = Self {
            themes: vec![
                CursorTheme::load(&settings.theme),
                CursorTheme::load("default"),
            ],
            size: ((settings.size as f32 * scale).round() as u32).clamp(1, 1024),
            cache: BTreeMap::new(),
            frames: vec![builtin()].into(),
            shape: String::new(),
            frame: 0,
            due: None,
        };
        cursor.set_shape("default", Instant::now());
        cursor
    }

    pub fn set_shape(&mut self, shape: &str, now: Instant) -> bool {
        if shape == self.shape {
            return false;
        }
        self.shape = shape.into();
        self.frame = 0;
        if shape == "none" {
            self.due = None;
            return true;
        }
        let frames = self.cache.get(shape).cloned().unwrap_or_else(|| {
            let frames = load(&self.themes, shape, self.size)
                .map(Arc::from)
                .unwrap_or_else(|| {
                    self.cache
                        .get("default")
                        .cloned()
                        .unwrap_or_else(|| vec![builtin()].into())
                });
            self.cache.insert(shape.into(), frames.clone());
            frames
        });
        self.frames = frames;
        self.due = (self.frames.len() > 1).then(|| now + self.frames[0].delay);
        true
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.due
    }

    pub fn tick(&mut self, now: Instant) -> bool {
        if self.due.is_none_or(|due| now < due) {
            return false;
        }
        self.frame = (self.frame + 1) % self.frames.len();
        self.due = Some(now + self.frames[self.frame].delay);
        true
    }

    pub fn draw(&self, target: &mut [u8], pitch: usize, size: (u32, u32), point: (u32, u32)) {
        if self.shape == "none" {
            return;
        }
        let frame = &self.frames[self.frame];
        let left = i64::from(point.0) - i64::from(frame.hotspot.0);
        let top = i64::from(point.1) - i64::from(frame.hotspot.1);
        for y in 0..frame.height {
            let py = top + i64::from(y);
            if py < 0 || py >= i64::from(size.1) {
                continue;
            }
            for x in 0..frame.width {
                let px = left + i64::from(x);
                if px < 0 || px >= i64::from(size.0) {
                    continue;
                }
                let argb = frame.pixels[(y * frame.width + x) as usize];
                let alpha = argb >> 24;
                let start = py as usize * pitch + px as usize * 4;
                for (channel, shift) in [0, 8, 16].into_iter().enumerate() {
                    let source = (argb >> shift) & 255;
                    let background = u32::from(target[start + channel]);
                    target[start + channel] =
                        (source + (background * (255 - alpha) + 127) / 255).min(255) as u8;
                }
            }
        }
    }
}

fn load(themes: &[CursorTheme], shape: &str, size: u32) -> Option<Vec<CursorFrame>> {
    for theme in themes {
        let mut paths = std::iter::once(shape)
            .chain(aliases(shape).iter().copied())
            .filter_map(|name| theme.load_icon_with_depth(name))
            .collect::<Vec<_>>();
        paths.sort_by_key(|(_, depth)| *depth);
        for (path, _) in paths {
            let Ok(file) = std::fs::File::open(path) else {
                continue;
            };
            let mut bytes = Vec::new();
            if file
                .take(16 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .is_err()
                || bytes.len() > 16 * 1024 * 1024
            {
                continue;
            }
            if let Some(images) = parse(&bytes)
                && let Some(frames) = select(images, size)
            {
                return Some(frames);
            }
        }
    }
    None
}

fn parse(bytes: &[u8]) -> Option<Vec<Image>> {
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
        ))
    };
    let header = word(4)? as usize;
    let count = word(12)? as usize;
    if count > 1024 {
        return None;
    }
    for at in 0..count {
        let toc = header.checked_add(at * 12)?;
        if word(toc)? != 0xfffd0002 {
            continue;
        }
        let pos = word(toc + 8)? as usize;
        let (width, height) = (word(pos + 16)?, word(pos + 20)?);
        if width == 0 || height == 0 || width > 1024 || height > 1024 {
            return None;
        }
        bytes.get(pos + 36..(pos + 36).checked_add(width as usize * height as usize * 4)?)?;
    }
    xcursor::parser::parse_xcursor(bytes)
}

fn select(images: Vec<Image>, size: u32) -> Option<Vec<CursorFrame>> {
    let nominal = images
        .iter()
        .min_by_key(|image| (image.size.abs_diff(size), std::cmp::Reverse(image.size)))?
        .size;
    let frames = images
        .into_iter()
        .filter(|image| image.size == nominal)
        .filter(|image| image.xhot < image.width && image.yhot < image.height)
        .map(|image| CursorFrame {
            width: image.width,
            height: image.height,
            hotspot: (image.xhot, image.yhot),
            pixels: image
                .pixels_rgba
                .chunks_exact(4)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
                .collect(),
            delay: Duration::from_millis(u64::from(image.delay.max(10))),
        })
        .collect::<Vec<_>>();
    (!frames.is_empty()).then_some(frames)
}

fn aliases(shape: &str) -> &'static [&'static str] {
    match shape {
        "default" => &["left_ptr", "arrow"],
        "pointer" => &["pointing_hand", "hand2", "hand1"],
        "text" => &["xterm"],
        "wait" => &["watch"],
        "progress" => &["left_ptr_watch"],
        "crosshair" => &["cross"],
        "grab" => &["openhand"],
        "grabbing" => &["closedhand"],
        "move" | "all-scroll" => &["fleur"],
        "not-allowed" | "no-drop" => &["crossed_circle"],
        "ew-resize" => &["sb_h_double_arrow", "h_double_arrow"],
        "ns-resize" => &["sb_v_double_arrow", "v_double_arrow"],
        "n-resize" => &["top_side"],
        "s-resize" => &["bottom_side"],
        "e-resize" => &["right_side"],
        "w-resize" => &["left_side"],
        "nw-resize" => &["top_left_corner"],
        "ne-resize" => &["top_right_corner"],
        "sw-resize" => &["bottom_left_corner"],
        "se-resize" => &["bottom_right_corner"],
        "nwse-resize" => &["bd_double_arrow"],
        "nesw-resize" => &["fd_double_arrow"],
        _ => &[],
    }
}

fn builtin() -> CursorFrame {
    let mut pixels = vec![0; 16 * 16];
    for y in 0..16 {
        for x in 0..=y / 2 {
            pixels[y * 16 + x] = if x == 0 || x == y / 2 || y == 15 {
                0xff000000
            } else {
                0xffffffff
            };
        }
    }
    CursorFrame {
        width: 16,
        height: 16,
        hotspot: (0, 0),
        pixels,
        delay: Duration::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ThemeFixture(PathBuf);

    impl ThemeFixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "pixel-cursor-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::create_dir(path.join("cursors")).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, images: &[(u32, u32, u32)]) {
            std::fs::write(self.0.join("cursors").join(name), encoded(images)).unwrap();
        }

        fn load(&self) -> CursorTheme {
            CursorTheme::load(self.0.to_str().unwrap())
        }
    }

    impl Drop for ThemeFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn encoded(images: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut words = vec![0x72756358, 16, 0x10000, images.len() as u32];
        for (index, &(size, _, _)) in images.iter().enumerate() {
            words.extend([
                0xfffd0002,
                size,
                16 + images.len() as u32 * 12 + index as u32 * 40,
            ]);
        }
        for &(size, pixel, delay) in images {
            words.extend([36, 0xfffd0002, size, 1, 1, 1, 0, 0, delay, pixel]);
        }
        words.into_iter().flat_map(u32::to_le_bytes).collect()
    }

    fn cursor(frames: Vec<CursorFrame>) -> Cursor {
        let frames: Arc<[CursorFrame]> = frames.into();
        Cursor {
            themes: Vec::new(),
            size: 24,
            cache: BTreeMap::from([("default".into(), frames.clone())]),
            frames,
            shape: "default".into(),
            frame: 0,
            due: None,
        }
    }

    #[test]
    fn raster_loading_selects_nearest_size_and_retains_argb_and_animation() {
        let fixture = ThemeFixture::new();
        fixture.write(
            "default",
            &[
                (16, 0xff112233, 0),
                (32, 0x80402010, 20),
                (32, 0xff998877, 80),
            ],
        );
        let frames = load(&[fixture.load()], "default", 24).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].pixels, [0x80402010]);
        assert_eq!(frames[0].delay, Duration::from_millis(20));
        assert_eq!(frames[1].delay, Duration::from_millis(80));
        assert!(load(&[], "default", 24).is_none());
        let smaller = load(&[fixture.load()], "default", 16).unwrap();
        assert_eq!(smaller[0].pixels, [0xff112233]);
    }

    #[test]
    fn own_legacy_shape_wins_over_inherited_modern_name() {
        let parent = ThemeFixture::new();
        parent.write("pointer", &[(24, 0xffff0000, 0)]);
        std::fs::write(
            parent.0.join("index.theme"),
            format!("[Icon Theme]\nInherits={}\n", parent.0.display()),
        )
        .unwrap();
        let child = ThemeFixture::new();
        child.write("hand2", &[(24, 0xff0000ff, 0)]);
        std::fs::write(
            child.0.join("index.theme"),
            format!("[Icon Theme]\nInherits={}\n", parent.0.display()),
        )
        .unwrap();
        let frames = load(&[child.load()], "pointer", 24).unwrap();
        assert_eq!(frames[0].pixels, [0xff0000ff]);
        assert!(load(&[child.load()], "text", 24).is_none());
    }

    #[test]
    fn hotspots_and_premultiplied_alpha_preserve_background_and_pitch() {
        let cursor = cursor(vec![CursorFrame {
            width: 2,
            height: 2,
            hotspot: (1, 1),
            pixels: vec![0xffffffff, 0, 0, 0x80402010],
            delay: Duration::ZERO,
        }]);
        let mut target = vec![99; 24];
        for start in [0, 4, 12, 16] {
            target[start..start + 4].copy_from_slice(&[100, 100, 100, 255]);
        }
        cursor.draw(&mut target, 12, (2, 2), (0, 0));
        assert_eq!(&target[..4], &[66, 82, 114, 255]);
        assert_eq!(&target[4..8], &[100, 100, 100, 255]);
        assert_eq!(&target[8..12], &[99; 4]);
        assert_eq!(&target[12..20], &[100, 100, 100, 255, 100, 100, 100, 255]);
        cursor.draw(&mut target, 12, (2, 2), (u32::MAX, u32::MAX));
        assert_eq!(&target[..4], &[66, 82, 114, 255]);
    }

    #[test]
    fn animation_advances_when_idle_and_hidden_or_missing_shapes_remain_safe() {
        let mut cursor = cursor(vec![builtin()]);
        let frames = select(
            parse(&encoded(&[(24, 0xffff0000, 20), (24, 0xff0000ff, 80)])).unwrap(),
            24,
        )
        .unwrap();
        cursor.cache.insert("wait".into(), frames.into());
        let now = Instant::now();
        assert!(cursor.set_shape("wait", now));
        assert!(!cursor.tick(now + Duration::from_millis(19)));
        assert!(cursor.tick(now + Duration::from_millis(20)));
        assert_eq!(cursor.frames[cursor.frame].pixels, [0xff0000ff]);
        assert_eq!(cursor.deadline(), Some(now + Duration::from_millis(100)));
        assert!(cursor.set_shape("none", now));
        assert!(cursor.deadline().is_none());
        let mut target = [99; 4];
        cursor.draw(&mut target, 4, (1, 1), (0, 0));
        assert_eq!(target, [99; 4]);
        assert!(cursor.set_shape("missing-shape", now));
        assert_eq!(cursor.frames[0].width, 16);
        assert!(cursor.deadline().is_none());
        assert!(!cursor.set_shape("missing-shape", now));
    }

    #[test]
    fn truncated_or_excessive_cursor_files_are_rejected_before_allocation() {
        let bytes = encoded(&[(24, 0xff123456, 0)]);
        assert!(parse(&bytes[..bytes.len() - 1]).is_none());
        let mut large = bytes.clone();
        large[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse(&large).is_none());
        let mut count = bytes;
        count[12..16].copy_from_slice(&2048u32.to_le_bytes());
        assert!(parse(&count).is_none());
    }
}
