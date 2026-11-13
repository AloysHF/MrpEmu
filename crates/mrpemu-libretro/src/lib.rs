//! Libretro core for the MrpEmu MRP emulator.
//!
//! Presents a loaded `.mrp` application as a 240×320 RGB565 retro console.
//! The guest key model (D-pad, OK, two soft keys, numeric keys) is mapped
//! onto the RetroArch joypad; the mapping is documented through input
//! descriptors.

pub mod ffi;

use std::{
    ffi::{c_char, c_void, CStr, CString},
    fs,
    path::PathBuf,
    ptr,
    sync::{Mutex, MutexGuard},
};

use ffi::*;
use mrpemu_core::{MrpKey, MrpMachine};

/// Aspect ratio of the default 240×320 portrait screen.
const ASPECT_RATIO: f32 = 240.0 / 320.0;

/// Frame rate reported until the VM drives real timing.
const FRAMES_PER_SECOND: f64 = mrpemu_core::emulator::PLACEHOLDER_FPS;

/// Sample rate reported until audio exists.
const AUDIO_SAMPLE_RATE: f64 = 22_050.0;

#[derive(Default)]
struct Callbacks {
    environment: EnvironmentCallback,
    video_refresh: VideoRefreshCallback,
    audio_sample_batch: AudioSampleBatchCallback,
    input_poll: InputPollCallback,
    input_state: InputStateCallback,
}

#[derive(Default)]
struct Core {
    callbacks: Callbacks,
    emulator: Option<MrpMachine>,
    /// Raw bytes of the loaded package, kept so `retro_reset` can reload it.
    content: Vec<u8>,
}

/// The libretro ABI may invoke callbacks from different threads, but the
/// frontend serializes them; the machine's `Rc` graph is single-threaded in
/// practice. This wrapper marks the guarded core `Send` — the one unsafe
/// promise at the libretro boundary.
struct SendCore(Core);
unsafe impl Send for SendCore {}
impl std::ops::Deref for SendCore {
    type Target = Core;
    fn deref(&self) -> &Core {
        &self.0
    }
}
impl std::ops::DerefMut for SendCore {
    fn deref_mut(&mut self) -> &mut Core {
        &mut self.0
    }
}

static CORE: Mutex<SendCore> = Mutex::new(SendCore(Core {
    callbacks: Callbacks {
        environment: None,
        video_refresh: None,
        audio_sample_batch: None,
        input_poll: None,
        input_state: None,
    },
    emulator: None,
    content: Vec::new(),
}));

fn core() -> MutexGuard<'static, SendCore> {
    CORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run `body`, turning a panic into `fallback`.
///
/// A panic must never unwind across the C ABI: the host would be torn down
/// with us. Swallowing it keeps the session recoverable and leaves a log
/// entry.
fn guard<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_owned());
            log::error!("MrpEmu libretro entry point panicked: {detail}");
            fallback
        }
    }
}

/// Frontend log sink installed through `RETRO_ENVIRONMENT_GET_LOG_INTERFACE`.
///
/// It lives outside `CORE` so logging while the core mutex is held cannot
/// deadlock. Native builds only: over wasm the callback cannot be called
/// safely (see `LogCallback`) and stderr carries the message instead.
#[cfg(not(target_os = "emscripten"))]
static FRONTEND_LOG: Mutex<LogCallback> = Mutex::new(None);

fn call_environment(callback: EnvironmentCallback, command: u32, data: *mut c_void) -> bool {
    callback.is_some_and(|callback| unsafe { callback(command, data) })
}

/// Joypad-id → guest key mapping, also used to build the input descriptors.
const JOYPAD_MAP: &[(u32, MrpKey, &CStr)] = &[
    (JOYPAD_UP, MrpKey::Up, c"D-Pad Up"),
    (JOYPAD_DOWN, MrpKey::Down, c"D-Pad Down"),
    (JOYPAD_LEFT, MrpKey::Left, c"D-Pad Left"),
    (JOYPAD_RIGHT, MrpKey::Right, c"D-Pad Right"),
    (JOYPAD_A, MrpKey::Ok, c"OK (confirm)"),
    (JOYPAD_B, MrpKey::LeftSoft, c"Left Soft Key (menu)"),
    (JOYPAD_X, MrpKey::RightSoft, c"Right Soft Key (back)"),
    (JOYPAD_Y, MrpKey::Num0, c"Digit 0"),
    (JOYPAD_SELECT, MrpKey::Star, c"Key *"),
    (JOYPAD_START, MrpKey::Pound, c"Key #"),
];

/// Builds the libretro input descriptor array with its NULL terminator.
fn input_descriptors() -> Vec<InputDescriptor> {
    let mut descriptors: Vec<InputDescriptor> = JOYPAD_MAP
        .iter()
        .map(|(id, _, description)| InputDescriptor {
            port: 0,
            device: DEVICE_JOYPAD,
            index: 0,
            id: *id,
            description: description.as_ptr(),
        })
        .collect();
    descriptors.push(InputDescriptor {
        port: 0,
        device: DEVICE_NONE,
        index: 0,
        id: 0,
        description: ptr::null(),
    });
    descriptors
}

struct FrontendLogger;

impl log::Log for FrontendLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            log::Level::Error => LOG_ERROR,
            log::Level::Warn => LOG_WARN,
            log::Level::Info => LOG_INFO,
            log::Level::Debug | log::Level::Trace => LOG_DEBUG,
        };

        // EmulatorJS forwards stderr to the browser console.
        #[cfg(target_os = "emscripten")]
        {
            let _ = level;
            eprintln!("{}", record.args());
        }

        // The message itself is the format string, so percent signs in content
        // paths have to be escaped before reaching the frontend's printf.
        #[cfg(not(target_os = "emscripten"))]
        {
            let Ok(message) = CString::new(record.args().to_string().replace('%', "%%")) else {
                return;
            };
            let callback = *FRONTEND_LOG
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(callback) = callback {
                unsafe { callback(level, message.as_ptr()) };
            }
        }
    }

    fn flush(&self) {}
}

static LOGGER: FrontendLogger = FrontendLogger;

#[no_mangle]
pub extern "C" fn retro_set_environment(callback: EnvironmentCallback) {
    guard((), || {
        core().callbacks.environment = callback;
        // Queried natively only; over wasm the interface is unusable, so
        // logging falls back to stderr.
        #[cfg(not(target_os = "emscripten"))]
        {
            let mut log = LogCallbackStruct { log: None };
            if call_environment(
                callback,
                ENVIRONMENT_GET_LOG_INTERFACE,
                ptr::from_mut(&mut log).cast(),
            ) {
                *FRONTEND_LOG
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = log.log;
            }
        }

        let mut support_no_game = false;
        call_environment(
            callback,
            ENVIRONMENT_SET_SUPPORT_NO_GAME,
            ptr::from_mut(&mut support_no_game).cast(),
        );
    });
}

#[no_mangle]
pub extern "C" fn retro_set_video_refresh(callback: VideoRefreshCallback) {
    guard((), || core().callbacks.video_refresh = callback);
}

#[no_mangle]
pub extern "C" fn retro_set_audio_sample(_callback: AudioSampleCallback) {
    // No audio yet; the batch callback is reserved for future use. This entry
    // point cannot panic, so it does not need the panic guard.
}

#[no_mangle]
pub extern "C" fn retro_set_audio_sample_batch(callback: AudioSampleBatchCallback) {
    guard((), || core().callbacks.audio_sample_batch = callback);
}

#[no_mangle]
pub extern "C" fn retro_set_input_poll(callback: InputPollCallback) {
    guard((), || core().callbacks.input_poll = callback);
}

#[no_mangle]
pub extern "C" fn retro_set_input_state(callback: InputStateCallback) {
    guard((), || core().callbacks.input_state = callback);
}

#[no_mangle]
pub extern "C" fn retro_init() {
    guard((), || {
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(log::LevelFilter::Debug);
    });
}

#[no_mangle]
pub extern "C" fn retro_deinit() {
    guard((), || {
        let mut core = core();
        core.emulator = None;
        core.content.clear();
    });
}

#[no_mangle]
pub extern "C" fn retro_api_version() -> u32 {
    guard(API_VERSION, || API_VERSION)
}

#[no_mangle]
/// # Safety
/// `info` must be null or point to writable `SystemInfo` storage.
pub unsafe extern "C" fn retro_get_system_info(info: *mut SystemInfo) {
    guard((), || {
        let Some(info) = (unsafe { info.as_mut() }) else {
            return;
        };
        *info = SystemInfo {
            library_name: c"MrpEmu".as_ptr(),
            library_version: concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast(),
            valid_extensions: c"mrp".as_ptr(),
            need_fullpath: true,
            block_extract: false,
        };
    });
}

#[no_mangle]
/// # Safety
/// `info` must be null or point to writable `SystemAvInfo` storage.
pub unsafe extern "C" fn retro_get_system_av_info(info: *mut SystemAvInfo) {
    guard((), || {
        let Some(info) = (unsafe { info.as_mut() }) else {
            return;
        };
        let core = core();
        let (width, height) = core
            .emulator
            .as_ref()
            .map(|emulator| (emulator.screen().width(), emulator.screen().height()))
            .unwrap_or((
                mrpemu_core::display::DEFAULT_WIDTH,
                mrpemu_core::display::DEFAULT_HEIGHT,
            ));
        *info = SystemAvInfo {
            geometry: GameGeometry {
                base_width: u32::from(width),
                base_height: u32::from(height),
                max_width: u32::from(width),
                max_height: u32::from(height),
                aspect_ratio: ASPECT_RATIO,
            },
            timing: SystemTiming {
                fps: FRAMES_PER_SECOND,
                sample_rate: AUDIO_SAMPLE_RATE,
            },
        };
    });
}

#[no_mangle]
pub extern "C" fn retro_set_controller_port_device(_port: u32, _device: u32) {
    // The core reads a fixed joypad layout; per-port device switching is a
    // no-op. This entry point cannot panic, so it does not need the guard.
}

#[no_mangle]
pub extern "C" fn retro_reset() {
    guard((), || {
        let mut core = core();
        if core.content.is_empty() {
            return;
        }
        match MrpMachine::load(core.content.clone()) {
            Ok(emulator) => core.emulator = Some(emulator),
            Err(error) => log::error!("reset failed: {error}"),
        }
    });
}

#[no_mangle]
pub extern "C" fn retro_run() {
    guard((), || {
        let mut core = core();
        let Core {
            callbacks,
            emulator,
            ..
        } = &mut core.0;
        let Some(emulator) = emulator.as_mut() else {
            return;
        };

        // Sync the frontend joypad into the guest key state.
        for (id, key, _) in JOYPAD_MAP {
            let pressed = callbacks
                .input_state
                .is_some_and(|callback| unsafe { callback(0, DEVICE_JOYPAD, 0, *id) != 0 });
            if pressed {
                emulator.input_mut().press(*key);
            } else {
                emulator.input_mut().release(*key);
            }
        }

        if let Some(poll) = callbacks.input_poll {
            unsafe { poll() };
        }

        emulator.run_frame();

        let screen = emulator.screen();
        if let Some(video_refresh) = callbacks.video_refresh {
            unsafe {
                video_refresh(
                    screen.pixels().as_ptr().cast(),
                    u32::from(screen.width()),
                    u32::from(screen.height()),
                    usize::from(screen.width()) * 2,
                );
            }
        }
    });
}

#[no_mangle]
/// # Safety
/// `info` must be null or point to readable `GameInfo` storage whose `path`
/// outlives the call.
pub unsafe extern "C" fn retro_load_game(info: *const GameInfo) -> bool {
    guard(false, || {
        let Some(info) = (unsafe { info.as_ref() }) else {
            return false;
        };
        if info.path.is_null() {
            log::error!("content must be provided as a full path");
            return false;
        }
        let path = PathBuf::from(
            unsafe { CStr::from_ptr(info.path) }
                .to_string_lossy()
                .as_ref(),
        );

        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(error) => {
                log::error!("failed to read {}: {error}", path.display());
                return false;
            }
        };
        let emulator = match MrpMachine::load(data) {
            Ok(emulator) => emulator,
            Err(error) => {
                log::error!("failed to load {}: {error}", path.display());
                return false;
            }
        };

        let mut core = core();
        let pixel_format = PIXEL_FORMAT_RGB565;
        call_environment(
            core.callbacks.environment,
            ENVIRONMENT_SET_PIXEL_FORMAT,
            ptr::from_ref(&pixel_format).cast_mut().cast(),
        );
        let descriptors = input_descriptors();
        call_environment(
            core.callbacks.environment,
            ENVIRONMENT_SET_INPUT_DESCRIPTORS,
            ptr::from_ref(&descriptors).cast_mut().cast(),
        );

        log::info!(
            "loaded {:?} (internal name {:?}, app id {}) from {}",
            emulator.header().display_name,
            emulator.header().internal_name,
            emulator.header().app_id,
            path.display()
        );
        core.content = emulator.package().to_vec();
        core.emulator = Some(emulator);
        true
    })
}

#[no_mangle]
pub extern "C" fn retro_unload_game() {
    guard((), || {
        let mut core = core();
        core.emulator = None;
        core.content.clear();
    });
}

#[no_mangle]
pub extern "C" fn retro_get_region() -> u32 {
    guard(REGION_NTSC, || REGION_NTSC)
}

#[no_mangle]
pub extern "C" fn retro_load_game_special(
    _game_type: u32,
    _info: *const GameInfo,
    _num_info: usize,
) -> bool {
    guard(false, || false)
}

#[no_mangle]
pub extern "C" fn retro_serialize_size() -> usize {
    // Save states arrive together with the VM; reporting zero keeps frontends
    // from offering broken state support.
    guard(0, || 0)
}

#[no_mangle]
/// # Safety
/// `data` must be null or point to writable storage of at least `size` bytes.
pub unsafe extern "C" fn retro_serialize(_data: *mut c_void, _size: usize) -> bool {
    guard(false, || false)
}

#[no_mangle]
/// # Safety
/// `data` must be null or point to readable storage of at least `size` bytes.
pub unsafe extern "C" fn retro_deserialize(_data: *const c_void, _size: usize) -> bool {
    guard(false, || false)
}

#[no_mangle]
pub extern "C" fn retro_cheat_reset() {}

#[no_mangle]
pub extern "C" fn retro_cheat_set(_index: u32, _enabled: bool, _code: *const c_char) {}

#[no_mangle]
/// # Safety
/// `id` must be a valid `RETRO_MEMORY_*` index.
pub unsafe extern "C" fn retro_get_memory_data(_id: u32) -> *mut c_void {
    guard(ptr::null_mut(), ptr::null_mut)
}

#[no_mangle]
pub extern "C" fn retro_get_memory_size(_id: u32) -> usize {
    guard(0, || 0)
}
