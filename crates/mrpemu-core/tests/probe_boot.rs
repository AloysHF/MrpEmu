//! Temporary probe: boots the largest ext directly. Not a regression gate.
use mrpemu_core::cfunction::{set_instruction_budget, Guest, GuestConfig};
use mrpemu_core::display::Screen;
use mrpemu_core::package::MrpPackage;

#[test]
#[ignore = "manual probe"]
fn probe_direct_boot() {
    let _ = env_logger::try_init();
    set_instruction_budget(20_000_000);
    let path = std::env::var("PROBE_PKG").unwrap();
    let data = std::fs::read(&path).expect("read package");
    let package = MrpPackage::parse(data).expect("parse");
    let mut exts: Vec<(String, Vec<u8>)> = package
        .entries()
        .iter()
        .filter(|e| e.name.to_lowercase().ends_with(".ext"))
        .map(|e| {
            (
                e.name.clone(),
                package.read_entry(&e.name).unwrap_or_default(),
            )
        })
        .collect();
    exts.sort_by_key(|(_, d)| std::cmp::Reverse(d.len()));
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
    // The first ext attempt allocates P at the heap base; watch the
    // function-pointer-array region the init builds there.
    guest.value_watch = mrpemu_core::cfunction::ValueWatch {
        lo: 0x0028_2050,
        hi: 0x0028_20b0,
        hits: Vec::new(),
    };
    for (name, ext) in &exts {
        eprintln!("booting {name} ({} bytes)", ext.len());
        let result = guest.boot_ext(ext, 0, 0, &mut screen);
        eprintln!(
            "  -> {:?}",
            result.as_ref().map(|_| "ok").map_err(|e| e.to_string())
        );
        if result.is_err() {
            // Dump the spin region the budget error reported so the loop
            // body can be disassembled.
            for row in 0..6 {
                let base = 0x0028_2060 + row * 16;
                let mut line = format!("  {base:08x}: ");
                for h in 0..8 {
                    line += &format!("{:04x} ", guest.r16(base + h * 2));
                }
                eprintln!("{line}");
            }
        }
        if result.is_ok() && guest.c_function != 0 {
            // Watch the stack-shell region the helper dispatches through.
            let dump = |tag: &str, guest: &Guest| {
                eprintln!("{tag}:");
                // The shell region: HANDLER_SP-0x100 .. HANDLER_SP.
                for row in 0..8 {
                    let base = 0x0027_fdb0 + row * 16;
                    let mut line = format!("  {base:08x}: ");
                    for h in 0..8 {
                        line += &format!("{:04x} ", guest.r16(base + h * 2));
                    }
                    eprintln!("{line}");
                }
            };
            dump("after-boot", &guest);
            for round in 0..3 {
                let _ = guest.send_event(0, 17, 0, &mut screen);
                dump(&format!("after-event-{}", round), &guest);
            }
            eprintln!("  events dispatched ok");
            break;
        }
    }
}
