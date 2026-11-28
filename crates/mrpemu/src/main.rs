//! MrpEmu standalone desktop frontend.
//!
//! Loads a `.mrp` application package and displays the emulated phone
//! screen in a minifb window. A headless mode with scripted key presses
//! drives deterministic screenshot capture (`scripts/batch-screenshots.ps1`).

use anyhow::{bail, Context, Result};
use clap::Parser;
use minifb::{InputCallback, Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

mod audio;

use mrpemu_core::cfunction::{reset_instruction_counter, set_instruction_budget};
use mrpemu_core::emulator::FRAME_RATE;
use mrpemu_core::save_state;
use mrpemu_core::{MrpKey, MrpMachine};

/// Guest instruction budget shared by boot and all headless frames. Matches
/// the corpus gate so a stuck title is bounded instead of hanging a batch.
const HEADLESS_INSTRUCTION_BUDGET: u64 = 512_000_000;

/// MrpEmu — Sky-mobi mythroad (MRP) feature-phone application emulator.
#[derive(Debug, Parser)]
#[command(name = "mrpemu", version, about)]
struct Args {
    /// Path to the `.mrp` application package to run.
    mrp_path: PathBuf,

    /// Positive integer scale factor applied to the phone screen.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..))]
    scale: u32,

    /// Directory with optional Mythroad system files and plugins.
    #[arg(long)]
    system_dir: Option<PathBuf>,

    /// Run without opening a window.
    #[arg(long)]
    headless: bool,

    /// Number of frames to run in headless mode.
    #[arg(long, default_value_t = 60)]
    frames: u32,

    /// Total guest instruction budget for boot and headless frames; 0 is
    /// unlimited. Only applied to headless and screenshot runs.
    #[arg(long, default_value_t = HEADLESS_INSTRUCTION_BUDGET)]
    instruction_limit: u64,

    /// Save a PNG of the presented frame and exit (implies headless).
    #[arg(short = 'S', long, value_name = "PATH")]
    screenshot: Option<PathBuf>,

    /// Number of frames to run before taking a screenshot.
    #[arg(long, default_value_t = 200)]
    screenshot_frames: u32,

    /// Directory where guest-created files persist between runs. Loaded
    /// before the first frame and flushed when the run ends.
    #[arg(long)]
    persist_dir: Option<PathBuf>,

    /// Restore a machine snapshot before the first frame. The snapshot must
    /// have been saved for this package.
    #[arg(long)]
    load_state: Option<PathBuf>,

    /// Write a machine snapshot when the run ends (after the last frame or
    /// when the window closes).
    #[arg(long)]
    save_state: Option<PathBuf>,

    /// Scripted phone key press at an exact frame, `frame:key`. Repeatable.
    /// Keys: up, down, left, right, ok, lsk, rsk, 0-9, star, pound.
    /// Headless and screenshot runs only.
    #[arg(long)]
    press: Vec<String>,

    /// Enable debug logging.
    #[arg(short, long)]
    verbose: bool,
}

/// One scripted press: fire the key at `frame`, held for two guest frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Press {
    frame: u32,
    key: MrpKey,
}

/// Maps a documented press name to the guest key model.
fn parse_key(name: &str) -> Option<MrpKey> {
    let key = match name.trim().to_ascii_lowercase().as_str() {
        "up" => MrpKey::Up,
        "down" => MrpKey::Down,
        "left" => MrpKey::Left,
        "right" => MrpKey::Right,
        "ok" | "enter" | "confirm" => MrpKey::Ok,
        "lsk" | "leftsoft" | "softleft" => MrpKey::LeftSoft,
        "rsk" | "rightsoft" | "softright" => MrpKey::RightSoft,
        "star" | "*" => MrpKey::Star,
        "pound" | "#" | "hash" => MrpKey::Pound,
        "0" => MrpKey::Num0,
        "1" => MrpKey::Num1,
        "2" => MrpKey::Num2,
        "3" => MrpKey::Num3,
        "4" => MrpKey::Num4,
        "5" => MrpKey::Num5,
        "6" => MrpKey::Num6,
        "7" => MrpKey::Num7,
        "8" => MrpKey::Num8,
        "9" => MrpKey::Num9,
        _ => return None,
    };
    Some(key)
}

/// Parses and sorts `frame:key` specifications into a deterministic schedule.
fn parse_presses(specs: &[String]) -> Result<Vec<Press>> {
    let mut schedule = specs
        .iter()
        .map(|spec| {
            let (frame, key) = spec
                .split_once(':')
                .with_context(|| format!("--press wants frame:key, got {spec:?}"))?;
            let frame = frame
                .trim()
                .parse()
                .with_context(|| format!("bad frame in {spec:?}"))?;
            let key =
                parse_key(key).with_context(|| format!("unknown key in {spec:?}; see --help"))?;
            Ok(Press { frame, key })
        })
        .collect::<Result<Vec<_>>>()?;
    schedule.sort_by_key(|press| press.frame);
    Ok(schedule)
}

/// Host keys mapped to feature-phone keys.
const KEY_MAP: &[(Key, MrpKey)] = &[
    (Key::Up, MrpKey::Up),
    (Key::Down, MrpKey::Down),
    (Key::Left, MrpKey::Left),
    (Key::Right, MrpKey::Right),
    (Key::Enter, MrpKey::Ok),
    (Key::Z, MrpKey::LeftSoft),
    (Key::X, MrpKey::RightSoft),
    (Key::Key0, MrpKey::Num0),
    (Key::Key1, MrpKey::Num1),
    (Key::Key2, MrpKey::Num2),
    (Key::Key3, MrpKey::Num3),
    (Key::Key4, MrpKey::Num4),
    (Key::Key5, MrpKey::Num5),
    (Key::Key6, MrpKey::Num6),
    (Key::Key7, MrpKey::Num7),
    (Key::Key8, MrpKey::Num8),
    (Key::Key9, MrpKey::Num9),
];

#[derive(Clone, Default)]
struct WindowText(Rc<RefCell<String>>);

impl InputCallback for WindowText {
    fn add_char(&mut self, codepoint: u32) {
        if let Some(ch) = char::from_u32(codepoint) {
            self.0.borrow_mut().push(ch);
        }
    }
}

/// Expands one RGB565 pixel to 8-bit RGB for PNG output.
fn rgb565_to_rgb8(pixel: u16) -> [u8; 3] {
    let r = (pixel >> 11) & 0x1F;
    let g = (pixel >> 5) & 0x3F;
    let b = pixel & 0x1F;
    [
        ((r << 3) | (r >> 2)) as u8,
        ((g << 2) | (g >> 4)) as u8,
        ((b << 3) | (b >> 2)) as u8,
    ]
}

/// Expands an RGB565 pixel to the 0RGB u32 format minifb expects.
fn rgb565_to_0rgb(pixel: u16) -> u32 {
    let [r, g, b] = rgb565_to_rgb8(pixel);
    0xFF00_0000 | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

/// Loads optional device files from the explicit or content-relative dir.
fn load_device_files(machine: &mut MrpMachine, args: &Args) {
    if let Some(directory) = args
        .system_dir
        .as_deref()
        .or_else(|| args.mrp_path.parent())
    {
        for &name in mrpemu_core::vfs::DEVICE_FILES {
            if let Ok(data) = std::fs::read(directory.join(name)) {
                machine.vfs_mut().write(name, data);
            }
        }
    }
}

/// Runs a fixed-clock frame sequence with the scripted presses. Each press
/// consumes two frames (press, then release) so the guest observes both edges.
fn run_scripted_frames(
    machine: &mut MrpMachine,
    presses: &[Press],
    frames: u32,
    package: &Path,
) -> Result<u32> {
    let total = presses
        .last()
        .map_or(frames, |press| frames.max(press.frame.saturating_add(2)));
    let frame_duration = Duration::from_secs_f64(1.0 / FRAME_RATE);
    let mut elapsed = Duration::ZERO;
    let mut next = 0usize;
    let mut frame = 0u32;
    while frame < total {
        if next < presses.len() && presses[next].frame <= frame {
            let press = presses[next];
            next += 1;
            machine.input_mut().press(press.key);
            machine.run_frame_at(elapsed);
            elapsed += frame_duration;
            machine.input_mut().release(press.key);
            machine.run_frame_at(elapsed);
            elapsed += frame_duration;
            log::debug!("frame {frame} press {:?}", press.key);
            frame += 2;
        } else {
            machine.run_frame_at(elapsed);
            elapsed += frame_duration;
            frame += 1;
        }
        if let Some(error) = machine.boot_error() {
            bail!("failed to start {}: {error}", package.display());
        }
    }
    Ok(frame)
}

/// Captures the presented frame as a PNG with the frame statistics the batch
/// script parses (`frame nonzero=N colors=N`).
fn capture_screenshot(
    machine: &mut MrpMachine,
    presses: &[Press],
    frames: u32,
    path: &Path,
    package: &Path,
) -> Result<()> {
    run_scripted_frames(machine, presses, frames, package)?;

    let (width, height) = (
        u32::from(machine.screen().width()),
        u32::from(machine.screen().height()),
    );
    let pixels = machine.screen().pixels();
    let colors: HashSet<u16> = pixels.iter().copied().collect();
    if colors.len() <= 1 {
        bail!("guest did not render a screenshot within {frames} frames");
    }
    let nonzero = pixels.iter().filter(|&&pixel| pixel != 0).count();
    log::info!("frame nonzero={nonzero} colors={}", colors.len());

    let rgb: Vec<u8> = pixels
        .iter()
        .flat_map(|&pixel| rgb565_to_rgb8(pixel))
        .collect();
    image::save_buffer(path, &rgb, width, height, image::ColorType::Rgb8)
        .with_context(|| format!("failed to save screenshot: {}", path.display()))?;
    log::info!("Screenshot saved to: {}", path.display());
    Ok(())
}

/// Runs the end-of-run work shared by every mode: snapshot writing and
/// persistence flush.
fn finish_run(machine: &mut MrpMachine, args: &Args) -> Result<()> {
    if let Some(path) = &args.save_state {
        let mut buffer = vec![0u8; save_state::SERIALIZED_SIZE];
        save_state::encode_machine(machine, &mut buffer)
            .map_err(|error| anyhow::anyhow!("failed to encode save state: {error}"))?;
        let len = save_state::snapshot_len(&buffer)
            .map_err(|error| anyhow::anyhow!("bad snapshot: {error}"))?;
        std::fs::write(path, &buffer[..len])
            .with_context(|| format!("failed to write {}", path.display()))?;
        log::info!("save state written to {} ({} bytes)", path.display(), len);
    }
    if args.persist_dir.is_some() {
        match machine.flush_persistence() {
            Ok(count) if count > 0 => log::info!("persistence: flushed {count} files"),
            Ok(_) => {}
            Err(error) => log::warn!("persistence: flush failed: {error}"),
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let level = if args.verbose {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };
    env_logger::Builder::new().filter_level(level).init();

    let presses = parse_presses(&args.press)?;
    let headless = args.headless || args.screenshot.is_some();
    if headless {
        set_instruction_budget(args.instruction_limit);
        reset_instruction_counter();
    }

    let mut machine = MrpMachine::from_path(&args.mrp_path)
        .with_context(|| format!("failed to load {}", args.mrp_path.display()))?;
    load_device_files(&mut machine, &args);
    if let Some(state_path) = &args.load_state {
        let data = std::fs::read(state_path)
            .with_context(|| format!("failed to read {}", state_path.display()))?;
        let restored = save_state::decode_machine(&data, save_state::content_crc32(&machine))
            .map_err(|error| {
                anyhow::anyhow!("failed to restore {}: {error}", state_path.display())
            })?;
        machine = restored;
        log::info!("restored save state from {}", state_path.display());
    }
    if let Some(dir) = &args.persist_dir {
        machine.set_persist_dir(dir);
        log::info!("persistence directory: {}", dir.display());
    }
    log::info!(
        "loaded {:?} (internal name {:?}, app id {})",
        machine.header().display_name,
        machine.header().internal_name,
        machine.header().app_id
    );

    if let Some(path) = &args.screenshot {
        capture_screenshot(
            &mut machine,
            &presses,
            args.screenshot_frames,
            path,
            &args.mrp_path,
        )?;
        return finish_run(&mut machine, &args);
    }

    if args.headless {
        let frames = run_scripted_frames(&mut machine, &presses, args.frames, &args.mrp_path)?;
        log::info!("Headless run completed: {frames} frames");
        return finish_run(&mut machine, &args);
    }

    // Audio output: open the default device and hand the sink to the core.
    let _audio_stream = match audio::open() {
        Ok((player, stream)) => {
            machine.set_audio_sink(Box::new(player));
            Some(stream)
        }
        Err(error) => {
            log::warn!("audio disabled: {error:#}");
            None
        }
    };

    let (width, height) = (
        usize::from(machine.screen().width()),
        usize::from(machine.screen().height()),
    );
    let scale = args.scale as usize;
    let mut window = Window::new(
        &format!("MrpEmu — {}", machine.header().display_name),
        width * scale,
        height * scale,
        WindowOptions::default(),
    )
    .context("failed to open window")?;
    window.set_target_fps(FRAME_RATE as usize);
    let window_text = WindowText::default();
    window.set_input_callback(Box::new(window_text.clone()));
    let mut buffer = vec![0; width * height];
    let frame_duration = Duration::from_secs_f64(1.0 / FRAME_RATE);
    let mut elapsed = Duration::ZERO;
    let mut touch_position = (0, 0);

    while window.is_open() {
        let editing = machine.is_editing();
        let escape = window.is_key_down(Key::Escape);
        if escape && !editing {
            break;
        }
        let text = std::mem::take(&mut *window_text.0.borrow_mut());
        if editing {
            machine.type_edit_text(&text);
            if window.is_key_pressed(Key::Backspace, KeyRepeat::Yes) {
                machine.backspace_edit_text();
            }
        }
        let held = window.get_keys();
        for (host, guest) in KEY_MAP {
            let pressed = if editing && *guest == MrpKey::RightSoft {
                escape
            } else {
                (!editing || *guest == MrpKey::Ok) && held.contains(host)
            };
            if pressed {
                machine.input_mut().press(*guest);
            } else {
                machine.input_mut().release(*guest);
            }
        }
        if let Some((x, y)) = window.get_unscaled_mouse_pos(MouseMode::Clamp) {
            touch_position = (
                (x as usize / scale).min(width - 1) as u16,
                (y as usize / scale).min(height - 1) as u16,
            );
        }
        machine.input_mut().set_touch(
            touch_position.0,
            touch_position.1,
            window.is_active() && window.get_mouse_down(MouseButton::Left),
        );

        machine.run_frame_at(elapsed);
        elapsed += frame_duration;
        if let Some(error) = machine.boot_error() {
            anyhow::bail!("failed to start {}: {error}", args.mrp_path.display());
        }

        for (output, &pixel) in buffer.iter_mut().zip(machine.screen().pixels()) {
            *output = rgb565_to_0rgb(pixel);
        }
        window
            .update_with_buffer(&buffer, width, height)
            .context("failed to present frame")?;
    }

    finish_run(&mut machine, &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_game_path_and_defaults() {
        let args = Args::try_parse_from(["mrpemu", "game.mrp"]).unwrap();

        assert_eq!(args.mrp_path, PathBuf::from("game.mrp"));
        assert_eq!(args.scale, 2);
        assert_eq!(args.frames, 60);
        assert_eq!(args.screenshot_frames, 200);
        assert_eq!(args.instruction_limit, HEADLESS_INSTRUCTION_BUDGET);
        assert!(!args.headless);
        assert!(!args.verbose);
        assert_eq!(args.persist_dir, None);
        assert_eq!(args.load_state, None);
        assert_eq!(args.save_state, None);
        assert!(Args::try_parse_from(["mrpemu"]).is_err());
    }

    #[test]
    fn parses_headless_screenshot_and_press_schedule() {
        let args = Args::try_parse_from([
            "mrpemu",
            "game.mrp",
            "--headless",
            "-S",
            "shot.png",
            "--screenshot-frames",
            "200",
            "--press",
            "30:lsk",
            "--press",
            "65:ok",
            "--instruction-limit",
            "0",
            "-v",
        ])
        .unwrap();

        assert!(args.headless);
        assert_eq!(args.screenshot, Some(PathBuf::from("shot.png")));
        assert_eq!(args.screenshot_frames, 200);
        assert_eq!(args.instruction_limit, 0);
        assert!(args.verbose);
        let presses = parse_presses(&args.press).unwrap();
        assert_eq!(
            presses,
            [
                Press {
                    frame: 30,
                    key: MrpKey::LeftSoft
                },
                Press {
                    frame: 65,
                    key: MrpKey::Ok
                },
            ]
        );
    }

    #[test]
    fn press_specs_are_sorted_and_validated() {
        let args =
            Args::try_parse_from(["mrpemu", "game.mrp", "--press", "90:9", "--press", "3:up"])
                .unwrap();
        assert_eq!(
            parse_presses(&args.press).unwrap(),
            [
                Press {
                    frame: 3,
                    key: MrpKey::Up
                },
                Press {
                    frame: 90,
                    key: MrpKey::Num9
                },
            ]
        );

        assert!(parse_presses(&["30".to_string()]).is_err());
        assert!(parse_presses(&["bad:ok".to_string()]).is_err());
        assert!(parse_presses(&["30:unknown".to_string()]).is_err());
    }

    #[test]
    fn rgb565_expands_to_primary_colors() {
        assert_eq!(rgb565_to_rgb8(0xF800), [255, 0, 0]);
        assert_eq!(rgb565_to_rgb8(0x07E0), [0, 255, 0]);
        assert_eq!(rgb565_to_rgb8(0x001F), [0, 0, 255]);
        assert_eq!(rgb565_to_0rgb(0xF800), 0xFFFF_0000);
    }
}
