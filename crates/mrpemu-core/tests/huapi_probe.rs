//! Temporary probe: boots one ext directly and inspects the framebuffer
//! after the budget runs out. Not a regression gate.
use mrpemu_core::cfunction::{set_instruction_budget, Guest, GuestConfig};
use mrpemu_core::display::Screen;
use mrpemu_core::package::MrpPackage;

#[test]
#[ignore = "manual probe"]
fn probe_game_ext() {
    let _ = env_logger::try_init();
    let budget: u64 = std::env::var("PROBE_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40_000_000);
    set_instruction_budget(budget);
    let path = std::env::var("PROBE_PKG").unwrap();
    let data = std::fs::read(&path).expect("read package");
    let package = MrpPackage::parse(data).expect("parse");
    let name = std::env::var("PROBE_EXT").unwrap_or_else(|_| "game.ext".to_string());
    let ext = package.read_entry(&name).expect("ext member");
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
    let result = guest.boot_ext(&ext, 0, 0, &mut screen);
    eprintln!(
        "boot result: {:?}",
        result.as_ref().map(|_| "ok").map_err(|e| e.to_string())
    );
    let nonblank = screen.pixels().iter().filter(|&&p| p != 0).count();
    eprintln!("screen nonblank={nonblank}/{}", screen.pixels().len());
    let sb = guest.r32(mrpemu_core::cfunction::SCREENBUF);
    eprintln!("screenbuf first word: {sb:#x}");
}
