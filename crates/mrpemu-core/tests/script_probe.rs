//! Temporary probe: drives the script boot path directly. Not a gate.
use mrpemu_core::script::ScriptBridge;
use mrpemu_core::MrpKey;

#[test]
#[ignore = "manual probe"]
fn probe_script_boot() {
    let _ = env_logger::try_init();
    let path = std::env::var("PROBE_PKG").unwrap();
    let mut bridge = ScriptBridge::new();
    let data = std::fs::read(&path).unwrap();
    let package = mrpemu_core::package::MrpPackage::parse(data).unwrap();
    bridge.load_package(package);
    bridge.create_guest_with(mrpemu_core::cfunction::GuestConfig::default());
    let result = bridge.run_start_script();
    eprintln!(
        "start.mr result: {:?}",
        result.as_ref().map(|_| "ok").map_err(|e| e.to_string())
    );
    let guest_ready = bridge.vm.guest.as_ref().map(|g| g.c_function).unwrap_or(0);
    eprintln!("guest c_function={guest_ready:#x}");
    // Pump frames through the bridge.
    for frame in 0..60 {
        if frame % 20 == 5 {
            bridge.send_event(0, 17, 0).ok();
        }
        let _ = bridge.fire_script_timer();
        if let Some(guest) = bridge.vm.guest.as_mut() {
            let mut screen = std::mem::replace(
                &mut bridge.vm.screen,
                mrpemu_core::display::Screen::new(240, 320),
            );
            let _ = guest.fire_timer(&mut screen);
            bridge.vm.screen = screen;
        }
        if frame % 10 == 0 {
            let nonblank = bridge
                .vm
                .screen
                .pixels()
                .iter()
                .filter(|&&p| p != 0)
                .count();
            eprintln!("[frame {frame}] nonblank={nonblank}");
        }
    }
    let nonblank = bridge
        .vm
        .screen
        .pixels()
        .iter()
        .filter(|&&p| p != 0)
        .count();
    eprintln!("[final] nonblank={nonblank}");
    let _ = MrpKey::Ok;
}
