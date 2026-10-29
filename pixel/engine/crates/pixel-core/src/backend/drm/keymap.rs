use std::collections::BTreeMap;
use std::ffi::CString;
use std::io;

#[derive(Default)]
pub(super) struct KeymapConfig {
    pub rules: Option<CString>,
    pub model: Option<CString>,
    pub layout: Option<CString>,
    pub variant: Option<CString>,
    pub options: Option<CString>,
}

impl KeymapConfig {
    pub fn load() -> io::Result<Self> {
        let mut files = Vec::new();
        for path in ["/etc/vconsole.conf", "/etc/default/keyboard"] {
            match std::fs::read_to_string(path) {
                Ok(contents) => files.push(contents),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io::Error::new(error.kind(), format!("{path}: {error}"))),
            }
        }
        Self::resolve(&files, |key| std::env::var(key).ok())
    }

    fn resolve(files: &[String], env: impl Fn(&str) -> Option<String>) -> io::Result<Self> {
        let mut values = BTreeMap::new();
        for file in files {
            for line in file.lines() {
                let Some((key, value)) = line.trim().split_once('=') else {
                    continue;
                };
                let value = value.trim();
                let value = if value.starts_with(['\'', '"']) {
                    let quote = value.as_bytes()[0] as char;
                    let Some(end) = value[1..].find(quote) else {
                        continue;
                    };
                    &value[1..end + 1]
                } else {
                    value.split('#').next().unwrap_or_default().trim()
                };
                values.insert(key.trim(), value);
            }
        }
        let get = |name: &str| -> io::Result<Option<CString>> {
            let value = env(&format!("XKB_DEFAULT_{name}")).or_else(|| {
                values
                    .get(format!("XKB{name}").as_str())
                    .map(|value| value.to_string())
            });
            value.map(CString::new).transpose().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("XKB {name} contains a NUL byte"),
                )
            })
        };
        Ok(Self {
            rules: get("RULES")?,
            model: get("MODEL")?,
            layout: get("LAYOUT")?,
            variant: get("VARIANT")?,
            options: get("OPTIONS")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_xkb_settings_are_loaded_without_executing_shell_values() {
        let config = KeymapConfig::resolve(&[
            "KEYMAP=de\nXKBLAYOUT=de".into(),
            "XKBMODEL=\"pc105\"\nXKBLAYOUT='fr' # French\nXKBVARIANT=oss\nXKBOPTIONS=\"\"\nXKBRULES=$(touch /never-run)".into(),
        ], |_| None).unwrap();
        assert_eq!(config.layout.as_deref(), Some(c"fr"));
        assert_eq!(config.model.as_deref(), Some(c"pc105"));
        assert_eq!(config.variant.as_deref(), Some(c"oss"));
        assert_eq!(config.options.as_deref(), Some(c""));
        assert_eq!(config.rules.as_deref(), Some(c"$(touch /never-run)"));
    }

    #[test]
    fn environment_overrides_system_settings_including_empty_options() {
        let config = KeymapConfig::resolve(
            &["XKBLAYOUT=fr\nXKBOPTIONS=ctrl:nocaps".into()],
            |key| match key {
                "XKB_DEFAULT_LAYOUT" => Some("us".into()),
                "XKB_DEFAULT_OPTIONS" => Some(String::new()),
                _ => None,
            },
        )
        .unwrap();
        assert_eq!(config.layout.as_deref(), Some(c"us"));
        assert_eq!(config.options.as_deref(), Some(c""));
        assert!(KeymapConfig::resolve(&[], |_| Some("\0".into())).is_err());
    }
}
