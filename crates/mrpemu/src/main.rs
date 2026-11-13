//! MrpEmu standalone desktop frontend.
//!
//! Loads a `.mrp` application package and displays the emulated phone
//! screen in a minifb window. The mythroad VM is not implemented yet, so the
//! window shows the placeholder frame produced by the core.

use anyhow::{Context, Result};
use clap::Parser;
use minifb::{Key, Window, WindowOptions};

mod audio;

use mrpemu_core::{MrpKey, MrpMachine};

/// MrpEmu — Sky-mobi mythroad (MRP) feature-phone application emulator.
#[derive(Debug, Parser)]
#[command(name = "mrpemu", version, about)]
struct Args {
    /// Path to the `.mrp` application package to run.
    mrp_path: std::path::PathBuf,

    /// Integer scale factor applied to the 240x320 screen.
    #[arg(long, default_value_t = 2)]
    scale: u32,
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
    window.set_target_fps(60);

    while window.is_open() && !window.is_key_down(Key::Escape) {
        let held = window.get_keys();
        for (host, guest) in KEY_MAP {
            if held.contains(host) {
                machine.input_mut().press(*guest);
            } else {
                machine.input_mut().release(*guest);
            }
        }

        machine.run_frame();

        let buffer: Vec<u32> = machine
            .screen()
            .pixels()
            .iter()
            .map(|&pixel| rgb565_to_0rgb(pixel))
            .collect();
        window
            .update_with_buffer(&buffer, width, height)
            .context("failed to present frame")?;
    }

    Ok(())
}
