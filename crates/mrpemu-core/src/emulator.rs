//! Machine-level orchestration for a loaded MRP application.

use std::collections::BTreeSet;
use std::path::Path;

use crate::display::{Screen, DEFAULT_HEIGHT, DEFAULT_WIDTH};
use crate::error::MrpError;
use crate::format::MrpHeader;
use crate::input::{InputState, MrpKey};
use crate::script::ScriptBridge;
use crate::vfs::MemoryVfs;

/// Frame rate reported to frontends while the VM does not drive real timing.
pub const PLACEHOLDER_FPS: f64 = 30.0;

const PLACEHOLDER_BACKGROUND: u16 = 0x1082;
const PLACEHOLDER_FRAME: u16 = 0x39E7;

/// A loaded MRP application: its package data, the phone screen, input
/// state, and the guest file tree.
///
/// The execution environment (MiniJ VM plus the `mrc_` API bridge) is not
/// implemented yet, so [`MrpMachine::run_frame`] is a placeholder that keeps
/// the screen content unchanged.
pub struct MrpMachine {
    header: MrpHeader,
    package: Vec<u8>,
    screen: Screen,
    input: InputState,
    vfs: MemoryVfs,
    script: Option<ScriptBridge>,
    boot_attempted: bool,
    boot_error: Option<String>,
    last_keys: BTreeSet<MrpKey>,
}

impl MrpMachine {
    /// Parses `data` as a `.mrp` package and prepares the machine state.
    pub fn load(data: Vec<u8>) -> Result<Self, MrpError> {
        let header = MrpHeader::parse(&data)?;
        let mut machine = Self {
            header,
            package: data,
            screen: Screen::new(DEFAULT_WIDTH, DEFAULT_HEIGHT),
            input: InputState::new(),
            vfs: MemoryVfs::new(),
            script: None,
            boot_attempted: false,
            boot_error: None,
            last_keys: BTreeSet::new(),
        };
        machine.draw_placeholder();
        Ok(machine)
    }

    /// Convenience loader that reads the package from `path` first.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, MrpError> {
        let data = std::fs::read(path.as_ref())?;
        Self::load(data)
    }

    pub fn header(&self) -> &MrpHeader {
        &self.header
    }

    /// Raw package bytes, starting with the parsed header.
    pub fn package(&self) -> &[u8] {
        &self.package
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    pub fn screen_mut(&mut self) -> &mut Screen {
        &mut self.screen
    }

    pub fn input(&self) -> &InputState {
        &self.input
    }

    pub fn input_mut(&mut self) -> &mut InputState {
        &mut self.input
    }

    pub fn vfs(&self) -> &MemoryVfs {
        &self.vfs
    }

    pub fn vfs_mut(&mut self) -> &mut MemoryVfs {
        &mut self.vfs
    }

    /// Direct access to the booted guest (diagnostics and tests).
    pub fn guest_ref(&self) -> Option<&crate::cfunction::Guest> {
        self.script
            .as_ref()
            .and_then(|bridge| bridge.vm.guest.as_ref())
    }

    /// Whether the running guest registered an ext event handler.
    pub fn ext_handler_registered(&self) -> bool {
        self.script
            .as_ref()
            .and_then(|bridge| bridge.vm.guest.as_ref())
            .map(|guest| guest.c_function != 0)
            .unwrap_or(false)
    }

    /// Missing globals the script referenced (diagnostics).
    pub fn missing_globals(&self) -> Vec<String> {
        self.script
            .as_ref()
            .map(|bridge| bridge.missing_globals())
            .unwrap_or_default()
    }

    /// The boot failure reason, once a boot was attempted and failed.
    pub fn boot_error(&self) -> Option<&str> {
        self.boot_error.as_deref()
    }

    /// Advances the application by one frame: boots through the start.mr
    /// script on first use, then delivers key edges and fires due timers.
    pub fn run_frame(&mut self) {
        if self.script.is_none() && !self.boot_attempted {
            self.boot_attempted = true;
            self.boot_guest();
        }
        let Some(bridge) = self.script.as_mut() else {
            return;
        };
        let exit_requested = bridge
            .vm
            .guest
            .as_ref()
            .map(|guest| guest.exit_requested)
            .unwrap_or(false);
        if exit_requested {
            return;
        }

        // Event pump: the guest's event handler runs first (the ext
        // registers via mr_c_function_new or testCom(2)); MR_IGNORE falls
        // through to the script's dealevent.
        let current: BTreeSet<MrpKey> = self.input.pressed().collect();
        let pressed: Vec<MrpKey> = current.difference(&self.last_keys).copied().collect();
        let released: Vec<MrpKey> = self.last_keys.difference(&current).copied().collect();
        self.last_keys = current;

        let pump = |bridge: &mut ScriptBridge, event_type: i32, p0: i32| {
            // Try the guest's ext handler first.
            if let Some(guest) = bridge.vm.guest.as_mut() {
                let mut screen = std::mem::replace(
                    &mut bridge.vm.screen,
                    crate::display::Screen::new(
                        crate::display::DEFAULT_WIDTH,
                        crate::display::DEFAULT_HEIGHT,
                    ),
                );
                let result = guest.send_event(event_type, p0, 0, &mut screen);
                bridge.vm.screen = screen;
                if !matches!(result, Ok(1)) {
                    return; // handled by the ext
                }
            }
            // Fall through to the script's dealevent.
            let _ = bridge.send_event(event_type, p0, 0);
        };

        let menu_active = bridge
            .vm
            .guest
            .as_ref()
            .map(|guest| guest.menu.is_some())
            .unwrap_or(false);
        if menu_active {
            // The platform menu owns key input while shown: Up/Down move
            // the selection, OK selects (MR_MENU_SELECT carries the item
            // index), RSK cancels (MR_MENU_RETURN), digits pick items.
            let guest = bridge.vm.guest.as_mut().expect("menu implies guest");
            if pressed.contains(&MrpKey::Up) {
                guest.move_menu_selection(-1);
            } else if pressed.contains(&MrpKey::Down) {
                guest.move_menu_selection(1);
            } else if pressed.contains(&MrpKey::Ok) || pressed.contains(&MrpKey::LeftSoft) {
                let selection = guest.menu.as_ref().map(|m| m.selection).unwrap_or(0);
                pump(bridge, 4, selection as i32);
            } else if pressed.contains(&MrpKey::RightSoft) {
                pump(bridge, 5, 0);
            } else if let Some(digit) = pressed.iter().find_map(|key| match key {
                MrpKey::Num0 => Some(9usize),
                MrpKey::Num1 => Some(0),
                MrpKey::Num2 => Some(1),
                MrpKey::Num3 => Some(2),
                MrpKey::Num4 => Some(3),
                MrpKey::Num5 => Some(4),
                MrpKey::Num6 => Some(5),
                MrpKey::Num7 => Some(6),
                MrpKey::Num8 => Some(7),
                MrpKey::Num9 => Some(8),
                _ => None,
            }) {
                pump(bridge, 4, digit as i32);
            }
        } else {
            for key in pressed {
                pump(bridge, 0, Self::mrc_key_code(key));
            }
            for key in released {
                pump(bridge, 1, Self::mrc_key_code(key));
            }
        }

        // Timers: restart the interval, then invoke the registered handler.
        let now = std::time::Instant::now();
        let due: Vec<i32> = bridge
            .vm
            .guest
            .as_ref()
            .map(|guest| {
                guest
                    .timers
                    .iter()
                    .filter(|timer| timer.next_fire <= now)
                    .map(|timer| timer.id)
                    .collect()
            })
            .unwrap_or_default();
        for id in due {
            if let Some(guest) = bridge.vm.guest.as_mut() {
                if let Some(timer) = guest.timers.iter_mut().find(|timer| timer.id == id) {
                    timer.next_fire = now + timer.interval;
                }
                let mut screen = std::mem::replace(
                    &mut bridge.vm.screen,
                    crate::display::Screen::new(
                        crate::display::DEFAULT_WIDTH,
                        crate::display::DEFAULT_HEIGHT,
                    ),
                );
                let _ = guest.fire_timer(&mut screen);
                bridge.vm.screen = screen;
            }
        }

        bridge.present();
        // Sync the presented guest screen into the machine's public screen
        // so frontends and tests observe post-boot drawing.
        self.screen = bridge.vm.screen.clone();
    }

    /// Boots the application: runs start.mr on the script bridge first
    /// (the SDK wrapper drives ext loading via `_com(800)`); when the
    /// script fails or does not produce a registered ext handler, falls
    /// back to loading `cfunction.ext` directly through the guest engine.
    fn boot_guest(&mut self) {
        let Ok(package) = crate::package::MrpPackage::parse(self.package.clone()) else {
            self.boot_error = Some("package reparse failed".to_string());
            return;
        };
        // Script path: start.mr runs on the VM and drives _com(800).
        let mut bridge = ScriptBridge::new();
        bridge.load_package(
            crate::package::MrpPackage::parse(self.package.clone())
                .expect("package parsed at load"),
        );
        bridge.create_guest();
        let script_ok = bridge.run_start_script().is_ok()
            && bridge
                .vm
                .guest
                .as_ref()
                .map(|guest| guest.c_function != 0)
                .unwrap_or(false);
        if script_ok {
            self.screen = std::mem::replace(
                &mut bridge.vm.screen,
                crate::display::Screen::new(DEFAULT_WIDTH, DEFAULT_HEIGHT),
            );
            self.script = Some(bridge);
            return;
        }
        // Direct ext path: try every .ext member in package order until one
        // registers an event handler. Self-registering images (hello) work
        // on the first try; stub loaders (test.mrp/mpc.mrp) need a later
        // member such as game.ext.
        let ext_files: Vec<(String, Vec<u8>)> = package
            .entries()
            .iter()
            .filter(|entry| entry.name.to_lowercase().ends_with(".ext"))
            .map(|entry| {
                (
                    entry.name.clone(),
                    package.read_entry(&entry.name).unwrap_or_default(),
                )
            })
            .collect();
        if ext_files.is_empty() {
            log::info!("pure-script package: no ext to load");
            return;
        }
        let mut guest =
            crate::cfunction::Guest::new(crate::cfunction::GuestConfig::default(), Vec::new());
        let mut screen = crate::display::Screen::new(DEFAULT_WIDTH, DEFAULT_HEIGHT);
        let mut last_error = None;
        for (name, ext) in &ext_files {
            match guest.boot_ext(
                ext,
                self.header.build_id,
                self.header.build_version,
                &mut screen,
            ) {
                Ok(()) if guest.c_function != 0 => {
                    log::info!(
                        "direct ext boot: {:?} via {} (app id {})",
                        self.header.display_name,
                        name,
                        self.header.build_id
                    );
                    self.screen = screen.clone();
                    self.script = Some(ScriptBridge::with_guest(guest, screen));
                    return;
                }
                Ok(_) => {
                    log::info!("ext {} loaded but did not register; trying next", name);
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                }
            }
        }
        self.boot_error =
            Some(last_error.unwrap_or_else(|| "no ext registered an event handler".to_string()));
    }

    /// MRC key codes expected by guest event handlers.
    fn mrc_key_code(key: MrpKey) -> i32 {
        match key {
            MrpKey::Num0 => 0,
            MrpKey::Num1 => 1,
            MrpKey::Num2 => 2,
            MrpKey::Num3 => 3,
            MrpKey::Num4 => 4,
            MrpKey::Num5 => 5,
            MrpKey::Num6 => 6,
            MrpKey::Num7 => 7,
            MrpKey::Num8 => 8,
            MrpKey::Num9 => 9,
            MrpKey::Star => 10,
            MrpKey::Pound => 11,
            MrpKey::Up => 12,
            MrpKey::Down => 13,
            MrpKey::Left => 14,
            MrpKey::Right => 15,
            MrpKey::Ok | MrpKey::LeftSoft => 17,
            MrpKey::RightSoft => 18,
        }
    }

    /// Paints a deterministic dark screen with a border so frontends have
    /// visible, testable output before the VM exists.
    fn draw_placeholder(&mut self) {
        let screen = &mut self.screen;
        screen.fill(PLACEHOLDER_BACKGROUND);
        let (width, height) = (screen.width(), screen.height());
        for x in 0..width {
            screen.set_pixel(x, 0, PLACEHOLDER_FRAME);
            screen.set_pixel(x, height - 1, PLACEHOLDER_FRAME);
        }
        for y in 0..height {
            screen.set_pixel(0, y, PLACEHOLDER_FRAME);
            screen.set_pixel(width - 1, y, PLACEHOLDER_FRAME);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_package() -> Vec<u8> {
        let mut data = vec![0u8; crate::format::FULL_HEADER_SIZE];
        data[0..4].copy_from_slice(&crate::format::MRP_MAGIC);
        data[4..8].copy_from_slice(&200u32.to_le_bytes());
        data[8..12].copy_from_slice(&200u32.to_le_bytes());
        data[12..16].copy_from_slice(&200u32.to_le_bytes());
        data
    }

    #[test]
    fn load_prepares_default_screen_and_placeholder() {
        let mut machine = MrpMachine::load(sample_package()).expect("sample package must load");

        assert_eq!(machine.screen().width(), DEFAULT_WIDTH);
        assert_eq!(machine.screen().height(), DEFAULT_HEIGHT);
        assert_eq!(machine.screen().pixels()[0], PLACEHOLDER_FRAME);
        assert_eq!(
            machine.screen().pixels()[PLACEHOLDER_CENTER],
            PLACEHOLDER_BACKGROUND
        );

        machine.run_frame();
        machine.run_frame();
    }

    const PLACEHOLDER_CENTER: usize =
        (DEFAULT_HEIGHT as usize / 2) * (DEFAULT_WIDTH as usize) + (DEFAULT_WIDTH as usize / 2);

    #[test]
    fn load_rejects_invalid_package() {
        let mut data = sample_package();
        data[0] = b'X';

        assert!(matches!(
            MrpMachine::load(data),
            Err(MrpError::BadMagic { .. })
        ));
    }
}
