use std::io;

use napi_derive::napi;
use pixel_core::backend::{Backend, KittyBackend, KittyConfig};

#[napi(object)]
#[derive(Clone)]
pub struct BackendOptions {
    pub name: String,
    pub card: Option<String>,
    pub input_devices: Option<Vec<String>>,
    pub connector: Option<String>,
    pub scale_factor: Option<f64>,
}

impl BackendOptions {
    pub fn kitty() -> Self {
        Self {
            name: "kitty".into(),
            card: None,
            input_devices: None,
            connector: None,
            scale_factor: None,
        }
    }

    pub fn validate(&self) -> io::Result<()> {
        match self.name.as_str() {
            "auto" | "kitty" => Ok(()),
            "drm" => {
                if !cfg!(all(target_os = "linux", feature = "drm")) {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "DRM is not in this native build; rebuild Pixel with build:native -- --drm on Linux",
                    ));
                }
                if self.card.as_ref().is_none_or(String::is_empty)
                    || self.input_devices.as_ref().is_none_or(|devices| {
                        devices.is_empty() || devices.iter().any(String::is_empty)
                    })
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "DRM requires a primary card and input devices",
                    ));
                }
                let scale = self.scale_factor.unwrap_or(1.0);
                if !scale.is_finite() || scale <= 0.0 || scale > 4.0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "DRM scale must be greater than zero, up to 4",
                    ));
                }
                Ok(())
            }
            name => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown Pixel backend: {name}"),
            )),
        }
    }
}

pub fn open(options: BackendOptions, kitty: KittyConfig) -> io::Result<Box<dyn Backend>> {
    options.validate()?;
    if options.name != "drm" {
        return Ok(Box::new(KittyBackend::new(kitty)?));
    }
    if kitty.tty.is_some()
        || kitty.host.is_some()
        || kitty.wrapper != pixel_core::wrapper::Wrapper::None
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DRM does not use terminal targets, pane hosts or terminal wrappers",
        ));
    }
    #[cfg(all(target_os = "linux", feature = "drm"))]
    {
        use pixel_core::backend::drm::{DrmBackend, DrmConfig};
        let config = DrmConfig {
            card: options.card.expect("validated card").into(),
            connector: options.connector,
            input_devices: options
                .input_devices
                .expect("validated input devices")
                .into_iter()
                .map(Into::into)
                .collect(),
            scale_factor: options.scale_factor.unwrap_or(1.0) as f32,
        };
        return Ok(Box::new(DrmBackend::new(config)?));
    }
    #[cfg(not(all(target_os = "linux", feature = "drm")))]
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_selection_rejects_unknown_and_unavailable_outputs_before_opening_devices() {
        let mut options = BackendOptions::kitty();
        assert!(options.validate().is_ok());
        options.name = "auto".into();
        assert!(options.validate().is_ok());
        options.name = "wayland".into();
        assert_eq!(
            options.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        options.name = "drm".into();
        let error = options.validate().unwrap_err();
        assert_eq!(
            error.kind(),
            if cfg!(all(target_os = "linux", feature = "drm")) {
                io::ErrorKind::InvalidInput
            } else {
                io::ErrorKind::Unsupported
            }
        );
    }

    #[cfg(all(target_os = "linux", feature = "drm"))]
    #[test]
    fn drm_configuration_is_validated_without_opening_a_session() {
        let mut options = BackendOptions {
            name: "drm".into(),
            card: Some("/dev/dri/card1".into()),
            input_devices: Some(vec!["/dev/input/event2".into()]),
            connector: None,
            scale_factor: Some(1.5),
        };
        assert!(options.validate().is_ok());
        options.scale_factor = Some(f64::NAN);
        assert!(options.validate().is_err());
        options.scale_factor = Some(1.0);
        options.input_devices = Some(vec![String::new()]);
        assert!(options.validate().is_err());
    }
}

#[napi]
pub fn check_backend(options: BackendOptions) -> napi::Result<()> {
    options.validate().map_err(crate::err)?;
    if options.name == "drm" {
        #[cfg(all(target_os = "linux", feature = "drm"))]
        pixel_core::backend::drm::DrmBackend::check_session().map_err(crate::err)?;
    }
    Ok(())
}
