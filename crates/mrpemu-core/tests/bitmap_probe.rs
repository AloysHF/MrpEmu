//! Temporary probe: boots an ext and inspects the per-frame drawBitmap
//! arguments and buffer content. Not a regression gate.
use mrpemu_core::cfunction::{set_instruction_budget, Guest, GuestConfig};
use mrpemu_core::display::Screen;
use mrpemu_core::package::MrpPackage;
use mrpemu_core::MrpKey;

#[test]
#[ignore = "manual probe"]
fn probe_bitmap() {
    let _ = env_logger::try_init();
    set_instruction_budget(0); // unlimited during boot
    let path = std::env::var("PROBE_PKG").unwrap();
    let data = std::fs::read(&path).expect("read package");
    let package = MrpPackage::parse(data).expect("parse");
    let ext = package.read_entry("game.ext").expect("ext member");
    let mut files: Vec<(String, Vec<u8>)> = package
        .entries()
        .iter()
        .map(|e| {
            (
                e.name.clone(),
                package.read_entry(&e.name).unwrap_or_default(),
            )
        })
        .collect();
    files.push(("app.mrp".to_string(), package.raw_data().to_vec()));
    let mut guest = Guest::new(GuestConfig::default(), files);
    let mut screen = Screen::new(240, 320);
    guest.boot_ext(&ext, 0, 0, &mut screen).expect("boot");
    // 30 frames of key/timer pump.
    for frame in 0..30 {
        if frame == 10 {
            guest.send_event(0, 17, 0, &mut screen).ok();
        }
        guest.fire_timer(&mut screen).ok();
    }
    // Draw calls happened; inspect the bitmap bookkeeping.
    eprintln!("bitmaps tracked: {}", guest.bitmaps.len());
    for (id, bmp) in &guest.bitmaps {
        let nonzero = bmp.pixels.iter().filter(|&&p| p != 0).count();
        eprintln!(
            "  bitmap {id}: {}x{} nonzero={}",
            bmp.width, bmp.height, nonzero
        );
    }
    let nonblank = screen.pixels().iter().filter(|&&p| p != 0).count();
    eprintln!("screen nonblank={nonblank}");
    // Dump the screenbuf content the guest wrote (first 8 rows, center).
    let sb = mrpemu_core::cfunction::SCREENBUF;
    for row in [0, 155, 156, 157, 158, 159, 160, 161] {
        let mut line = format!("row {row}: ");
        for col in (100..140).step_by(4) {
            line += &format!("{:04x} ", guest.r16(sb + (row * 240 + col) as u32 * 2));
        }
        eprintln!("{line}");
    }
}
