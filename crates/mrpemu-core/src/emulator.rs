//! Machine-level orchestration for a loaded MRP application.

use std::path::Path;

use crate::display::{Screen, DEFAULT_HEIGHT, DEFAULT_WIDTH};
use crate::error::MrpError;
use crate::format::MrpHeader;
use crate::input::InputState;
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
#[derive(Debug)]
pub struct MrpMachine {
    header: MrpHeader,
    package: Vec<u8>,
    screen: Screen,
    input: InputState,
    vfs: MemoryVfs,
    warned_vm_missing: bool,
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
            warned_vm_missing: false,
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

    /// Advances the application by one frame.
    ///
    /// Placeholder until the MiniJ VM lands: logs a single warning, then
    /// keeps the placeholder screen content unchanged. Frontends can rely on
    /// the call being cheap and side-effect free.
    pub fn run_frame(&mut self) {
        if !self.warned_vm_missing {
            log::warn!(
                "mythroad VM execution is not implemented yet; \
                 the screen shows a placeholder frame"
            );
            self.warned_vm_missing = true;
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
