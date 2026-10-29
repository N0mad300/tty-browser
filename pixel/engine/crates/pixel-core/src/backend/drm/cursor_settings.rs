use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq)]
pub(super) struct CursorSettings {
    pub theme: String,
    pub size: u32,
}

impl CursorSettings {
    pub fn load() -> Self {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
        let gtk = config
            .as_ref()
            .map(|config| {
                ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"]
                    .into_iter()
                    .filter_map(|file| std::fs::read_to_string(config.join(file)).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Self::resolve(|key| std::env::var(key).ok(), &gtk, gsettings)
    }

    fn resolve(
        env: impl Fn(&str) -> Option<String>,
        gtk: &[String],
        desktop: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let env_value = |key| env(key).filter(|value| !value.trim().is_empty());
        let theme = env_value("XCURSOR_THEME")
            .or_else(|| env_value("HYPRCURSOR_THEME"))
            .or_else(|| {
                gtk.iter()
                    .find_map(|file| setting(file, "gtk-cursor-theme-name"))
            })
            .or_else(|| desktop("cursor-theme"))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "default".into());
        let size = env_value("XCURSOR_SIZE")
            .and_then(|value| cursor_size(&value))
            .or_else(|| env_value("HYPRCURSOR_SIZE").and_then(|value| cursor_size(&value)))
            .or_else(|| {
                gtk.iter().find_map(|file| {
                    setting(file, "gtk-cursor-theme-size").and_then(|value| cursor_size(&value))
                })
            })
            .or_else(|| desktop("cursor-size").and_then(|value| cursor_size(&value)))
            .unwrap_or(24);
        Self { theme, size }
    }
}

fn cursor_size(value: &str) -> Option<u32> {
    value
        .trim()
        .parse()
        .ok()
        .filter(|size| (1..=256).contains(size))
}

fn setting(file: &str, key: &str) -> Option<String> {
    let mut settings = false;
    for line in file.lines().map(str::trim) {
        if line.starts_with('[') {
            settings = line == "[Settings]";
        }
        if !settings {
            continue;
        }
        if let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            return Some(value.trim().trim_matches(['\'', '"']).to_owned());
        }
    }
    None
}

fn gsettings(key: &str) -> Option<String> {
    let mut command = Command::new("gsettings");
    command.args(["get", "org.gnome.desktop.interface", key]);
    let output = query(command, Duration::from_millis(300))?;
    Some(
        output
            .trim()
            .trim_matches('\'')
            .replace("\\'", "'")
            .replace("\\\\", "\\"),
    )
}

fn query(mut command: Command, timeout: Duration) -> Option<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let until = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return String::from_utf8(child.wait_with_output().ok()?.stdout).ok();
            }
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_environment_wins_without_querying_desktop_settings() {
        let settings = CursorSettings::resolve(
            |key| match key {
                "XCURSOR_THEME" => Some("Sweet-cursors".into()),
                "XCURSOR_SIZE" => Some("32".into()),
                _ => None,
            },
            &[],
            |_| panic!("desktop settings must not be queried"),
        );
        assert_eq!(
            settings,
            CursorSettings {
                theme: "Sweet-cursors".into(),
                size: 32
            }
        );
    }

    #[test]
    fn tty_settings_use_gtk_then_persisted_gnome_settings_then_defaults() {
        let gtk = ["[Other]\ngtk-cursor-theme-name=wrong\n[Settings]\ngtk-cursor-theme-name=GTK\ngtk-cursor-theme-size=28".into()];
        let settings =
            CursorSettings::resolve(|_| None, &gtk, |_| panic!("GTK settings are complete"));
        assert_eq!(
            settings,
            CursorSettings {
                theme: "GTK".into(),
                size: 28
            }
        );
        let settings = CursorSettings::resolve(
            |_| None,
            &[],
            |key| {
                Some(
                    if key == "cursor-theme" {
                        "Sweet-cursors"
                    } else {
                        "24"
                    }
                    .into(),
                )
            },
        );
        assert_eq!(
            settings,
            CursorSettings {
                theme: "Sweet-cursors".into(),
                size: 24
            }
        );
        let settings = CursorSettings::resolve(|_| None, &[], |_| None);
        assert_eq!(
            settings,
            CursorSettings {
                theme: "default".into(),
                size: 24
            }
        );
    }

    #[test]
    fn hyprcursor_settings_are_candidates_and_invalid_sizes_fall_through() {
        let settings = CursorSettings::resolve(
            |key| match key {
                "XCURSOR_SIZE" => Some("999999".into()),
                "HYPRCURSOR_THEME" => Some("Shared-Xcursor-theme".into()),
                "HYPRCURSOR_SIZE" => Some("36".into()),
                _ => None,
            },
            &[],
            |_| None,
        );
        assert_eq!(
            settings,
            CursorSettings {
                theme: "Shared-Xcursor-theme".into(),
                size: 36
            }
        );
    }

    #[test]
    fn unavailable_or_slow_settings_command_does_not_block_startup() {
        assert!(
            query(
                Command::new("/pixel-no-such-settings-program"),
                Duration::from_millis(10)
            )
            .is_none()
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 5"]);
        let start = Instant::now();
        assert!(query(command, Duration::from_millis(20)).is_none());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
