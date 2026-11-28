//! Core options surfaced to the frontend (RetroArch: Quick Menu → Core
//! Options). Values arrive as text, so reads parse, default, and clamp.

use std::ffi::{CStr, CString};
use std::ptr;

use crate::ffi::{
    EnvironmentCallback, RetroVariable, ENVIRONMENT_GET_VARIABLE, ENVIRONMENT_SET_CORE_OPTIONS,
};

pub const OPTION_VOLUME: &CStr = c"mrpemu_volume";

/// A parsed snapshot of the core options the frontend reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreOptions {
    pub volume: u32,
}

impl Default for CoreOptions {
    fn default() -> Self {
        Self { volume: 100 }
    }
}

fn option_variables() -> [RetroVariable; 2] {
    [
        RetroVariable {
            key: OPTION_VOLUME.as_ptr(),
            value: c"Audio Volume (%); 100|90|80|70|60|50|40|30|20|10|0"
                .as_ptr()
                .cast_mut(),
        },
        RetroVariable {
            key: ptr::null(),
            value: ptr::null_mut(),
        },
    ]
}

/// Registers the option definitions with the frontend.
pub fn set_core_options(callback: EnvironmentCallback) -> bool {
    let variables = option_variables();
    crate::call_environment(
        callback,
        ENVIRONMENT_SET_CORE_OPTIONS,
        variables.as_ptr().cast_mut().cast(),
    )
}

/// Reads one option value through the frontend; `None` when unset.
pub fn get_core_option(callback: EnvironmentCallback, key: &CStr) -> Option<String> {
    let key = CString::new(key.to_bytes()).ok()?;
    let mut variable = RetroVariable {
        key: key.as_ptr(),
        value: ptr::null_mut(),
    };
    if !crate::call_environment(
        callback,
        ENVIRONMENT_GET_VARIABLE,
        ptr::from_mut(&mut variable).cast(),
    ) {
        return None;
    }
    if variable.value.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(variable.value) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Returns whether any option changed since the last poll.
pub fn core_options_changed(callback: EnvironmentCallback) -> bool {
    let mut updated = false;
    if crate::call_environment(
        callback,
        crate::ffi::ENVIRONMENT_GET_VARIABLE_UPDATE,
        ptr::from_mut(&mut updated).cast(),
    ) {
        updated
    } else {
        false
    }
}

/// Parses the frontend-reported values into a typed snapshot with defaults
/// and clamping: unset and malformed values fall back to the defaults, and
/// out-of-range numbers clamp instead of failing the run.
pub fn read_core_options(callback: EnvironmentCallback) -> CoreOptions {
    let mut options = CoreOptions::default();
    if let Some(volume) = get_core_option(callback, OPTION_VOLUME) {
        options.volume = volume.trim().parse::<u32>().unwrap_or(100).min(100);
    }
    options
}
