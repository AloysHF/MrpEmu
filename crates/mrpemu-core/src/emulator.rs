//! Machine-level orchestration for a loaded MRP application.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use crate::display::{Screen, DEFAULT_HEIGHT, DEFAULT_WIDTH};
use crate::error::MrpError;
use crate::format::MrpHeader;
use crate::input::{InputState, MrpKey, TouchState};
use crate::script::ScriptBridge;
use crate::vfs::MemoryVfs;

/// Frontend frame cadence for the phone screen and input.
pub const FRAME_RATE: f64 = 30.0;

const PLACEHOLDER_BACKGROUND: u16 = 0x1082;
const PLACEHOLDER_FRAME: u16 = 0x39E7;

/// A loaded MRP application with a script VM, a native guest, and a virtual phone.
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
    last_touch: TouchState,
    /// Directory for guest-created file persistence.
    persist_dir: Option<std::path::PathBuf>,
    /// Screen resolution (detected from the corpus directory or default).
    resolution: (u16, u16),
    /// Audio sink parked until a guest exists to receive it.
    pending_audio_sink: Option<Box<dyn crate::audio::AudioSink>>,
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
            last_touch: TouchState::default(),
            persist_dir: None,
            resolution: (240, 320),
            pending_audio_sink: None,
        };
        machine.draw_placeholder();
        Ok(machine)
    }

    /// Convenience loader that reads the package from `path` first.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, MrpError> {
        let path = path.as_ref();
        let mut machine = Self::load(std::fs::read(path)?)?;
        let (width, height) = resolution_from_path(path);
        if (width, height) != machine.resolution {
            machine.set_resolution(width, height);
            machine.draw_placeholder();
        }
        Ok(machine)
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

    /// Sets the persistence directory and loads any previously saved
    /// guest files into the pending file table.
    pub fn set_persist_dir(&mut self, dir: impl Into<std::path::PathBuf>) {
        let dir = dir.into();
        self.persist_dir = Some(dir.clone());
        // Load persisted files into the bridge's pending file table so the
        // guest sees them at boot. After boot, dirty files sync on close.
        let dir = dir.clone();
        if let Some(bridge) = self.script.as_mut() {
            let _ = bridge.vm.guest.as_mut().map(|guest| {
                let _ = guest.load_persistence(&dir);
            });
        }
    }

    /// Writes all guest-created/modified files to the persistence
    /// directory. Returns the number of files written.
    pub fn flush_persistence(&mut self) -> std::io::Result<usize> {
        let Some(dir) = self.persist_dir.clone() else {
            return Ok(0);
        };
        if let Some(guest) = self
            .script
            .as_ref()
            .and_then(|bridge| bridge.vm.guest.as_ref())
        {
            guest.flush_persistence(&dir)
        } else {
            Ok(0)
        }
    }

    /// Sets the screen resolution. Call before the first `run_frame`.
    pub fn set_resolution(&mut self, width: u16, height: u16) {
        self.resolution = (width, height);
        self.screen = crate::display::Screen::new(width, height);
    }

    /// The guest platform configuration for the machine's resolution: the
    /// SDK runtime reads the screen size from the table's data slots, so
    /// every guest creation must carry `set_resolution`'s values.
    fn guest_config(&self) -> crate::cfunction::GuestConfig {
        crate::cfunction::GuestConfig {
            screen_width: self.resolution.0,
            screen_height: self.resolution.1,
            ..Default::default()
        }
    }

    fn prepare_guest(&mut self, guest: &mut crate::cfunction::Guest, elapsed: Option<Duration>) {
        guest.set_frame_time(elapsed);
        for (path, bytes) in self
            .vfs
            .paths()
            .filter_map(|path| self.vfs.read(path).map(|bytes| (path, bytes)))
        {
            if guest.file_data(path).is_none() {
                guest.files.push((path.to_string(), bytes.to_vec()));
            }
        }
        if let Some(sink) = self.pending_audio_sink.take() {
            guest.audio_sink = sink;
        }
    }

    /// Installs the audio sink used by the guest's `mr_playSound` /
    /// `mr_stopSound` calls. Applies to the live guest when one exists;
    /// otherwise the sink is parked until the next boot creates a guest.
    pub fn set_audio_sink(&mut self, sink: Box<dyn crate::audio::AudioSink>) {
        if let Some(bridge) = self.script.as_mut() {
            if let Some(guest) = bridge.vm.guest.as_mut() {
                guest.audio_sink = sink;
                return;
            }
        }
        self.pending_audio_sink = Some(sink);
    }

    /// The boot failure reason, once a boot was attempted and failed.
    pub fn boot_error(&self) -> Option<&str> {
        self.boot_error.as_deref()
    }

    /// Advances the application by one frame: boots through the start.mr
    /// script on first use, then delivers input events and fires due timers.
    pub fn run_frame(&mut self) {
        self.run_frame_with_clock(None);
    }

    /// Advances one frame using a caller-supplied, nondecreasing guest uptime.
    pub fn run_frame_at(&mut self, elapsed: Duration) {
        self.run_frame_with_clock(Some(elapsed));
    }

    fn run_frame_with_clock(&mut self, elapsed: Option<Duration>) {
        if self.script.is_none() && !self.boot_attempted {
            self.boot_attempted = true;
            self.boot_guest(elapsed);
            if let Some(error) = self.boot_error() {
                log::error!("guest boot failed: {error}");
                self.screen.fill(0);
            }
        }
        let Some(bridge) = self.script.as_mut() else {
            return;
        };
        if let Some(guest) = bridge.vm.guest.as_mut() {
            guest.set_frame_time(elapsed);
        }
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
        let touch = self.input.touch();
        let last_touch = std::mem::replace(&mut self.last_touch, touch);

        let pump = |bridge: &mut ScriptBridge, event_type: i32, p0: i32, p1: i32| {
            // Try the guest's ext handler first.
            if let Some(guest) = bridge.vm.guest.as_mut() {
                let result = guest.send_event(event_type, p0, p1, &mut bridge.vm.screen);
                if let Err(error) = &result {
                    log::debug!("guest event {event_type} failed: {error}");
                }
                if !matches!(result, Ok(1)) {
                    return; // handled by the ext
                }
            }
            // Fall through to the script's dealevent.
            let _ = bridge.send_event(event_type, p0, p1);
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
                pump(bridge, 4, selection as i32, 0);
            } else if pressed.contains(&MrpKey::RightSoft) {
                pump(bridge, 5, 0, 0);
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
                pump(bridge, 4, digit as i32, 0);
            }
        } else {
            for key in pressed {
                pump(bridge, 0, Self::mrc_key_code(key), 0);
            }
            for key in released {
                pump(bridge, 1, Self::mrc_key_code(key), 0);
            }
            let touch_event = match (last_touch.pressed, touch.pressed) {
                (false, true) => Some(2),                        // MR_MOUSE_DOWN
                (true, false) => Some(3),                        // MR_MOUSE_UP
                (true, true) if last_touch != touch => Some(12), // MR_MOUSE_MOVE
                _ => None,
            };
            if let Some(event_type) = touch_event {
                pump(bridge, event_type, i32::from(touch.x), i32::from(touch.y));
            }
        }

        // Timers: restart the interval, then invoke the registered handler.
        let due: Vec<i32> = bridge
            .vm
            .guest
            .as_ref()
            .map(|guest| {
                let now = guest.clock_now();
                guest
                    .timers
                    .iter()
                    .filter(|timer| timer.next_fire <= now)
                    .map(|timer| timer.id)
                    .collect()
            })
            .unwrap_or_default();
        for id in due {
            let ext_driven = bridge
                .vm
                .guest
                .as_ref()
                .map(|guest| guest.c_function != 0)
                .unwrap_or(false);
            if ext_driven {
                if let Some(guest) = bridge.vm.guest.as_mut() {
                    let now = guest.clock_now();
                    if let Some(timer) = guest.timers.iter_mut().find(|timer| timer.id == id) {
                        timer.next_fire = now + timer.interval;
                    }
                    if let Err(error) = guest.fire_timer(&mut bridge.vm.screen) {
                        log::debug!("guest timer failed: {error}");
                    }
                }
            } else {
                // Script-level timers route through `_com(3, fn)` /
                // `dealtimer` instead of the guest engine.
                let _ = bridge.fire_script_timer();
            }
        }

        // The reference platform starts its own timer at app start and
        // forwards ticks to the ext (helper code 2) without waiting for the
        // ext to call mr_timerStart — drive ext apps on the frame cadence
        // when they registered no timer of their own.
        let ext_driven = bridge
            .vm
            .guest
            .as_ref()
            .map(|guest| guest.c_function != 0 && guest.timers.is_empty())
            .unwrap_or(false);
        if ext_driven {
            if let Some(guest) = bridge.vm.guest.as_mut() {
                if let Err(error) = guest.fire_timer(&mut bridge.vm.screen) {
                    log::debug!("guest timer failed: {error}");
                }
            }
        }

        bridge.present();
        // Sync the presented guest screen into the machine's public screen
        // so frontends and tests observe post-boot drawing.
        self.screen.clone_from(&bridge.vm.screen);
    }

    /// Boots the application: runs start.mr on the script bridge first
    /// (the SDK wrapper drives ext loading via `_com(800)`); when the
    /// script fails or does not produce a registered ext handler, falls
    /// back to loading `cfunction.ext` directly through the guest engine.
    fn boot_guest(&mut self, elapsed: Option<Duration>) {
        let Ok(package) = crate::package::MrpPackage::parse(self.package.clone()) else {
            self.boot_error = Some("package reparse failed".to_string());
            return;
        };
        let has_ext = package
            .entries()
            .iter()
            .any(|entry| entry.name.to_ascii_lowercase().ends_with(".ext"));
        let mut script_error = None;
        if package
            .entries()
            .iter()
            .any(|entry| entry.name == "start.mr")
        {
            let mut bridge = ScriptBridge::new();
            bridge.load_package(package.clone());
            bridge.create_guest_with(self.guest_config());
            if let Some(guest) = bridge.vm.guest.as_mut() {
                self.prepare_guest(guest, elapsed);
            }
            match bridge.run_start_script() {
                Ok(()) => {
                    bridge.present();
                    let pixels = bridge.vm.screen.pixels();
                    let drew = pixels.iter().any(|&pixel| pixel != pixels[0]);
                    let scheduled = bridge
                        .vm
                        .guest
                        .as_ref()
                        .is_some_and(|guest| !guest.timers.is_empty());
                    if !has_ext || drew || scheduled {
                        self.screen.clone_from(&bridge.vm.screen);
                        self.script = Some(bridge);
                        return;
                    }
                    script_error =
                        Some("start.mr did not produce video or schedule an update".to_string());
                }
                Err(error) => {
                    log::debug!(
                        "start.mr failed: {error}; missing globals: {:?}",
                        bridge.missing_globals()
                    );
                    script_error = Some(error);
                }
            }
            if let Some(guest) = bridge.vm.guest.as_mut() {
                guest.audio_sink.stop_bgm();
                self.pending_audio_sink = Some(std::mem::replace(
                    &mut guest.audio_sink,
                    Box::new(crate::audio::NullAudioSink),
                ));
            }
        }
        let mut direct_error = None;
        if !has_ext || !self.try_direct_ext_boot(&package, elapsed, &mut direct_error) {
            self.boot_error = Some(
                direct_error
                    .or(script_error)
                    .unwrap_or_else(|| "missing application entry".to_string()),
            );
        }
    }

    /// Tries direct extensions, largest first, when the script did not start.
    fn try_direct_ext_boot(
        &mut self,
        package: &crate::package::MrpPackage,
        elapsed: Option<Duration>,
        last_error: &mut Option<String>,
    ) -> bool {
        let mut ext_files: Vec<(String, Vec<u8>)> = package
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
        ext_files.sort_by_key(|(_, data)| std::cmp::Reverse(data.len()));
        if ext_files.is_empty() {
            log::info!("pure-script package: no ext to load");
            return false;
        }
        // Seed the guest with the package members so loader-stub exts can
        // read the application resources through mr_readFile, plus the raw
        // package bytes under `pack_filename` — the SDK runtime's internal
        // resource loader opens the pack itself and extracts members.
        let mut packaged_files: Vec<(String, Vec<u8>)> = package
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.name.clone(),
                    package.read_entry(&entry.name).unwrap_or_default(),
                )
            })
            .collect();
        packaged_files.push(("app.mrp".to_string(), self.package.clone()));
        for (name, ext) in &ext_files {
            // A fresh guest per attempt: a failed boot can leave the heap
            // drained (the SDK init allocates large pools before failing),
            // which would poison every later attempt.
            let mut guest =
                crate::cfunction::Guest::new(self.guest_config(), packaged_files.clone());
            self.prepare_guest(&mut guest, elapsed);
            let mut screen = Screen::new(self.resolution.0, self.resolution.1);
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
                    return true;
                }
                Ok(_) => {
                    log::info!("ext {} loaded but did not register; trying next", name);
                }
                Err(error) => {
                    log::debug!("ext {name} boot failed: {error}");
                    *last_error = Some(error.to_string());
                }
            }
            guest.audio_sink.stop_bgm();
            self.pending_audio_sink = Some(std::mem::replace(
                &mut guest.audio_sink,
                Box::new(crate::audio::NullAudioSink),
            ));
        }
        false
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
            MrpKey::LeftSoft => 17,
            MrpKey::RightSoft => 18,
            MrpKey::Ok => 20,
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

fn resolution_from_path(path: &Path) -> (u16, u16) {
    for component in path.components().rev() {
        let label = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        if label.contains("240x400") || label.contains("240×400") {
            return (240, 400);
        }
        if label.contains("320x480") || label.contains("320×480") {
            return (320, 480);
        }
        if label.contains("240x320") || label.contains("240×320") {
            return (240, 320);
        }
    }
    (DEFAULT_WIDTH, DEFAULT_HEIGHT)
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

    #[test]
    fn delivers_touch_edges_and_coordinates_to_the_native_handler() {
        use crate::cfunction::{Guest, EXT_BASE};

        let mut machine = MrpMachine::load(sample_package()).unwrap();
        let mut guest = Guest::new(machine.guest_config(), Vec::new());
        let recorded = EXT_BASE + 20;
        for (i, word) in [
            0xe59f_3008, // ldr r3, [pc, #8]
            0xe883_0007, // stmia r3, {r0-r2}
            0xe3a0_0000, // mov r0, #0
            0xe12f_ff1e, // bx lr
            recorded,
        ]
        .into_iter()
        .enumerate()
        {
            guest.w32(EXT_BASE + i as u32 * 4, word);
        }
        guest.event_function = EXT_BASE;
        machine.script = Some(ScriptBridge::with_guest(guest, machine.screen.clone()));

        let untouched = [u32::MAX; 3];
        for (frame, ((x, y, pressed), expected)) in [
            ((10, 20, false), untouched),
            ((11, 22, true), [2, 11, 22]),
            ((13, 25, true), [12, 13, 25]),
            ((13, 25, true), untouched),
            ((14, 27, false), [3, 14, 27]),
            ((16, 29, false), untouched),
        ]
        .into_iter()
        .enumerate()
        {
            let guest = machine.script.as_mut().unwrap().vm.guest.as_mut().unwrap();
            for offset in [0, 4, 8] {
                guest.w32(recorded + offset, u32::MAX);
            }
            machine.input_mut().set_touch(x, y, pressed);
            machine.run_frame_at(Duration::from_millis(frame as u64 * 33));
            let guest = machine.guest_ref().unwrap();
            let actual = [0, 4, 8].map(|offset| guest.r32(recorded + offset));
            assert_eq!(actual, expected, "frame {frame}");
        }
    }
}
