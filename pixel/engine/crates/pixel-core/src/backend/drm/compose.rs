#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::ptr::NonNull;

use xkbcommon_dl as xkb;

pub(super) enum ComposedText {
    Unchanged,
    Pending,
    Text(String),
}

pub(super) struct Compose {
    api: &'static xkb::XkbCommonCompose,
    state: NonNull<xkb::xkb_compose_state>,
}

impl Compose {
    pub fn new(context: NonNull<xkb::xkb_context>) -> io::Result<Self> {
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|value| !value.is_empty())
            .unwrap_or_else(|| "C.UTF-8".into());
        let locale = CString::new(locale).map_err(io::Error::other)?;
        let api = xkb::xkbcommon_compose_option()
            .ok_or_else(|| io::Error::other("libxkbcommon compose support is required"))?;
        unsafe {
            let table = NonNull::new((api.xkb_compose_table_new_from_locale)(
                context.as_ptr(),
                locale.as_ptr(),
                xkb::xkb_compose_compile_flags::XKB_COMPOSE_COMPILE_NO_FLAGS,
            ))
            .ok_or_else(|| io::Error::other("could not load the keyboard compose table"))?;
            let state = NonNull::new((api.xkb_compose_state_new)(
                table.as_ptr(),
                xkb::xkb_compose_state_flags::XKB_COMPOSE_STATE_NO_FLAGS,
            ));
            (api.xkb_compose_table_unref)(table.as_ptr());
            Ok(Self {
                api,
                state: state
                    .ok_or_else(|| io::Error::other("could not create keyboard compose state"))?,
            })
        }
    }

    pub fn feed(&mut self, symbol: u32) -> ComposedText {
        unsafe {
            (self.api.xkb_compose_state_feed)(self.state.as_ptr(), symbol);
            match (self.api.xkb_compose_state_get_status)(self.state.as_ptr()) {
                xkb::xkb_compose_status::XKB_COMPOSE_NOTHING => ComposedText::Unchanged,
                xkb::xkb_compose_status::XKB_COMPOSE_COMPOSING => ComposedText::Pending,
                xkb::xkb_compose_status::XKB_COMPOSE_CANCELLED => {
                    (self.api.xkb_compose_state_reset)(self.state.as_ptr());
                    ComposedText::Pending
                }
                xkb::xkb_compose_status::XKB_COMPOSE_COMPOSED => {
                    let mut text = [0u8; 128];
                    let length = (self.api.xkb_compose_state_get_utf8)(
                        self.state.as_ptr(),
                        text.as_mut_ptr().cast(),
                        text.len(),
                    );
                    (self.api.xkb_compose_state_reset)(self.state.as_ptr());
                    if length > 0 && (length as usize) < text.len() {
                        ComposedText::Text(
                            String::from_utf8_lossy(&text[..length as usize]).into_owned(),
                        )
                    } else {
                        ComposedText::Pending
                    }
                }
            }
        }
    }
}

impl Drop for Compose {
    fn drop(&mut self) {
        unsafe {
            (self.api.xkb_compose_state_unref)(self.state.as_ptr());
        }
    }
}
