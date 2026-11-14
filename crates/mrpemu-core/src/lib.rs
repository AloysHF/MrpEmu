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

pub mod audio;
pub mod cfunction;
pub mod display;
pub mod emulator;
pub mod error;
pub mod fmt;
pub mod format;
pub mod input;
pub mod lua_pattern;
pub mod luastate;
pub mod md5;
pub mod opcodes;
pub mod package;
pub mod script;
pub mod undump;
pub mod value_helpers;
pub mod vfs;
pub mod vm;

pub use cfunction::{Guest, GuestConfig};
pub use display::Screen;
pub use emulator::MrpMachine;
pub use error::MrpError;
pub use format::MrpHeader;
pub use input::{InputState, MrpKey};
pub use opcodes::{Instruction, Op};
pub use package::MrpPackage;
pub use script::ScriptBridge;
pub use undump::{Chunk, Constant, Proto};
pub use vfs::MemoryVfs;
pub use vm::{Key, Table, Value, Vm};
