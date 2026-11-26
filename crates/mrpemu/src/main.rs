//! MrpEmu standalone desktop frontend.
//!
//! Loads a `.mrp` application package and displays the emulated phone
//! screen in a minifb window.

use anyhow::{Context, Result};
use clap::Parser;
use minifb::{Key, Window, WindowOptions};
use std::time::Duration;

mod audio;

use mrpemu_core::emulator::FRAME_RATE;
use mrpemu_core::{MrpKey, MrpMachine};

/// MrpEmu — Sky-mobi mythroad (MRP) feature-phone application emulator.
#[derive(Debug, Parser)]
#[command(name = "mrpemu", version, about)]
struct Args {
    /// Path to the `.mrp` application package to run.
    mrp_path: std::path::PathBuf,

    /// Integer scale factor applied to the phone screen.
    #[arg(long, default_value_t = 2)]
    scale: u32,

    /// Directory with optional Mythroad system files and plugins.
    #[arg(long)]
    system_dir: Option<std::path::PathBuf>,
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

/// Expands an RGB565 pixel to the 0RGB u32 format minifb expects.
fn rgb565_to_0rgb(pixel: u16) -> u32 {
    let r = u32::from((pixel >> 11) & 0x1F);
    let g = u32::from((pixel >> 5) & 0x3F);
    let b = u32::from(pixel & 0x1F);
    let r = (r << 3) | (r >> 2);
    let g = (g << 2) | (g >> 4);
    let b = (b << 3) | (b >> 2);
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    let mut machine = MrpMachine::from_path(&args.mrp_path)
        .with_context(|| format!("failed to load {}", args.mrp_path.display()))?;
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
    log::info!(
        "loaded {:?} (internal name {:?}, app id {})",
        machine.header().display_name,
        machine.header().internal_name,
        machine.header().app_id
    );

    // Audio output: open the default device and hand the sink to the core.
    match audio::CpalSink::open() {
        Ok(sink) => machine.set_audio_sink(Box::new(sink)),
        Err(error) => log::warn!("audio disabled: {error:#}"),
    }

    let (width, height) = (
        usize::from(machine.screen().width()),
        usize::from(machine.screen().height()),
    );
    let mut window = Window::new(
        &format!("MrpEmu — {}", machine.header().display_name),
        width * args.scale as usize,
        height * args.scale as usize,
        WindowOptions::default(),
    )
    .context("failed to open window")?;
    window.set_target_fps(FRAME_RATE as usize);
    let mut buffer = vec![0; width * height];
    let frame_duration = Duration::from_secs_f64(1.0 / FRAME_RATE);
    let mut elapsed = Duration::ZERO;

    while window.is_open() && !window.is_key_down(Key::Escape) {
        let held = window.get_keys();
        for (host, guest) in KEY_MAP {
            if held.contains(host) {
                machine.input_mut().press(*guest);
            } else {
                machine.input_mut().release(*guest);
            }
        }

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

    Ok(())
}
