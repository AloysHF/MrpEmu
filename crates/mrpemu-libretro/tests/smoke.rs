//! Libretro FFI smoke test: drives the full frontend callback set against
//! a real package and verifies that `retro_load_game` succeeds, frames run,
//! and `retro_get_system_av_info` reports the emulated screen. Ignored by
//! default because it needs the local corpus under tmp/mrp_game.

use std::ffi::CString;
use std::path::PathBuf;

use mrpemu::ffi::{GameGeometry, GameInfo, SystemAvInfo, SystemTiming};

/// A C-compatible environment callback: answers the pixel-format query so
/// the core can configure itself, ignores the rest.
unsafe extern "C" fn environment(cmd: u32, data: *mut core::ffi::c_void) -> bool {
    const RETRO_ENVIRONMENT_SET_PIXEL_FORMAT: u32 = 10;
    if cmd == RETRO_ENVIRONMENT_SET_PIXEL_FORMAT && !data.is_null() {
        // RETRO_PIXEL_FORMAT_0RGB1555 = 0.
        *(data as *mut u32) = 0;
        return true;
    }
    false
}

unsafe extern "C" fn video_refresh(
    _data: *const core::ffi::c_void,
    _width: u32,
    _height: u32,
    _pitch: usize,
) {
}

unsafe extern "C" fn audio_sample_batch(_data: *const i16, _frames: usize) -> usize {
    0
}

unsafe extern "C" fn input_poll() {}

/// Returns a nonzero state for the D-pad/OK so the guest receives input.
unsafe extern "C" fn input_state(_port: u32, _device: u32, _index: u32, id: u32) -> i16 {
    // RETRO_DEVICE_ID_JOYPAD_A = 8 (maps to the MRP Ok key).
    if id == 8 {
        1
    } else {
        0
    }
}

fn corpus_path(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from("../../tmp/mrp_game/热门游戏").join(name);
    path.is_file().then_some(path)
}

/// The full FFI sequence a libretro frontend performs, then a few frames.
#[test]
#[ignore = "requires the local reference corpus under tmp/mrp_game"]
fn libretro_smoke() {
    // No package for the early calls: the core must tolerate them.
    unsafe {
        mrpemu::retro_set_environment(Some(environment));
        mrpemu::retro_set_video_refresh(Some(video_refresh));
        mrpemu::retro_set_audio_sample_batch(Some(audio_sample_batch));
        mrpemu::retro_set_input_poll(Some(input_poll));
        mrpemu::retro_set_input_state(Some(input_state));
        mrpemu::retro_init();
        mrpemu::retro_set_controller_port_device(0, 1);

        let Some(path) = corpus_path("007烈焰行动.mrp") else {
            eprintln!("corpus missing; skipping");
            mrpemu::retro_deinit();
            return;
        };
        let c_path = CString::new(path.to_string_lossy().as_ref()).expect("cstring");
        let info = GameInfo {
            path: c_path.as_ptr(),
            data: core::ptr::null(),
            size: 0,
            meta: core::ptr::null(),
        };
        let loaded = mrpemu::retro_load_game(&info);
        assert!(loaded, "retro_load_game must succeed on a real package");

        let mut av = SystemAvInfo {
            geometry: GameGeometry {
                base_width: 0,
                base_height: 0,
                max_width: 0,
                max_height: 0,
                aspect_ratio: 0.0,
            },
            timing: SystemTiming {
                fps: 0.0,
                sample_rate: 0.0,
            },
        };
        mrpemu::retro_get_system_av_info(&mut av);
        assert_eq!(av.geometry.base_width, 240, "emulated width");
        assert_eq!(av.geometry.base_height, 320, "emulated height");
        assert!(av.timing.fps > 0.0, "timing fps");

        // Run frames; the emulator must not panic or abort.
        for _ in 0..30 {
            mrpemu::retro_run();
        }
        mrpemu::retro_unload_game();
        mrpemu::retro_deinit();
    }
}
