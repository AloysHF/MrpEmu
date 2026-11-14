//! Temporary probe: boots game.ext and prints the tail of the boot's slot
//! activity plus the installed handler state. Not a regression gate.
use mrpemu_core::cfunction::{set_instruction_budget, Guest, GuestConfig};
use mrpemu_core::display::Screen;
use mrpemu_core::package::MrpPackage;

#[test]
#[ignore = "manual probe"]
fn probe_boot_tail() {
    let _ = env_logger::try_init();
    set_instruction_budget(0);
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
    eprintln!(
        "boot done: c_function={:#x} c_function_p={:#x} event_function={:#x} timer_function={:#x}",
        guest.c_function, guest.c_function_p, guest.event_function, guest.timer_function
    );
    let total: u64 = guest.slot_calls.iter().sum();
    eprintln!("total slot calls during boot: {total}");
    for (slot, count) in guest.slot_calls.iter().enumerate() {
        if *count > 0 {
            eprintln!("  slot {slot}: {count}");
        }
    }
}
