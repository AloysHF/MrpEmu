//! MrpEmu core — a platform-independent emulator for Sky-mobi mythroad (MRP)
//! feature-phone applications.
//!
//! MRP applications are `.mrp` packages that run on the mythroad runtime of
//! MTK-based Chinese feature phones. The runtime executes MiniJ bytecode and
//! bridges `mrc_`-prefixed platform APIs (display, input, files, network) to
//! the phone firmware; some packages additionally ship `ext` ARM native
//! libraries.
//!
//! # Status
//!
//! The crate currently implements `.mrp` package header parsing plus the
//! screen, input, and virtual file system scaffolding around the execution
//! environment. The MiniJ VM and the `mrc_` API bridge are not implemented
//! yet; [`MrpMachine::run_frame`] is a documented placeholder that keeps the
//! screen content unchanged.

pub mod display;
pub mod emulator;
pub mod error;
pub mod format;
pub mod input;
pub mod vfs;

pub use display::Screen;
pub use emulator::MrpMachine;
pub use error::MrpError;
pub use format::MrpHeader;
pub use input::{InputState, MrpKey};
pub use vfs::MemoryVfs;
