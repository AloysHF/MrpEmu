//! Exercises the libretro lifecycle and inspects the submitted RGB565 pixels.

use std::ffi::CString;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    Mutex, OnceLock,
};

use mrpemu::ffi::{
    GameGeometry, GameInfo, SystemAvInfo, SystemTiming, ENVIRONMENT_GET_SAVE_DIRECTORY,
    ENVIRONMENT_GET_SYSTEM_DIRECTORY, ENVIRONMENT_SET_PIXEL_FORMAT, PIXEL_FORMAT_RGB565,
};

static VIDEO_WIDTH: AtomicU32 = AtomicU32::new(0);
static VIDEO_HEIGHT: AtomicU32 = AtomicU32::new(0);
static VIDEO_PITCH: AtomicUsize = AtomicUsize::new(0);
static VIDEO_VISIBLE: AtomicBool = AtomicBool::new(false);
static INPUT_FRAME: AtomicU32 = AtomicU32::new(0);
static SYSTEM_DIR: OnceLock<CString> = OnceLock::new();
static SAVE_DIR: Mutex<Option<CString>> = Mutex::new(None);

unsafe extern "C" fn environment(cmd: u32, data: *mut core::ffi::c_void) -> bool {
    if cmd == ENVIRONMENT_GET_SYSTEM_DIRECTORY && !data.is_null() {
        if let Some(directory) = SYSTEM_DIR.get() {
            unsafe { *data.cast() = directory.as_ptr() };
            return true;
        }
    }
    if cmd == ENVIRONMENT_GET_SAVE_DIRECTORY && !data.is_null() {
        if let Some(directory) = SAVE_DIR.lock().unwrap().as_ref() {
            unsafe { *data.cast() = directory.as_ptr() };
            return true;
        }
    }
    if cmd == ENVIRONMENT_SET_PIXEL_FORMAT && !data.is_null() {
        return unsafe { *data.cast::<u32>() } == PIXEL_FORMAT_RGB565;
    }
    false
}

unsafe extern "C" fn video_refresh(
    data: *const core::ffi::c_void,
    width: u32,
    height: u32,
    pitch: usize,
) {
    VIDEO_WIDTH.store(width, Ordering::Relaxed);
    VIDEO_HEIGHT.store(height, Ordering::Relaxed);
    VIDEO_PITCH.store(pitch, Ordering::Relaxed);
    if !VIDEO_VISIBLE.load(Ordering::Relaxed)
        && !data.is_null()
        && width != 0
        && height != 0
        && pitch == width as usize * 2
    {
        let pixels = unsafe {
            std::slice::from_raw_parts(data.cast::<u16>(), width as usize * height as usize)
        };
        let active_pixels = pixels.iter().filter(|&&pixel| pixel != 0).count();
        let active_rows = pixels
            .chunks_exact(width as usize)
            .filter(|row| row.iter().any(|&pixel| pixel != 0))
            .count();
        if pixels.iter().any(|&pixel| pixel != pixels[0])
            && active_pixels * 200 >= pixels.len()
            && active_rows * 8 >= height as usize
        {
            VIDEO_VISIBLE.store(true, Ordering::Relaxed);
        }
    }
}

unsafe extern "C" fn audio_sample_batch(_data: *const i16, _frames: usize) -> usize {
    0
}

unsafe extern "C" fn input_poll() {}

unsafe extern "C" fn input_state(_port: u32, _device: u32, _index: u32, id: u32) -> i16 {
    let frame = INPUT_FRAME.load(Ordering::Relaxed);
    i16::from(
        [(30, 0), (50, 8), (85, 0), (105, 8), (140, 0), (160, 8)]
            .iter()
            .any(|&(start, button)| id == button && (start..start + 4).contains(&frame)),
    )
}

/// The full FFI sequence a libretro frontend performs, including reset.
#[test]
#[ignore = "requires the local reference corpus under tmp/mrp_game"]
fn libretro_smoke() {
    // Worktrees keep the gitignored corpus out; point MRPEMU_CORPUS_ROOT at
    // the main checkout when running there.
    let root = std::env::var_os("MRPEMU_CORPUS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../../tmp/mrp_game/热门游戏"));
    if let Ok(directory) = std::env::var("CORPUS_SYSTEM_DIR") {
        SYSTEM_DIR.set(CString::new(directory).unwrap()).unwrap();
    }
    let paths = if std::env::var_os("CORPUS_HOT").is_some() {
        let mut paths = Vec::new();
        let mut directories = vec![root.clone()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).expect("hot corpus directory") {
                let entry = entry.expect("corpus entry");
                let path = entry.path();
                if entry.file_type().expect("corpus entry type").is_dir() {
                    directories.push(path);
                } else if path.extension().is_some_and(|ext| ext == "mrp") {
                    paths.push(path);
                }
            }
        }
        paths.sort();
        paths
    } else {
        [
            "007烈焰行动.mrp",
            "指环王之霍比特人正版（240x400）.mrp",
            "诛仙2~倾城媚狐正版（240x400）.mrp",
        ]
        .map(|name| root.join(name))
        .to_vec()
    };
    assert!(!paths.is_empty());
    unsafe {
        mrpemu::retro_set_environment(Some(environment));
        mrpemu::retro_set_video_refresh(Some(video_refresh));
        mrpemu::retro_set_audio_sample_batch(Some(audio_sample_batch));
        mrpemu::retro_set_input_poll(Some(input_poll));
        mrpemu::retro_set_input_state(Some(input_state));
        mrpemu::retro_init();
        mrpemu::retro_set_controller_port_device(0, 1);
        // One fixed-capacity state buffer, sized by the core itself.
        let state_size = mrpemu::retro_serialize_size();
        assert!(state_size > 0, "the core offers save states");
        let mut state_buffer = vec![0u8; state_size];

        let mut failures = Vec::new();
        let save_root =
            std::env::temp_dir().join(format!("mrpemu-libretro-{}", std::process::id()));
        std::fs::create_dir_all(&save_root).expect("create save root");
        for path in &paths {
            let name = path.file_name().unwrap().to_string_lossy();
            let height = if name.contains("240x400") { 400 } else { 320 };
            // One persistence directory per content, like a frontend's
            // per-game saves layout.
            let content_save_dir = save_root.join(name.as_ref());
            std::fs::create_dir_all(&content_save_dir).expect("create content save dir");
            *SAVE_DIR.lock().unwrap() =
                Some(CString::new(content_save_dir.to_string_lossy().as_ref()).unwrap());
            let c_path = CString::new(path.to_string_lossy().as_ref()).expect("cstring");
            let info = GameInfo {
                path: c_path.as_ptr(),
                data: core::ptr::null(),
                size: 0,
                meta: core::ptr::null(),
            };
            assert!(mrpemu::retro_load_game(&info), "must load {name}");
            for reset in [false, true] {
                if reset {
                    mrpemu::retro_reset();
                }
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
                assert_eq!(av.geometry.base_width, 240);
                assert_eq!(av.geometry.base_height, height);
                assert_eq!(av.geometry.aspect_ratio, 240.0 / height as f32);
                assert!(av.timing.fps > 0.0);
            }
            VIDEO_VISIBLE.store(false, Ordering::Relaxed);
            for frame in 0..200 {
                INPUT_FRAME.store(frame, Ordering::Relaxed);
                mrpemu::retro_run();
                if VIDEO_VISIBLE.load(Ordering::Relaxed) {
                    break;
                }
            }
            assert_eq!(VIDEO_WIDTH.load(Ordering::Relaxed), 240);
            assert_eq!(VIDEO_HEIGHT.load(Ordering::Relaxed), height);
            assert_eq!(VIDEO_PITCH.load(Ordering::Relaxed), 480);
            if VIDEO_VISIBLE.load(Ordering::Relaxed) {
                eprintln!("content [240x{height}] {name}");
            } else {
                failures.push(name.clone());
            }

            // Save state: serialize the booted machine, advance a few frames,
            // restore, and confirm the core keeps running on the restored
            // state.
            assert!(mrpemu::retro_serialize_size() == state_buffer.len());
            INPUT_FRAME.store(0, Ordering::Relaxed);
            assert!(
                mrpemu::retro_serialize(state_buffer.as_mut_ptr().cast(), state_buffer.len()),
                "serialize must succeed for {name}"
            );
            for frame in 0..10u32 {
                INPUT_FRAME.store(frame, Ordering::Relaxed);
                mrpemu::retro_run();
            }
            assert!(
                mrpemu::retro_unserialize(state_buffer.as_ptr().cast(), state_buffer.len()),
                "unserialize must succeed for {name}"
            );
            for frame in 0..10u32 {
                INPUT_FRAME.store(frame, Ordering::Relaxed);
                mrpemu::retro_run();
            }

            mrpemu::retro_unload_game();
            // The unload flush creates the persistence directory even when
            // the title wrote no files yet.
            assert!(
                content_save_dir.is_dir(),
                "persistence directory survives unload for {name}"
            );
        }
        mrpemu::retro_deinit();
        assert!(failures.is_empty(), "packages without video: {failures:?}");
        eprintln!("video passed for {} packages", paths.len());
    }
}
