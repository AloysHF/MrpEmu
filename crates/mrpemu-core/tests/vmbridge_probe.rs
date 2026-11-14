//! Temporary probe: drives a package through the real boot path with the
//! Lua VM bridge active. Not a regression gate.
use mrpemu_core::{MrpKey, MrpMachine};

#[test]
#[ignore = "manual probe"]
fn probe_vmbridge() {
    let _ = env_logger::try_init();
    let path = std::env::var("PROBE_PKG").unwrap();
    let mut machine = MrpMachine::from_path(&path).expect("boot");
    let frames: usize = std::env::var("PROBE_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let mut nonblank_best = 0usize;
    for frame in 0..frames {
        if frame % 20 == 5 {
            machine.input_mut().press(MrpKey::Ok);
        } else if frame % 20 == 8 {
            machine.input_mut().release(MrpKey::Ok);
        }
        machine.run_frame();
        let nonblank = machine
            .screen()
            .pixels()
            .iter()
            .filter(|&&p| p != 0)
            .count();
        nonblank_best = nonblank_best.max(nonblank);
        if frame % 10 == 0 {
            eprintln!("[frame {frame}] nonblank={nonblank} best={nonblank_best}");
        }
    }
    eprintln!("[final] nonblank_best={nonblank_best}");
    // Coarse ASCII map: 60x40 downsample of the RGB565 screen.
    let (w, h) = (240usize, 320usize);
    let px = machine.screen().pixels();
    for row in 0..40 {
        let mut line = String::new();
        for col in 0..60 {
            let mut sum = 0u32;
            for dy in 0..8 {
                for dx in 0..4 {
                    let p = px[(row * 8 + dy) * w + col * 4 + dx] as u32;
                    let r = (p >> 11) & 0x1f;
                    let g = (p >> 5) & 0x3f;
                    let b = p & 0x1f;
                    sum += r + g / 2 + b;
                }
            }
            line.push(if sum > 32 * 32 {
                '#'
            } else if sum > 8 * 32 {
                '+'
            } else if sum > 0 {
                '.'
            } else {
                ' '
            });
        }
        eprintln!("{line}");
    }
}
