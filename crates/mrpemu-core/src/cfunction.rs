//! Guest machine for `cfunction.ext`: guest memory, the `mr_table` function
//! table bridge, and the ARM execution loop.
//!
//! An ext image is a raw ARM code blob whose first 8 bytes are reserved:
//! the loader writes the `mr_table` address at base+0 and the
//! `mr_c_function_st` address at base+4, and `mr_c_function_load` sits at a
//! fixed base+8 entry (it receives no useful register arguments and instead
//! reads both pointers through its own base). During load the ext calls
//! table slot 25 (`mr_c_function_new`) to register its event handler; the
//! host then drives the ext by calling that handler as
//! `(P, code, input, input_len, output*, output_len*)` with codes 6/8 for
//! the boot handshake and 0/1 for init and events.
//!
//! Guest→host calls go through a bridge page: every function slot in
//! `mr_func_tab` points at a distinct bridge address, and the run loop
//! intercepts the PC before it can execute there, dispatches to the host
//! implementation for that slot, and returns via LR. This mirrors the
//! SWI-stub approach of the reference loader without relying on SWI
//! emulation.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use armv4t_emu::{reg, Cpu, Memory};

/// General-purpose register numbers (the crate only names the specials).
const R0: reg::Reg = 0;
const R1: reg::Reg = 1;
const R2: reg::Reg = 2;
const R3: reg::Reg = 3;

use crate::error::MrpError;
use crate::fmt::{self, Arg};
use crate::vm::Value;

// ---------------------------------------------------------------------------
// Guest memory layout
// ---------------------------------------------------------------------------

/// Guest memory geometry — mirrors the reference firmware map (ext code
/// 0x80000-0x180000, stack 0x180000-0x280000 growing down from 0x280000,
/// bridge/tables at 0x280000, heap above). The SDK runtime derives its
/// shell/control placements from LG_mem_end and the stack region, so the
/// compressed layouts we tried before collided with them.
///
/// `mr_func_tab`: one pointer per table slot (149 slots).
pub const TABLE_BASE: u32 = 0x0028_0000;
pub const TABLE_SLOTS: usize = 149;
/// One bridge word per slot; guest calls into these addresses are
/// intercepted before execution.
pub const BRIDGE_BASE: u32 = 0x0028_0400;
/// The `sendAppEvent` callback bridge just past the function table: the
/// chunk descriptor points here so the ext's timer wrappers reach the
/// platform (code 0 = start, 1 = stop; `(ext, chunk, code, p0, p1)`).
pub const SEND_APP_EVENT_ADDR: u32 = BRIDGE_BASE + (TABLE_SLOTS as u32) * 4 + 8;
/// Data variables referenced by the pointer-valued table slots.
pub const DATA_BASE: u32 = 0x0028_0800;
/// The internal (VM) table `_mr_c_internal_table`: 78 pointer entries plus
/// the VM/timer state cells. Sits in the free region between the data-slot
/// area and the heap — the 128-byte data-slot stride would otherwise place
/// other slot data inside the table's 312-byte pointer array.
pub const INTERNAL_TABLE_BASE: u32 = 0x0028_1800;
/// One dispatch address per `_mr_c_internal_table` entry 16-76 (the
/// `asm_mrp_*` Lua C API wrappers). Guest calls into these addresses are
/// intercepted before execution and forwarded to the host Lua state.
pub const VM_BRIDGE_BASE: u32 = 0x0028_1A00;
/// Number of Lua C API bridge slots: entries 16..=76 (77 is NULL).
pub const VM_BRIDGE_COUNT: u32 = 61;
/// The magic `mrp_State*` the `vm_state` cell exports. Bridge calls accept
/// this value as `L`; the state itself lives host-side.
pub const VM_STATE_ADDR: u32 = 0x0028_1C00;
/// Static strings the Lua bridge hands back to the guest (type names for
/// `mrp_typename`/`mrp_shorttypename`): name `i` lives at
/// `VM_STRINGS_BASE + i * 16`, NUL-terminated.
pub const VM_STRINGS_BASE: u32 = 0x0028_1D00;
/// Where the ext image is loaded.
pub const EXT_BASE: u32 = 0x0008_0000;
pub const EXT_MAX_SIZE: u32 = 0x0010_0000;
/// Region backing the `mr_lg_mem` "large memory" pool. In the reference
/// runtime `mr_malloc` *is* the large-memory allocator: the exported
/// LG_mem_* slots describe this pool and guests walk its free list
/// (`{next, len}` nodes relative to the base), so the heap and the pool are
/// the same region.
pub const LG_MEM_BASE: u32 = HEAP_BASE;
pub const LG_MEM_LEN: u32 = HEAP_LEN;
/// First-fit heap used by `mr_malloc`/`mr_free`. 8 MB: the SDK runtime's
/// init asks for a 4 MB large-memory pool in one block.
pub const HEAP_BASE: u32 = 0x0028_2000;
pub const HEAP_LEN: u32 = 8 * 1024 * 1024;
/// The platform-owned RGB565 buffer sits above the SDK heap.
pub const SCREENBUF: u32 = HEAP_BASE + HEAP_LEN;
pub const SCREENBUF_SIZE: u32 = 240 * 320 * 2;
/// The guest stack grows down from here (the real firmware's stack region
/// 0x180000-0x280000, sitting between the ext code area and the tables).
pub const STACK_TOP: u32 = 0x0028_0000;
pub const MEM_SIZE: u32 = EVENT_RECORD + 0x1000;
const BITMAP_TABLE: u32 = SCREENBUF + 320 * 480 * 4;
const SCREEN_BITMAP: u32 = BITMAP_TABLE + 30 * 16;
const TILE_TABLE: u32 = SCREEN_BITMAP + 16;
const MAP_TABLE: u32 = TILE_TABLE + 3 * 20;
const SOUND_TABLE: u32 = MAP_TABLE + 3 * 4;
const SPRITE_TABLE: u32 = SOUND_TABLE + 5 * 12;
const FONT_METRICS: u32 = SPRITE_TABLE + 10 * 2;
const STORAGE_PATH_BUFFER: u32 = FONT_METRICS + 4;
const EVENT_RECORD: u32 = STORAGE_PATH_BUFFER + 1024;
const EXTENDED_RAM_BASE: u32 = MEM_SIZE.next_multiple_of(0x1000);
const EXTENDED_RAM_END: u32 = EXTENDED_RAM_BASE + 12 * 1024 * 1024;
const DEFAULT_STORAGE_ROOT: &str = "c:/mythroad/";

/// Largest single allocation accepted from the heap.
const HEAP_MAX_ALLOC: u32 = HEAP_LEN - 16;
/// First-fit bookkeeping: allocations are 8-aligned and never split below
/// this size, mirroring the reference allocator.
const HEAP_MIN_PIECE: u32 = 8;
/// Cap on guest instructions per host call, so runaway ext code surfaces as
/// an error instead of a hang.
pub const DEFAULT_INSTRUCTION_LIMIT: u64 = 32_000_000;
// Startup can unpack a whole archive and probe the guest's memory in one call.
pub(crate) const BOOT_INSTRUCTION_LIMIT: u64 = 128_000_000;
const CLOCK_POLL_INSTRUCTIONS_PER_MS: u64 = 16_384;
const MYTHROAD_MINI_VERSION: i32 = 2011;

/// Cross-call instruction budget (0 = unlimited) shared by the MiniJ VM and
/// the ARM guest engine. The corpus gate sets a finite value so a guest or
/// script stuck in an infinite loop classifies as hung instead of burning
/// minutes per package.
static INSTRUCTION_BUDGET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static INSTRUCTIONS_USED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Sets the total instruction budget for subsequently executed script and
/// guest code (0 = unlimited). Intended for headless survey gates.
pub fn set_instruction_budget(budget: u64) {
    INSTRUCTION_BUDGET.store(budget, std::sync::atomic::Ordering::Relaxed);
}

/// Clears the shared instruction counter (the budget itself is unchanged).
pub fn reset_instruction_counter() {
    INSTRUCTIONS_USED.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// Charges one instruction against the shared budget; returns `true` when
/// the budget is finite and exhausted.
pub(crate) fn charge_instruction() -> bool {
    let budget = INSTRUCTION_BUDGET.load(std::sync::atomic::Ordering::Relaxed);
    if budget == 0 {
        return false;
    }
    INSTRUCTIONS_USED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= budget
}

// ---------------------------------------------------------------------------
// Function table description (slot order from the reference loader)
// ---------------------------------------------------------------------------

/// Data variables exported through pointer-valued `mr_func_tab` slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSlot {
    ScreenWidth,
    ScreenHeight,
    ScreenBit,
    ScreenBufPtr,
    PackFilename,
    StartFilename,
    OldPackFilename,
    OldStartFilename,
    RamFile,
    RamFileLen,
    SoundOn,
    ShakeOn,
    LgMemBase,
    LgMemLen,
    LgMemEnd,
    LgMemLeft,
    LgMemMin,
    LgMemTop,
    LgMemFree,
    SmsCfgBuf,
    CInternalTable,
    CPortTable,
    StartFileParameter,
    SmsReturnFlag,
    SmsReturnVal,
    ExitCb,
    ExitCbData,
    Entry,
    BitmapTable,
    TileTable,
    MapTable,
    SoundTable,
    SpriteTable,
}

/// Classification of one `mr_func_tab` slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// A bridged host function.
    Func,
    /// A pointer to guest data; the slot value is an address.
    Data(DataSlot),
}

/// The slot layout of `mr_func_tab`, in guest-visible order.
pub const SLOTS: [Slot; TABLE_SLOTS] = {
    use DataSlot::*;
    [
        Slot::Func,                     // 0 mr_malloc
        Slot::Func,                     // 1 mr_free
        Slot::Func,                     // 2 mr_realloc
        Slot::Func,                     // 3 mr_memcpy
        Slot::Func,                     // 4 mr_memmove
        Slot::Func,                     // 5 mr_strcpy
        Slot::Func,                     // 6 mr_strncpy
        Slot::Func,                     // 7 mr_strcat
        Slot::Func,                     // 8 mr_strncat
        Slot::Func,                     // 9 mr_memcmp
        Slot::Func,                     // 10 mr_strcmp
        Slot::Func,                     // 11 mr_strncmp
        Slot::Func,                     // 12 mr_strcoll
        Slot::Func,                     // 13 mr_memchr
        Slot::Func,                     // 14 mr_memset
        Slot::Func,                     // 15 mr_strlen
        Slot::Func,                     // 16 mr_strstr
        Slot::Func,                     // 17 mr_sprintf
        Slot::Func,                     // 18 mr_atoi
        Slot::Func,                     // 19 mr_strtoul
        Slot::Func,                     // 20 mr_rand
        Slot::Func,                     // 21 reserved
        Slot::Func,                     // 22 reserved
        Slot::Data(CInternalTable),     // 23
        Slot::Data(CPortTable),         // 24
        Slot::Func,                     // 25 mr_c_function_new
        Slot::Func,                     // 26 mr_printf
        Slot::Func,                     // 27 mr_mem_get
        Slot::Func,                     // 28 mr_mem_free
        Slot::Func,                     // 29 mr_refreshBuffer
        Slot::Func,                     // 30 mr_getCharBitmap
        Slot::Func,                     // 31 mr_timerStart
        Slot::Func,                     // 32 mr_timerStop
        Slot::Func,                     // 33 mr_getTime
        Slot::Func,                     // 34 mr_getDatetime
        Slot::Func,                     // 35 mr_getUserInfo
        Slot::Func,                     // 36 mr_sleep
        Slot::Func,                     // 37 mr_plat
        Slot::Func,                     // 38 mr_platEx
        Slot::Func,                     // 39 mr_ferrno
        Slot::Func,                     // 40 mr_open
        Slot::Func,                     // 41 mr_close
        Slot::Func,                     // 42 mr_info
        Slot::Func,                     // 43 mr_write
        Slot::Func,                     // 44 mr_read
        Slot::Func,                     // 45 mr_seek
        Slot::Func,                     // 46 mr_getLen
        Slot::Func,                     // 47 mr_remove
        Slot::Func,                     // 48 mr_rename
        Slot::Func,                     // 49 mr_mkDir
        Slot::Func,                     // 50 mr_rmDir
        Slot::Func,                     // 51 mr_findStart
        Slot::Func,                     // 52 mr_findGetNext
        Slot::Func,                     // 53 mr_findStop
        Slot::Func,                     // 54 mr_exit
        Slot::Func,                     // 55 mr_startShake
        Slot::Func,                     // 56 mr_stopShake
        Slot::Func,                     // 57 mr_playSound
        Slot::Func,                     // 58 mr_stopSound
        Slot::Func,                     // 59 mr_sendSms
        Slot::Func,                     // 60 mr_call
        Slot::Func,                     // 61 mr_getNetworkID
        Slot::Func,                     // 62 mr_connectWAP
        Slot::Func,                     // 63 mr_menuCreate
        Slot::Func,                     // 64 mr_menuSetItem
        Slot::Func,                     // 65 mr_menuShow
        Slot::Func,                     // 66 reserved
        Slot::Func,                     // 67 mr_menuRelease
        Slot::Func,                     // 68 mr_menuRefresh
        Slot::Func,                     // 69 mr_dialogCreate
        Slot::Func,                     // 70 mr_dialogRelease
        Slot::Func,                     // 71 mr_dialogRefresh
        Slot::Func,                     // 72 mr_textCreate
        Slot::Func,                     // 73 mr_textRelease
        Slot::Func,                     // 74 mr_textRefresh
        Slot::Func,                     // 75 mr_editCreate
        Slot::Func,                     // 76 mr_editRelease
        Slot::Func,                     // 77 mr_editGetText
        Slot::Func,                     // 78 mr_winCreate
        Slot::Func,                     // 79 mr_winRelease
        Slot::Func,                     // 80 mr_getScreenInfo
        Slot::Func,                     // 81 mr_initNetwork
        Slot::Func,                     // 82 mr_closeNetwork
        Slot::Func,                     // 83 mr_getHostByName
        Slot::Func,                     // 84 mr_socket
        Slot::Func,                     // 85 mr_connect
        Slot::Func,                     // 86 mr_closeSocket
        Slot::Func,                     // 87 mr_recv
        Slot::Func,                     // 88 mr_recvFrom
        Slot::Func,                     // 89 mr_send
        Slot::Func,                     // 90 mr_sendTo
        Slot::Data(ScreenBufPtr),       // 91
        Slot::Data(ScreenWidth),        // 92
        Slot::Data(ScreenHeight),       // 93
        Slot::Data(ScreenBit),          // 94
        Slot::Data(BitmapTable),        // 95 mr_bitmap
        Slot::Data(TileTable),          // 96 mr_tile
        Slot::Data(MapTable),           // 97 mr_map
        Slot::Data(SoundTable),         // 98 mr_sound
        Slot::Data(SpriteTable),        // 99 mr_sprite
        Slot::Data(PackFilename),       // 100
        Slot::Data(StartFilename),      // 101
        Slot::Data(OldPackFilename),    // 102
        Slot::Data(OldStartFilename),   // 103
        Slot::Data(RamFile),            // 104
        Slot::Data(RamFileLen),         // 105
        Slot::Data(SoundOn),            // 106
        Slot::Data(ShakeOn),            // 107
        Slot::Data(LgMemBase),          // 108
        Slot::Data(LgMemLen),           // 109
        Slot::Data(LgMemEnd),           // 110
        Slot::Data(LgMemLeft),          // 111
        Slot::Data(SmsCfgBuf),          // 112
        Slot::Func,                     // 113 mr_md5Init
        Slot::Func,                     // 114 mr_md5Append
        Slot::Func,                     // 115 mr_md5Finish
        Slot::Func,                     // 116 mr_loadSmsCfg
        Slot::Func,                     // 117 mr_saveSmsCfg
        Slot::Func,                     // 118 mr_dispUpEx
        Slot::Func,                     // 119 mr_drawPoint
        Slot::Func,                     // 120 mr_drawBitmap
        Slot::Func,                     // 121 mr_drawBitmapEx
        Slot::Func,                     // 122 mr_drawRect
        Slot::Func,                     // 123 mr_drawText
        Slot::Func,                     // 124 mr_bitmapCheck
        Slot::Func,                     // 125 mr_readFile
        Slot::Func,                     // 126 mr_wstrlen
        Slot::Func,                     // 127 mr_registerAPP
        Slot::Func,                     // 128 mr_drawTextEx
        Slot::Func,                     // 129 mr_effSetCon
        Slot::Func,                     // 130 mr_testCom
        Slot::Func,                     // 131 mr_testCom1
        Slot::Func,                     // 132 mr_c2u
        Slot::Func,                     // 133 mr_div
        Slot::Func,                     // 134 mr_mod
        Slot::Data(LgMemMin),           // 135
        Slot::Data(LgMemTop),           // 136
        Slot::Func,                     // 137 mr_updCrc
        Slot::Data(StartFileParameter), // 138
        Slot::Data(SmsReturnFlag),      // 139
        Slot::Data(SmsReturnVal),       // 140
        Slot::Func,                     // 141 mr_unzip
        Slot::Data(ExitCb),             // 142
        Slot::Data(ExitCbData),         // 143
        Slot::Data(Entry),              // 144
        Slot::Func,                     // 145 mr_platDrawChar
        Slot::Data(LgMemFree),          // 146
        Slot::Func,                     // 147 mr_transBitmapDraw
        Slot::Func,                     // 148 mr_drawRegion
    ]
};

/// Guest platform configuration filled into the data slots at boot.
#[derive(Debug, Clone)]
pub struct GuestConfig {
    pub screen_width: u16,
    pub screen_height: u16,
    /// IMEI/IMSI/vendor strings exposed to the guest.
    pub imei: String,
    pub imsi: String,
    pub vendor: String,
    pub device: String,
    /// Name of the running package file, as the guest sees it.
    pub pack_filename: String,
}

impl Default for GuestConfig {
    fn default() -> Self {
        Self {
            screen_width: 240,
            screen_height: 320,
            imei: "000000000000000".to_string(),
            imsi: "000000000000000".to_string(),
            vendor: "MRPEMU".to_string(),
            device: "MRPEMU".to_string(),
            pack_filename: "app.mrp".to_string(),
        }
    }
}

/// An active guest timer registered through `mr_timerStart`.
#[derive(Debug, Clone)]
pub struct GuestTimer {
    pub id: i32,
    pub interval: Duration,
    pub next_fire: Instant,
}

/// The guest CPU, memory, and registered ext state.
pub struct Guest {
    pub mem: Vec<u8>,
    pub cpu: Cpu,
    config: GuestConfig,
    uses_explicit_video: bool,
    started_at: Instant,
    frame_time: Option<Duration>,
    sleep_elapsed: Duration,
    rand_state: u32,
    /// Free guest blocks as (offset, size) in guest free-list order.
    heap_free_blocks: Vec<(u32, u32)>,
    heap_extent: u32,
    heap_left: u32,
    heap_min: u32,
    heap_top: u32,
    extended_allocations: BTreeMap<u32, u32>,
    offline_sockets: u16,
    /// Guest address of the ext's registered event handler.
    pub c_function: u32,
    /// Guest address of the ext's `mr_c_function_st`.
    pub c_function_p: u32,
    ext_load_base: u32,
    /// Guest addresses registered through `mr_testCom` (codes 2..6).
    pub event_function: u32,
    pub timer_function: u32,
    stop_function: u32,
    pause_app_function: u32,
    resume_app_function: u32,
    pub timers: Vec<GuestTimer>,
    pub next_timer_id: i32,
    /// Total guest instructions executed across all calls; the corpus gate
    /// sets a budget so a guest stuck in an infinite loop classifies fast.
    pub total_instructions: u64,
    /// Optional value-range write watch (set by boot probes to trace which
    /// store clobbers an ext's dispatch cells).
    pub value_watch: ValueWatch,
    /// Audio output handed to the guest's `mr_playSound`/`mr_stopSound`
    /// calls. Defaults to the discarding sink; frontends install a real
    /// device backend.
    pub audio_sink: Box<dyn crate::audio::AudioSink>,
    /// Snapshot of the helper dispatch shell the SDK runtime copies below
    /// its own init frame. Captured after the boot handshake and restored
    /// before every later helper call: a dispatched callback's deeper
    /// frame writes over the shell's dispatch-target cell, and without the
    /// restore every second call jumps to garbage.
    stub_snapshot: Option<Vec<u8>>,
    /// Execution watch: when the pc enters `[lo, hi)` the loop dumps the
    /// branch ring and registers once (boot probes use this to catch the
    /// jump into a region being executed as code).
    pub exec_watch: Option<(u32, u32)>,
    pub exec_watch_fired: bool,
    /// Guest file handles opened through `mr_open`.
    pub open_files: Vec<OpenFile>,
    /// Package/virtual-file contents readable through `mr_readFile`,
    /// `mr_open`, and friends: `(name, decompressed bytes)`.
    pub files: Vec<(String, Vec<u8>)>,
    storage_root: String,
    pub exit_requested: bool,
    /// Diagnostics: how often each table slot was called.
    pub slot_calls: [u64; TABLE_SLOTS],
    /// Heap address of the single glyph export buffer (lazily allocated).
    glyph_buf: u32,
    dirty_files: std::collections::HashSet<String>,
    directories: std::collections::HashSet<String>,
    md5_contexts: std::collections::HashMap<u32, crate::md5::Md5>,
    /// Loaded bitmaps: id → RGB565 pixel data.
    pub bitmaps: std::collections::HashMap<u32, Bitmap>,
    /// Monotonically increasing handle for GUI controls (menu/dialog/text/edit/win).
    pub next_gui_handle: u32,
    /// The active platform menu (runtime-rendered, key-driven).
    pub menu: Option<MenuState>,
    pub(crate) edit: Option<EditState>,
    /// The host-side Lua state the ext drives through the internal table's
    /// `asm_mrp_*` bridge (entries 16-76).
    pub lua: crate::luastate::LuaState,
    /// Return value of the last internal-table bridge call.
    vm_bridge_ret: u32,
    /// Guest-readable backing for Lua string results: content → heap block.
    lua_string_ptrs: std::collections::HashMap<Vec<u8>, u32>,
    /// The MiniJ VM that runs bytecode closures loaded through the Lua
    /// bridge; created lazily on the first bytecode call.
    lua_vm: Option<Box<crate::vm::Vm>>,
}

/// A runtime menu: title, items, and the current selection. The platform
/// menu owns key input while shown; OK selects, RSK cancels.
#[derive(Debug, Clone, Default)]
pub struct MenuState {
    pub handle: u32,
    pub title: Vec<u8>,
    pub items: Vec<Vec<u8>>,
    pub count: usize,
    pub selection: usize,
}

pub(crate) struct EditState {
    handle: u32,
    pub title: String,
    pub text: Vec<u16>,
    kind: u32,
    max_chars: usize,
    buffer: u32,
    buffer_size: u32,
    pub visible: bool,
    pub confirmation: Option<(crate::input::MrpKey, i32)>,
}

impl EditState {
    pub fn append(&mut self, ch: char) {
        let allowed = match self.kind {
            1 => ch.is_ascii_digit(),
            3 => ch.is_alphabetic(),
            _ => true,
        };
        if allowed && !ch.is_control() && ch.len_utf16() == 1 && self.text.len() < self.max_chars {
            self.text.push(ch as u16);
        }
    }

    pub fn display_text(&self) -> String {
        if self.kind == 2 {
            "*".repeat(self.text.len())
        } else {
            String::from_utf16_lossy(&self.text)
        }
    }
}

/// A loaded bitmap with dimensions and RGB565 pixel data.
#[derive(Debug, Clone)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u16>,
}

#[derive(Debug, Clone)]
pub struct OpenFile {
    file_index: usize,
    pos: usize,
    readable: bool,
    writable: bool,
    closed: bool,
}

impl Guest {
    /// Creates the guest memory image and fills every table slot. `files`
    /// backs `mr_readFile`/`mr_open` (package members and VFS content).
    pub fn new(config: GuestConfig, files: Vec<(String, Vec<u8>)>) -> Self {
        let mut guest = Self {
            mem: vec![0; MEM_SIZE as usize],
            cpu: Cpu::new(),
            config,
            uses_explicit_video: false,
            started_at: Instant::now(),
            frame_time: None,
            sleep_elapsed: Duration::ZERO,
            rand_state: 0x1234_5678,
            heap_free_blocks: vec![(0, HEAP_LEN)],
            heap_extent: HEAP_LEN,
            heap_left: HEAP_LEN,
            heap_min: HEAP_LEN,
            heap_top: 0,
            extended_allocations: BTreeMap::new(),
            offline_sockets: 0,
            c_function: 0,
            c_function_p: 0,
            ext_load_base: EXT_BASE,
            event_function: 0,
            timer_function: 0,
            stop_function: 0,
            pause_app_function: 0,
            resume_app_function: 0,
            timers: Vec::new(),
            next_timer_id: 1,
            total_instructions: 0,
            value_watch: ValueWatch::default(),
            audio_sink: Box::new(crate::audio::NullAudioSink),
            stub_snapshot: None,
            exec_watch: None,
            exec_watch_fired: false,
            open_files: Vec::new(),
            files,
            storage_root: DEFAULT_STORAGE_ROOT.to_string(),
            exit_requested: false,
            slot_calls: [0; TABLE_SLOTS],
            glyph_buf: 0,
            dirty_files: std::collections::HashSet::new(),
            directories: std::collections::HashSet::new(),
            md5_contexts: std::collections::HashMap::new(),
            bitmaps: std::collections::HashMap::new(),
            next_gui_handle: 0,
            menu: None,
            edit: None,
            lua: crate::luastate::LuaState::new(),
            vm_bridge_ret: 0,
            lua_string_ptrs: std::collections::HashMap::new(),
            lua_vm: None,
        };
        guest.install_table();
        // The heap starts as one whole free block; the offset `HEAP_LEN`
        // terminates the free list (an empty list can never be offset 0).
        guest.w32(HEAP_BASE, HEAP_LEN);
        guest.w32(HEAP_BASE + 4, HEAP_LEN);
        guest
    }

    fn install_table(&mut self) {
        for (index, slot) in SLOTS.iter().enumerate() {
            let address = match slot {
                Slot::Func => BRIDGE_BASE + (index as u32) * 4,
                Slot::Data(data) => self.data_slot_address(*data),
            };
            self.w32(TABLE_BASE + (index as u32) * 4, address);
        }
        // Fill the screen data slots and the export strings.
        let w = u32::from(self.config.screen_width);
        let h = u32::from(self.config.screen_height);
        self.set_data(DataSlot::ScreenWidth, w);
        self.set_data(DataSlot::ScreenHeight, h);
        self.set_data(DataSlot::ScreenBit, 16);
        self.set_data(DataSlot::ScreenBufPtr, SCREENBUF);
        self.w16(SCREEN_BITMAP, w as u16);
        self.w16(SCREEN_BITMAP + 2, h as u16);
        self.w32(SCREEN_BITMAP + 4, w * h * 2);
        self.w32(SCREEN_BITMAP + 12, SCREENBUF);
        for tile in 0..3 {
            self.w16(TILE_TABLE + tile * 20 + 12, w as u16);
            self.w16(TILE_TABLE + tile * 20 + 14, h as u16);
        }
        self.set_data(DataSlot::LgMemBase, LG_MEM_BASE);
        self.set_data(DataSlot::LgMemLen, LG_MEM_LEN);
        self.set_data(DataSlot::LgMemEnd, LG_MEM_BASE + LG_MEM_LEN);
        self.set_data(DataSlot::LgMemLeft, LG_MEM_LEN);
        self.set_data(DataSlot::LgMemMin, LG_MEM_LEN);
        self.set_data(DataSlot::LgMemTop, 0);
        self.sync_lg_slots();
        self.set_data(DataSlot::SoundOn, 0);
        self.set_data(DataSlot::ShakeOn, 0);
        self.set_data(DataSlot::RamFile, 0);
        self.set_data(DataSlot::RamFileLen, 0);
        self.set_data(DataSlot::SmsReturnFlag, 0);
        self.set_data(DataSlot::SmsReturnVal, 0);
        self.set_data(DataSlot::ExitCb, 0);
        self.set_data(DataSlot::ExitCbData, 0);
        let pack = self.config.pack_filename.clone();
        self.store_guest_string(DataSlot::PackFilename, &pack);
        self.store_guest_string(DataSlot::StartFilename, "start.mr");
        self.store_guest_string(DataSlot::OldPackFilename, "");
        self.store_guest_string(DataSlot::OldStartFilename, "");
        self.store_guest_string(DataSlot::Entry, "_dsm");
        self.store_guest_string(DataSlot::StartFileParameter, "");
        // Large-memory pool starts untouched.
        // The internal state table (slot 23): an array of 78 pointers,
        // mirroring the reference _mr_c_internal_table, in its own region
        // (the 128-byte data-slot stride would place other slot data inside
        // the 312-byte pointer array). The ext reads entries 0-6 for
        // VM/timer state and 7-11 for gz/SMS state, then 12-76 as function
        // pointers (SMS helpers, `mr_isMr`, and the Lua C API bridges).
        let table = INTERNAL_TABLE_BASE;
        let entry_count = 78u32;
        let state_base = table + entry_count * 4; // state cells after ptrs
                                                  // Zero the full table + state region (440 bytes).
        for i in 0..(entry_count + 32) {
            self.w32(table + i * 4, 0);
        }
        // Entries 0-6: pointers to state cells.
        // [0]=m0_files(NULL) [1]=vm_state(non-NULL magic) [2]=mr_state(1=Run)
        // [3]=bi(0) [4]=timer_p(NULL) [5]=timer_state(0) [6]=no_pause(0)
        let init_state: [(u32, u32); 7] = [
            (0, 0),             // m0_files: NULL pointer
            (1, VM_STATE_ADDR), // vm_state: the host Lua state magic
            (2, 1),             // mr_state: 1 = MR_STATE_RUN
            (3, 0),             // bi: 0
            (4, 0),             // timer_p: NULL
            (5, 0),             // timer_state: 0
            (6, 0),             // timer_run_without_pause: 0
        ];
        for (index, value) in init_state.iter() {
            let cell = state_base + index * 4;
            self.w32(table + index * 4, cell);
            self.w32(cell, *value);
        }
        // Entries 7-11: gz/SMS state cells (all zero).
        for index in 7..12u32 {
            let cell = state_base + index * 4;
            self.w32(table + index * 4, cell);
            self.w32(cell, 0);
        }
        // Entries 12-15: the SMS/isMr helper bridges, sitting just below
        // the Lua C API dispatch range.
        for index in 12..16u32 {
            self.w32(table + index * 4, VM_BRIDGE_BASE - 0x10 + (index - 12) * 4);
        }
        // Entries 16-76: the Lua C API bridge (`asm_mrp_*` in the
        // reference). Each gets its own interceptable address; the host
        // dispatch decodes the operation from the address.
        for index in 16..77u32 {
            self.w32(table + index * 4, VM_BRIDGE_BASE + (index - 16) * 4);
        }
        // Entry 77 stays NULL, mirroring the reference table.
        // Static strings for the Lua bridge: the full and short type names,
        // in `crate::luastate` type-code order (NONE first).
        const TYPE_NAMES: [&str; 9] = [
            "no value", "nil", "boolean", "userdata", "number", "string", "table", "function",
            "userdata",
        ];
        const SHORT_NAMES: [&str; 9] = [
            "no ", "nil", "boo", "uda", "num", "str", "tab", "fun", "uda",
        ];
        for (index, name) in TYPE_NAMES.iter().enumerate() {
            let base = (VM_STRINGS_BASE + index as u32 * 16) as usize;
            self.mem[base..base + name.len()].copy_from_slice(name.as_bytes());
            self.mem[base + name.len()] = 0;
        }
        for (index, name) in SHORT_NAMES.iter().enumerate() {
            let base = (VM_STRINGS_BASE + 9 * 16 + index as u32 * 16) as usize;
            self.mem[base..base + name.len()].copy_from_slice(name.as_bytes());
            self.mem[base + name.len()] = 0;
        }
        // Port table and SMS config buffer use self-referential scratch.
        for slot in [DataSlot::CPortTable, DataSlot::SmsCfgBuf] {
            let address = self.data_slot_address(slot);
            self.set_data(slot, address);
        }
    }

    /// Reserves a private DATA-area region for each data slot.
    fn data_slot_address(&self, slot: DataSlot) -> u32 {
        // Fixed per-variant offsets inside the DATA area; strings get 128
        // bytes, pointer/counter slots 4 bytes.
        use DataSlot::*;
        let index: u32 = match slot {
            ScreenWidth => 0,
            ScreenHeight => 1,
            ScreenBit => 2,
            ScreenBufPtr => 3,
            PackFilename => 4,
            StartFilename => 5,
            OldPackFilename => 6,
            OldStartFilename => 7,
            RamFile => 8,
            RamFileLen => 9,
            SoundOn => 10,
            ShakeOn => 11,
            LgMemBase => 12,
            LgMemLen => 13,
            LgMemEnd => 14,
            LgMemLeft => 15,
            LgMemMin => 16,
            LgMemTop => 17,
            LgMemFree => 18,
            BitmapTable => return BITMAP_TABLE,
            TileTable => return TILE_TABLE,
            MapTable => return MAP_TABLE,
            SoundTable => return SOUND_TABLE,
            SpriteTable => return SPRITE_TABLE,
            SmsCfgBuf => 20,
            CInternalTable => return INTERNAL_TABLE_BASE,
            CPortTable => 22,
            StartFileParameter => 23,
            SmsReturnFlag => 24,
            SmsReturnVal => 25,
            ExitCb => 26,
            ExitCbData => 27,
            Entry => 28,
        };
        DATA_BASE + index * 128
    }

    fn set_data(&mut self, slot: DataSlot, value: u32) {
        let address = self.data_slot_address(slot);
        self.w32(address, value);
    }

    /// Mirrors the allocator bookkeeping into the exported `LG_mem_*` data
    /// slots. The reference runtime's `mr_malloc` pool *is* the large-memory
    /// pool, and guests walk its free list through these slots, so they must
    /// track the live allocator state.
    fn sync_lg_slots(&mut self) {
        self.set_data(DataSlot::LgMemLeft, self.heap_left);
        self.set_data(DataSlot::LgMemMin, self.heap_min);
        self.set_data(DataSlot::LgMemTop, self.heap_top);
        // Slot 146 exports a pointer to the free-list head struct
        // (`{next, len}`), mirroring the reference `&LG_mem_free`.
        let cell = self.data_slot_address(DataSlot::LgMemFree);
        self.w32(cell, self.heap_head());
    }

    fn get_data(&self, slot: DataSlot) -> u32 {
        let address = self.data_slot_address(slot);
        self.r32(address)
    }

    /// Stores a NUL-terminated string into a string-typed data slot.
    fn store_guest_string(&mut self, slot: DataSlot, value: &str) {
        let address = self.data_slot_address(slot);
        let bytes = value.as_bytes();
        let len = bytes.len().min(126);
        let base = address as usize;
        self.mem[base..base + len].copy_from_slice(&bytes[..len]);
        self.mem[base + len] = 0;
    }

    // -- Guest memory access -------------------------------------------------

    pub fn r8(&self, addr: u32) -> u8 {
        *self.mem.get(addr as usize).unwrap_or(&0)
    }

    pub fn r16(&self, addr: u32) -> u16 {
        if addr as usize + 2 <= self.mem.len() {
            u16::from_le_bytes([self.r8(addr), self.r8(addr + 1)])
        } else {
            0
        }
    }

    pub fn r32(&self, addr: u32) -> u32 {
        if addr as usize + 4 <= self.mem.len() {
            u32::from_le_bytes([
                self.r8(addr),
                self.r8(addr + 1),
                self.r8(addr + 2),
                self.r8(addr + 3),
            ])
        } else {
            0
        }
    }

    pub fn w8(&mut self, addr: u32, value: u8) {
        if let Some(cell) = self.mem.get_mut(addr as usize) {
            *cell = value;
        }
    }

    pub fn w16(&mut self, addr: u32, value: u16) {
        self.w8(addr, value as u8);
        self.w8(addr + 1, (value >> 8) as u8);
    }

    pub fn w32(&mut self, addr: u32, value: u32) {
        self.w8(addr, value as u8);
        self.w8(addr + 1, (value >> 8) as u8);
        self.w8(addr + 2, (value >> 16) as u8);
        self.w8(addr + 3, (value >> 24) as u8);
    }

    /// Reads a NUL-terminated C string from guest memory.
    pub fn guest_cstring(&self, addr: u32, max: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let start = addr as usize;
        if start >= self.mem.len() {
            return out;
        }
        let end = self.mem.len().min(start.saturating_add(max));
        for &byte in &self.mem[start..end] {
            if byte == 0 {
                break;
            }
            out.push(byte);
        }
        out
    }

    // -- Heap (first-fit free list, reference layout) ------------------------

    fn heap_head(&self) -> u32 {
        self.heap_free_blocks
            .first()
            .map_or(self.heap_extent, |&(offset, _)| offset)
    }

    fn import_guest_heap(&mut self) -> bool {
        let head = self.get_data(DataSlot::LgMemFree);
        let left = self.get_data(DataSlot::LgMemLeft);
        let top = self.get_data(DataSlot::LgMemTop);
        let end = self.get_data(DataSlot::LgMemEnd);
        if !(HEAP_BASE + HEAP_LEN..=BITMAP_TABLE).contains(&end) {
            return false;
        }
        let extent = end - HEAP_BASE;
        if extent == self.heap_extent
            && head == self.heap_head()
            && left == self.heap_left
            && top == self.heap_top
        {
            return true;
        }

        // Native loaders temporarily splice their output block into this pool.
        // Adopt only a structurally valid list after an exported state change.
        let mut blocks = Vec::new();
        let mut visited = HashSet::new();
        let mut offset = head;
        while offset != extent {
            if !offset.is_multiple_of(4)
                || offset > extent - HEAP_MIN_PIECE
                || !visited.insert(offset)
                || charge_instruction()
            {
                log::debug!("invalid guest heap link at offset {offset:#x} (head {head:#x})");
                return false;
            }
            let next = self.r32(HEAP_BASE + offset);
            let size = self.r32(HEAP_BASE + offset + 4);
            if next > extent
                || size < HEAP_MIN_PIECE
                || !size.is_multiple_of(8)
                || size > extent - offset
            {
                log::debug!("invalid guest heap node at {offset:#x}: size {size:#x}, next {next:#x} (head {head:#x})");
                return false;
            }
            blocks.push((offset, size));
            offset = next;
        }
        self.heap_free_blocks = blocks;
        self.heap_extent = extent;
        self.heap_left = left;
        self.heap_min = self.get_data(DataSlot::LgMemMin);
        self.heap_top = top;
        true
    }

    pub fn heap_malloc(&mut self, requested: u32) -> u32 {
        if requested == 0 || requested > HEAP_MAX_ALLOC || !self.import_guest_heap() {
            return 0;
        }
        let len = requested.max(HEAP_MIN_PIECE).div_ceil(8) * 8;
        if len >= self.heap_left {
            return 0;
        }
        let mut block = None;
        for (index, &(offset, size)) in self.heap_free_blocks.iter().enumerate() {
            if charge_instruction() {
                return 0;
            }
            if size >= len {
                block = Some((index, offset, size));
                break;
            }
        }
        let Some((index, offset, size)) = block else {
            return 0;
        };
        let cut = offset + len;
        let next = if size > len {
            self.heap_free_blocks[index] = (cut, size - len);
            self.write_heap_node(index);
            cut
        } else {
            self.heap_free_blocks.remove(index);
            self.heap_free_blocks
                .get(index)
                .map_or(self.heap_extent, |&(start, _)| start)
        };
        if index > 0 {
            self.write_heap_node(index - 1);
        }
        self.heap_left -= len;
        self.heap_min = self.heap_min.min(self.heap_left);
        self.heap_top = self.heap_top.max(next);
        self.sync_lg_slots();
        HEAP_BASE + offset
    }

    pub(crate) fn heap_free(&mut self, address: u32, requested: u32) {
        if requested == 0
            || requested > HEAP_LEN
            || !address.is_multiple_of(4)
            || !(HEAP_BASE..BITMAP_TABLE).contains(&address)
        {
            return;
        }
        let len = requested.max(HEAP_MIN_PIECE).div_ceil(8) * 8;
        if !self.import_guest_heap() || address + len > HEAP_BASE + self.heap_extent {
            return;
        }
        let offset = address - HEAP_BASE;
        if self
            .heap_free_blocks
            .iter()
            .any(|&(start, size)| start < offset + len && offset < start + size)
        {
            return;
        }
        let was_sorted = self.heap_free_blocks.is_sorted();
        if !was_sorted {
            self.heap_free_blocks.sort_unstable();
        }
        let mut index = self
            .heap_free_blocks
            .partition_point(|&(start, _)| start < offset);
        let mut size = len;
        if let Some(&(_, next_size)) = self
            .heap_free_blocks
            .get(index)
            .filter(|(start, _)| *start == offset + len)
        {
            self.heap_free_blocks.remove(index);
            size += next_size;
        }
        if index > 0 && {
            let (previous, previous_size) = self.heap_free_blocks[index - 1];
            previous + previous_size == offset
        } {
            index -= 1;
            self.heap_free_blocks[index].1 += size;
        } else {
            self.heap_free_blocks.insert(index, (offset, size));
        }
        if was_sorted {
            self.write_heap_node(index);
            if index > 0 {
                self.write_heap_node(index - 1);
            }
        } else {
            for index in 0..self.heap_free_blocks.len() {
                self.write_heap_node(index);
            }
        }
        self.heap_left = self.heap_left.saturating_add(len);
        self.sync_lg_slots();
    }

    fn write_heap_node(&mut self, index: usize) {
        // Publish the SDK free-list view without trusting stores to freed memory.
        let (offset, size) = self.heap_free_blocks[index];
        let next = self
            .heap_free_blocks
            .get(index + 1)
            .map_or(self.heap_extent, |&(start, _)| start);
        self.w32(HEAP_BASE + offset, next);
        self.w32(HEAP_BASE + offset + 4, size);
    }

    fn allocate_extended_ram(&mut self, size: u32) -> u32 {
        if size > EXTENDED_RAM_END - EXTENDED_RAM_BASE {
            return 0;
        }
        let size = size.next_multiple_of(8);
        let mut address = EXTENDED_RAM_BASE;
        for (&start, &allocated) in &self.extended_allocations {
            if size <= start - address {
                break;
            }
            address = start + allocated;
        }
        let end = address + size;
        if end > EXTENDED_RAM_END {
            return 0;
        }
        self.extended_allocations.insert(address, size);
        if end as usize > self.mem.len() {
            self.mem.resize(end as usize, 0);
        }
        address
    }

    // -- Guest execution -----------------------------------------------------

    /// Evaluates the ARM condition field (bits 31-28) against the current
    /// CPSR flags. Used for in-loop decodes that armv4t_emu never sees.
    fn arm_cond_passed(&self, cond: u32) -> bool {
        let cpsr = self.cpu.reg_get(self.cpu.mode(), reg::CPSR);
        let n = (cpsr >> 31) & 1 == 1;
        let z = (cpsr >> 30) & 1 == 1;
        let c = (cpsr >> 29) & 1 == 1;
        let v = (cpsr >> 28) & 1 == 1;
        match cond & 0xF {
            0x0 => z,            // EQ
            0x1 => !z,           // NE
            0x2 => c,            // CS
            0x3 => !c,           // CC
            0x4 => n,            // MI
            0x5 => !n,           // PL
            0x6 => v,            // VS
            0x7 => !v,           // VC
            0x8 => c && !z,      // HI
            0x9 => !c || z,      // LS
            0xA => n == v,       // GE
            0xB => n != v,       // LT
            0xC => !z && n == v, // GT
            0xD => z || n != v,  // LE
            0xE => true,         // AL
            _ => false,          // NV / unpredictable: never execute
        }
    }

    /// Executes one ARMv5TE DSP multiply instruction (`SMLAxy`, `SMULxy`,
    /// `SMLAWy`, `SMULWy`, or `SMLALxy`), which armv4t_emu never sees.
    ///
    /// Encoding: `cond 0001 op Rd Rn Rs 1 x y b Rm` where `x`/`y` pick the
    /// high (`T`) or low (`B`) halfword of `Rm`/`Rs` and `op` (bits23-20
    /// plus bit7..4 shape) selects the variant.
    fn dsp_multiply(&mut self, inst: u32, op: u32) {
        let mode = self.cpu.mode();
        let rd = (inst >> 16) & 0xF;
        let rn = (inst >> 12) & 0xF;
        let rs = (inst >> 8) & 0xF;
        let rm = inst & 0xF;
        let x = (inst >> 6) & 1; // Rm halfword: 0 = low (B), 1 = high (T)
        let y = (inst >> 5) & 1; // Rs halfword
        let half = |value: u32, high: u32| -> i32 {
            let bits = if high != 0 {
                (value >> 16) & 0xFFFF
            } else {
                value & 0xFFFF
            };
            (bits as u16) as i16 as i32
        };
        let a = half(self.cpu.reg_get(mode, rm as reg::Reg), x);
        let b = half(self.cpu.reg_get(mode, rs as reg::Reg), y);
        match op {
            // SMLAxy: Rd = Rm[x] * Rs[y] + Rn
            0x10 => {
                let result = a
                    .wrapping_mul(b)
                    .wrapping_add(self.cpu.reg_get(mode, rn as reg::Reg) as i32);
                self.cpu.reg_set(mode, rd as reg::Reg, result as u32);
            }
            // SMULxy: Rd = Rm[x] * Rs[y]
            0x16 => {
                self.cpu
                    .reg_set(mode, rd as reg::Reg, a.wrapping_mul(b) as u32);
            }
            // SMLAWy (bit4=0): Rd = (Rm * Rs[y]) top 32 bits + Rn.
            // SMULWy (bit4=1): the same product without the accumulate.
            0x12 => {
                let wide = (self.cpu.reg_get(mode, rm as reg::Reg) as i32) as i64;
                let product = wide * b as i64;
                let top = (product >> 16) as i32;
                let result = if inst & 0x10 != 0 {
                    top
                } else {
                    top.wrapping_add(self.cpu.reg_get(mode, rn as reg::Reg) as i32)
                };
                self.cpu.reg_set(mode, rd as reg::Reg, result as u32);
            }
            // SMLALxy: RdHi:RdLo += Rm[x] * Rs[y] (64-bit accumulate)
            0x14 => {
                let product = (a as i64).wrapping_mul(b as i64);
                let hi = self.cpu.reg_get(mode, rd as reg::Reg) as u64;
                let lo = self.cpu.reg_get(mode, rn as reg::Reg) as u64;
                let acc = ((hi << 32) | lo).wrapping_add(product as u64);
                self.cpu.reg_set(mode, rd as reg::Reg, (acc >> 32) as u32);
                self.cpu.reg_set(mode, rn as reg::Reg, acc as u32);
            }
            // SMULWy handled by 0x12 with bit4 set; unreachable here because
            // the caller masks bit4 = 0, but kept exhaustive for op values.
            _ => {}
        }
    }

    /// Enters a guest function pointer: bit0 carries the ARM/Thumb
    /// interworking state (the universal `bx` calling convention), so it is
    /// mirrored into CPSR.T and masked out of PC.
    fn enter_function(&mut self, entry: u32) {
        let cpsr = self.cpu.reg_get(self.cpu.mode(), reg::CPSR);
        let cpsr = if entry & 1 != 0 {
            cpsr | (1 << 5)
        } else {
            cpsr & !(1 << 5)
        };
        self.cpu.reg_set(self.cpu.mode(), reg::CPSR, cpsr);
        self.cpu.reg_set(self.cpu.mode(), reg::PC, entry & !1);
    }

    /// Runs guest code from `entry` until it returns through LR==0 or jumps
    /// back to zero, intercepting bridge calls. Returns the guest's r0.
    /// SP starts at `STACK_TOP`, LR at 0.
    pub fn run_guest(
        &mut self,
        entry: u32,
        limit: u64,
        screen: &mut crate::display::Screen,
    ) -> Result<u32, MrpError> {
        self.run_guest_with_sp(entry, STACK_TOP, limit, screen)
    }

    /// [`Self::run_guest`] with an explicit initial SP — used when the
    /// caller must place stack arguments for the guest (the `mr_c_function`
    /// trailing output pointers).
    pub fn run_guest_with_sp(
        &mut self,
        entry: u32,
        sp: u32,
        limit: u64,
        screen: &mut crate::display::Screen,
    ) -> Result<u32, MrpError> {
        self.enter_function(entry);
        self.cpu.reg_set(self.cpu.mode(), reg::SP, sp);
        self.cpu.reg_set(self.cpu.mode(), reg::LR, 0);
        let bridge_end = BRIDGE_BASE + (TABLE_SLOTS as u32) * 4;

        let trace = std::env::var("MRP_TRACE").is_ok();
        let guard_pcs: Vec<u32> = std::env::var("MRP_GUARD_PC")
            .map(|value| {
                value
                    .split(',')
                    .filter_map(|part| u32::from_str_radix(part.trim(), 16).ok())
                    .collect()
            })
            .unwrap_or_default();

        let mut ring: Vec<(u32, u32, u32)> = Vec::new();
        let mut prev_pc = self.cpu.reg_get(self.cpu.mode(), reg::PC);
        let mut last_bridge_slot = None;
        let mut last_clock_poll = None;
        let mut clock_polls = 0u32;
        let mut clock_poll_instructions = 0u64;
        for executed in 0..limit {
            if trace {
                let pc_now = self.cpu.reg_get(self.cpu.mode(), reg::PC);
                if executed > 0
                    && pc_now != prev_pc.wrapping_add(2)
                    && pc_now != prev_pc.wrapping_add(4)
                    && pc_now != prev_pc.wrapping_add(8)
                    && !(BRIDGE_BASE..BRIDGE_BASE + (TABLE_SLOTS as u32) * 4).contains(&pc_now)
                {
                    eprintln!(
                        "[branch {executed}] {prev_pc:#010x} -> {pc_now:#010x} lr={:010x}",
                        self.cpu.reg_get(self.cpu.mode(), reg::LR)
                    );
                }
                prev_pc = pc_now;
            }
            if trace {
                ring.push((
                    self.cpu.reg_get(self.cpu.mode(), reg::PC),
                    self.r32(self.cpu.reg_get(self.cpu.mode(), reg::PC)),
                    self.cpu.reg_get(self.cpu.mode(), 9),
                ));
                if ring.len() > 24 {
                    ring.remove(0);
                }
            }
            if executed != 0 && executed % 5_000_000 == 0 {
                let pc = self.cpu.reg_get(self.cpu.mode(), reg::PC);
                log::info!("guest still running: {executed} instructions, pc={pc:#010x}");
            }
            let pc = self.cpu.reg_get(self.cpu.mode(), reg::PC);
            if let Some((lo, hi)) = self.exec_watch {
                if pc >= lo && pc < hi && !self.exec_watch_fired {
                    self.exec_watch_fired = true;
                    eprintln!("[exec-watch] pc entered {lo:#010x}..{hi:#010x} at step {executed}");
                    eprintln!(
                        "  r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} r4={:#010x} r9={:#010x} lr={:#010x} sp={:#010x}",
                        self.cpu.reg_get(self.cpu.mode(), R0),
                        self.cpu.reg_get(self.cpu.mode(), R1),
                        self.cpu.reg_get(self.cpu.mode(), R2),
                        self.cpu.reg_get(self.cpu.mode(), R3),
                        self.cpu.reg_get(self.cpu.mode(), 4),
                        self.cpu.reg_get(self.cpu.mode(), 9),
                        self.cpu.reg_get(self.cpu.mode(), reg::LR),
                        self.cpu.reg_get(self.cpu.mode(), reg::SP),
                    );
                    for (index, (rpc, rinst, rr0)) in ring.iter().enumerate() {
                        eprintln!(
                            "[exec-watch ring {index}] pc={rpc:#010x} inst={rinst:#010x} r9={rr0:#010x}"
                        );
                    }
                }
            }
            if charge_instruction() {
                return Err(MrpError::Unsupported(format!(
                    "instruction budget exceeded (stuck loop at pc {pc:#010x}?)"
                )));
            }
            if pc == 0 {
                if trace {
                    let r9 = self.cpu.reg_get(self.cpu.mode(), 9);
                    let cb44 = self.r32(r9.wrapping_add(44));
                    let cb40 = self.r32(r9.wrapping_add(40));
                    eprintln!(
                        "[ret] r9={r9:#x} rw+44={cb44:#x} rw+40={cb40:#x} r0={:#x}",
                        self.cpu.reg_get(self.cpu.mode(), R0)
                    );
                }
                if trace {
                    for (index, (rpc, rinst, rr0)) in ring.iter().enumerate() {
                        eprintln!("[return ring {index}] pc={rpc:#010x} inst={rinst:#010x} r0={rr0:#010x}");
                    }
                }
                return Ok(self.cpu.reg_get(self.cpu.mode(), R0));
            }
            if trace && pc == 0x0020_60b0 {
                eprintln!(
                    "[guard@60b0] r0={:#x} r1={:#x} cpsr={:#x} flags: n={} z={} c={}",
                    self.cpu.reg_get(self.cpu.mode(), R0),
                    self.cpu.reg_get(self.cpu.mode(), R1),
                    self.cpu.reg_get(self.cpu.mode(), reg::CPSR),
                    (self.cpu.reg_get(self.cpu.mode(), reg::CPSR) >> 31) & 1,
                    (self.cpu.reg_get(self.cpu.mode(), reg::CPSR) >> 30) & 1,
                    (self.cpu.reg_get(self.cpu.mode(), reg::CPSR) >> 29) & 1,
                );
            }
            // Generic probe guard (MRP_GUARD_PC=<hex>): dumps r0 and a
            // stack window at one pc. Boot-path diagnostics use this to
            // inspect a validation cell at the exact comparison site.
            if trace && guard_pcs.contains(&pc) {
                let sp = self.cpu.reg_get(self.cpu.mode(), reg::SP);
                let mut line = format!(
                    "[guard@{pc:08x}] r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} lr={:#010x} sp={sp:#010x} stack:",
                    self.cpu.reg_get(self.cpu.mode(), R0),
                    self.cpu.reg_get(self.cpu.mode(), R1),
                    self.cpu.reg_get(self.cpu.mode(), R2),
                    self.cpu.reg_get(self.cpu.mode(), R3),
                    self.cpu.reg_get(self.cpu.mode(), reg::LR),
                );
                for word in 0..24u32 {
                    line += &format!(" {:x}", self.r32(sp + word * 4));
                }
                eprintln!("{line}");
            }
            if pc == SEND_APP_EVENT_ADDR {
                // The chunk's sendAppEvent callback: (ext, chunk, code,
                // timer, param1-on-stack) — code 0 starts the platform timer
                // with the stack argument as the interval, code 1 stops it.
                let code = self.cpu.reg_get(self.cpu.mode(), R2) as i32;
                let interval = self.stack_arg(0) as i32;
                if trace {
                    eprintln!("[sendAppEvent] code={code} interval={interval}");
                }
                match code {
                    0 => {
                        let interval = interval.max(1) as u64;
                        self.timers.retain(|timer| timer.id != 0);
                        self.timers.push(GuestTimer {
                            id: 0,
                            interval: Duration::from_millis(interval),
                            next_fire: self.clock_now() + Duration::from_millis(interval),
                        });
                    }
                    1 => {
                        self.timers.retain(|timer| timer.id != 0);
                    }
                    _ => {}
                }
                let lr = self.cpu.reg_get(self.cpu.mode(), reg::LR);
                self.enter_function(lr);
                self.cpu.reg_set(self.cpu.mode(), R0, 0);
                continue;
            }
            if (VM_BRIDGE_BASE - 0x10..VM_BRIDGE_BASE + VM_BRIDGE_COUNT * 4).contains(&pc) {
                // Internal-table bridge: entries 12-15 (SMS/isMr helpers,
                // mapped just below the base) and 16-76 (the Lua C API).
                let slot = ((pc - (VM_BRIDGE_BASE - 0x10)) / 4) as usize + 12;
                let result = self.dispatch_vm_bridge(slot, screen);
                self.cpu.reg_set(self.cpu.mode(), R0, result as u32);
                // Return through LR with `bx lr` semantics, mirroring the
                // function-table bridge above.
                let lr = self.cpu.reg_get(self.cpu.mode(), reg::LR);
                self.enter_function(lr);
                continue;
            }
            if (BRIDGE_BASE..bridge_end).contains(&pc) {
                let slot = ((pc - BRIDGE_BASE) / 4) as usize;
                if slot >= TABLE_SLOTS || SLOTS[slot] != Slot::Func {
                    return Err(MrpError::Unsupported(format!(
                        "guest jumped into non-function table slot {slot}"
                    )));
                }
                self.slot_calls[slot] += 1;
                if slot == 33 && self.frame_time.is_some() {
                    // A guest waiting in a synchronous loop cannot receive a new frame.
                    if let Some(previous) = last_clock_poll {
                        if last_bridge_slot == Some(33) || clock_polls >= 32 {
                            clock_poll_instructions += executed - previous;
                            let milliseconds =
                                clock_poll_instructions / CLOCK_POLL_INSTRUCTIONS_PER_MS;
                            self.sleep_elapsed += Duration::from_millis(milliseconds);
                            clock_poll_instructions %= CLOCK_POLL_INSTRUCTIONS_PER_MS;
                        } else {
                            clock_poll_instructions = 0;
                        }
                    }
                    clock_polls += 1;
                    last_clock_poll = Some(executed);
                }
                last_bridge_slot = Some(slot);
                if trace {
                    eprintln!(
                        "[dispatch] slot={slot} r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} lr={:#010x}",
                        self.cpu.reg_get(self.cpu.mode(), R0),
                        self.cpu.reg_get(self.cpu.mode(), R1),
                        self.cpu.reg_get(self.cpu.mode(), R2),
                        self.cpu.reg_get(self.cpu.mode(), R3),
                        self.cpu.reg_get(self.cpu.mode(), reg::LR),
                    );
                }
                let result = self.dispatch(slot, screen);
                self.cpu.reg_set(self.cpu.mode(), R0, result as u32);
                // Return through LR with `bx lr` semantics: bit0 selects
                // Thumb, so a Thumb `bl`/`blx` into the bridge resumes in
                // Thumb after the host call.
                let lr = self.cpu.reg_get(self.cpu.mode(), reg::LR);
                self.enter_function(lr);
                continue;
            }
            if trace && executed < 80000 {
                eprintln!(
                    "[step {executed}] pc={pc:#010x} inst={:08x} r0={:#010x} r1={:#010x} r4={:#010x} r9={:#010x} lr={:#010x} sp={:#010x}",
                    self.r32(pc),
                    self.cpu.reg_get(self.cpu.mode(), R0),
                    self.cpu.reg_get(self.cpu.mode(), R1),
                    self.cpu.reg_get(self.cpu.mode(), 4),
                    self.cpu.reg_get(self.cpu.mode(), 9),
                    self.cpu.reg_get(self.cpu.mode(), reg::LR),
                    self.cpu.reg_get(self.cpu.mode(), reg::SP),
                );
            }
            // Thumb fetches are halfword aligned; ARM fetches word aligned.
            let thumb = self.cpu.thumb_mode();
            if pc as usize >= self.mem.len() || if thumb { (pc & 1) != 0 } else { (pc & 3) != 0 } {
                return Err(MrpError::Unsupported(format!(
                    "guest PC out of range: {pc:#010x}"
                )));
            }
            let inst = self.r32(pc);
            if trace && executed < 80000 {
                eprintln!(
                    "[trace {executed}] pc={pc:#010x} inst={inst:#010x} r0={:#010x} lr={:#010x} sp={:#010x} mode={:?}",
                    self.cpu.reg_get(self.cpu.mode(), R0),
                    self.cpu.reg_get(self.cpu.mode(), reg::LR),
                    self.cpu.reg_get(self.cpu.mode(), reg::SP),
                    self.cpu.mode(),
                );
            }
            // ARMv5 `BLX register` (x12FFF3X): the ext images target the
            // ARM9 (ARMv5TE) phones, and armv4t_emu stops at ARMv4, so the
            // loop decodes this one itself before handing the rest of the
            // instruction set to the CPU core. The top nibble carries the
            // ARM condition: the runtime guards optional hooks with
            // `cmp r0, #0; blxne r0`, so a failed condition must skip the
            // call instead of branching through a null pointer.
            if !thumb && (inst & 0x0FFF_FFF0) == 0x012F_FF30 {
                if self.arm_cond_passed(inst >> 28) {
                    let target = self.cpu.reg_get(self.cpu.mode(), (inst & 0xF) as reg::Reg);
                    self.cpu
                        .reg_set(self.cpu.mode(), reg::LR, pc.wrapping_add(4));
                    self.cpu.reg_set(self.cpu.mode(), reg::PC, target & !1);
                    let cpsr = self.cpu.reg_get(self.cpu.mode(), reg::CPSR);
                    let cpsr = if target & 1 != 0 {
                        cpsr | (1 << 5)
                    } else {
                        cpsr & !(1 << 5)
                    };
                    self.cpu.reg_set(self.cpu.mode(), reg::CPSR, cpsr);
                } else {
                    // Failed condition: the BLX is a no-op; advance past it.
                    self.cpu
                        .reg_set(self.cpu.mode(), reg::PC, pc.wrapping_add(4));
                }
                continue;
            }
            // ARMv5TE `LDRD`/`STRD` (double-word load/store): armv4t_emu's
            // halfword path debug-asserts on the encoding, which aborts the
            // emulator in debug builds. L=1 belongs to signed loads.
            if !thumb && (inst & 0x0E10_00D0) == 0x0000_00D0 {
                if self.arm_cond_passed(inst >> 28) {
                    let pre_index = inst & 0x0100_0000 != 0; // P
                    let up = inst & 0x0080_0000 != 0; // U
                    let immediate = inst & 0x0040_0000 != 0; // I
                    let writeback = inst & 0x0020_0000 != 0; // W
                    let load = inst & 0x20 == 0;
                    let rn = ((inst >> 16) & 0xF) as reg::Reg;
                    let rd = ((inst >> 12) & 0xF) as reg::Reg;
                    if rd & 1 != 0 || rd >= reg::LR {
                        return Err(MrpError::Unsupported(format!(
                            "invalid ARM doubleword register pair at {pc:#010x}"
                        )));
                    }
                    let offset = if immediate {
                        ((inst >> 4) & 0xF0) | (inst & 0xF)
                    } else {
                        self.cpu.reg_get(self.cpu.mode(), (inst & 0xF) as reg::Reg)
                    };
                    let base = if rn == reg::PC {
                        pc.wrapping_add(8)
                    } else {
                        self.cpu.reg_get(self.cpu.mode(), rn)
                    };
                    let target = if up {
                        base.wrapping_add(offset)
                    } else {
                        base.wrapping_sub(offset)
                    };
                    let addr = if pre_index { target } else { base };
                    let aligned = addr & !3;
                    let high_address = aligned.wrapping_add(4);
                    if load {
                        let low = self.r32(aligned);
                        let high = self.r32(high_address);
                        self.cpu.reg_set(self.cpu.mode(), rd, low);
                        self.cpu.reg_set(self.cpu.mode(), rd + 1, high);
                    } else {
                        let low = self.cpu.reg_get(self.cpu.mode(), rd);
                        let high = self.cpu.reg_get(self.cpu.mode(), rd + 1);
                        self.w32(aligned, low);
                        self.w32(high_address, high);
                    }
                    if (!pre_index || writeback) && rn != reg::PC {
                        self.cpu.reg_set(self.cpu.mode(), rn, target);
                    }
                }
                self.cpu
                    .reg_set(self.cpu.mode(), reg::PC, pc.wrapping_add(4));
                continue;
            }
            // ARMv5TE DSP multiply family (SMLAxy/SMULxy/SMLAWy/SMULWy/
            // SMLALxy): armv4t_emu has no ARMv5TE, and the ext runtime uses
            // `smlabb` for its index math (e.g. `col + row * width`), so the
            // loop decodes these itself. Encoding: bits27-20 select the
            // variant, bits7-4 = 1xy0/1xy1 pick the halfword operands.
            let dsp_op = (inst >> 20) & 0xFF;
            let dsp_bits74 = (inst >> 4) & 0xF;
            let dsp_match = match dsp_op {
                // SMLAxy / SMLALxy / SMULxy: bits7-4 = 1xy0.
                0x10 | 0x14 | 0x16 => (dsp_bits74 & 0b1001) == 0b1000,
                // SMLAWy (bit4=0) / SMULWy (bit4=1): bits7-4 = 1xyb.
                0x12 => (dsp_bits74 & 0b1000) == 0b1000,
                _ => false,
            };
            if !thumb && (inst & 0x0F00_0000) == 0x0100_0000 && dsp_match {
                if self.arm_cond_passed(inst >> 28) {
                    self.dsp_multiply(inst, dsp_op);
                }
                // Both paths resume at the following instruction.
                self.cpu
                    .reg_set(self.cpu.mode(), reg::PC, pc.wrapping_add(4));
                continue;
            }
            // ARMv5 `BLX <label>` (1111 101H imm24): the crate decodes the
            // encoding as a plain BL without switching state, so the loop
            // decodes the interworking branch itself. BLX immediate always
            // enters Thumb state; the H bit only contributes bit[1] of the
            // target address.
            if !thumb && (inst & 0xFE00_0000) == 0xFA00_0000 {
                let h = (inst >> 24) & 1;
                let imm24 = inst & 0x00FF_FFFF;
                let combined = ((imm24 << 1) | h) as i32;
                let offset = if combined & 0x0100_0000 != 0 {
                    combined - 0x0200_0000
                } else {
                    combined
                };
                self.cpu
                    .reg_set(self.cpu.mode(), reg::LR, pc.wrapping_add(4));
                let cpsr = self.cpu.reg_get(self.cpu.mode(), reg::CPSR);
                self.cpu
                    .reg_set(self.cpu.mode(), reg::CPSR, cpsr | (1 << 5));
                let target = pc.wrapping_add(8).wrapping_add((offset << 1) as u32);
                self.cpu.reg_set(self.cpu.mode(), reg::PC, target & !1);
                continue;
            }
            // Thumb decodes the crate gets wrong: `BL`/`BLX` long-branch
            // pairs never set the interworking bit in the link register
            // (so a later `bx lr` would return into ARM mode), the BLX
            // variant's second halfword is decoded as a plain conditional
            // branch, and `BLX register` is decoded as plain `BX` without
            // writing the link register at all.
            if thumb {
                let half = self.r16(pc);
                if (half & 0xF800) == 0xF000 {
                    // First halfword of a long-branch pair (11110 S
                    // imm10H). The second halfword selects the variant:
                    // 11111 … = BL (stay Thumb), 11101 … = BLX (switch to
                    // ARM, imm10L in bits 10-1 and the target bit1 in
                    // bit0). The link register takes the address after the
                    // pair with bit0 set — the return stays in Thumb.
                    let second = self.r16(pc.wrapping_add(2));
                    let offset_high = (((half & 0x7FF) as u32) << 12) as i32;
                    let offset_high = if offset_high & 0x0040_0000 != 0 {
                        offset_high - 0x0080_0000
                    } else {
                        offset_high
                    };
                    let base = pc.wrapping_add(4);
                    let raw = base
                        .wrapping_add(offset_high as u32)
                        .wrapping_add(((second & 0x7FF) << 1) as u32);
                    let target = if (second & 0xF800) == 0xF800 {
                        // BL: the target stays halfword aligned in Thumb.
                        raw
                    } else {
                        // BLX: the target is ARM code — word aligned.
                        raw & !3
                    };
                    self.cpu.reg_set(self.cpu.mode(), reg::LR, base | 1);
                    let cpsr = self.cpu.reg_get(self.cpu.mode(), reg::CPSR);
                    let cpsr = if (second & 0xF800) == 0xF800 {
                        cpsr | (1 << 5)
                    } else {
                        cpsr & !(1 << 5)
                    };
                    self.cpu.reg_set(self.cpu.mode(), reg::CPSR, cpsr);
                    self.cpu.reg_set(self.cpu.mode(), reg::PC, target & !1);
                    continue;
                }
                if (half & 0xFF00) == 0xBD00 {
                    // Thumb POP with PC (1011 1101 rlist): ARMv5T makes the
                    // loaded bit0 select the return state; the crate keeps
                    // the CPU in Thumb, which corrupts interworking returns.
                    let rlist = half & 0xFF;
                    let count = rlist.count_ones() + 1;
                    let sp = self.cpu.reg_get(self.cpu.mode(), reg::SP);
                    let mut addr = sp;
                    for reg_index in 0..8u32 {
                        if rlist & (1 << reg_index) != 0 {
                            let value = self.r32(addr);
                            self.cpu
                                .reg_set(self.cpu.mode(), reg_index as reg::Reg, value);
                            addr += 4;
                        }
                    }
                    let pc_value = self.r32(addr);
                    self.cpu.reg_set(self.cpu.mode(), reg::SP, sp + count * 4);
                    let cpsr = self.cpu.reg_get(self.cpu.mode(), reg::CPSR);
                    let cpsr = if pc_value & 1 != 0 {
                        cpsr | (1 << 5)
                    } else {
                        cpsr & !(1 << 5)
                    };
                    self.cpu.reg_set(self.cpu.mode(), reg::CPSR, cpsr);
                    self.cpu.reg_set(self.cpu.mode(), reg::PC, pc_value & !1);
                    continue;
                }
                if (half & 0xFF80) == 0x4780 {
                    // Thumb BLX register (0100 0111 1 Rm 000): switch state
                    // from Rm[0] and write the link register with bit0 set.
                    let rm = ((half >> 3) & 0xF) as reg::Reg;
                    let target = self.cpu.reg_get(self.cpu.mode(), rm);
                    self.cpu
                        .reg_set(self.cpu.mode(), reg::LR, pc.wrapping_add(2) | 1);
                    let cpsr = self.cpu.reg_get(self.cpu.mode(), reg::CPSR);
                    let cpsr = if target & 1 != 0 {
                        cpsr | (1 << 5)
                    } else {
                        cpsr & !(1 << 5)
                    };
                    self.cpu.reg_set(self.cpu.mode(), reg::CPSR, cpsr);
                    self.cpu.reg_set(self.cpu.mode(), reg::PC, target & !1);
                    continue;
                }
            }
            // A guest-triggered debug_assert inside armv4t_emu (e.g. a
            // misidentified ARMv5TE encoding reaching the ARMv4 decoder)
            // must not abort the emulator.
            let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.cpu
                    .step(&mut GuestMem(&mut self.mem, &mut self.value_watch))
            }));
            if step.is_err() {
                return Err(MrpError::Unsupported(format!(
                    "guest step panicked at pc {pc:#010x} inst {:#010x}",
                    self.r32(pc)
                )));
            }
            // Value-watch hits: the guest wrote a pointer inside the
            // watched range while running at `pc`.
            if (self.value_watch.lo != 0 || self.value_watch.hi != 0)
                && !self.value_watch.hits.is_empty()
            {
                let hits = std::mem::take(&mut self.value_watch.hits);
                for (addr, val) in hits {
                    eprintln!("[watch] pc={pc:#010x} wrote {val:#010x} -> {addr:#010x}");
                }
            }
            // ARMv5 PC-write interworking: LDM/LDR/MOV-to-PC and the second
            // BL halfword carry the new Thumb state in bit0 of the written
            // PC. The crate keeps the bit in the register instead of
            // updating CPSR, so mirror it here and mask it out before the
            // next fetch.
            let pc_after = self.cpu.reg_get(self.cpu.mode(), reg::PC);
            if pc_after & 1 != 0 {
                let cpsr = self.cpu.reg_get(self.cpu.mode(), reg::CPSR);
                let cpsr = if self.cpu.thumb_mode() {
                    cpsr
                } else {
                    cpsr | (1 << 5)
                };
                self.cpu.reg_set(self.cpu.mode(), reg::CPSR, cpsr);
                self.cpu.reg_set(self.cpu.mode(), reg::PC, pc_after & !1);
            }
        }
        let hot: Vec<String> = self
            .slot_calls
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(slot, count)| format!("{slot}:{count}"))
            .collect();
        let pc = self.cpu.reg_get(self.cpu.mode(), reg::PC);
        let instruction_set = if self.cpu.thumb_mode() {
            "Thumb"
        } else {
            "ARM"
        };
        Err(MrpError::Unsupported(format!(
            "guest instruction limit exceeded after {limit} at {instruction_set} pc {pc:#010x}; hot slots [{}]",
            hot.join(", ")
        )))
    }

    /// Calls the ext's registered event handler:
    /// `(P, code, input, input_len, output*, output_len*)`.
    pub fn call_c_function(
        &mut self,
        code: i32,
        input: u32,
        input_len: i32,
        limit: u64,
        screen: &mut crate::display::Screen,
    ) -> Result<i32, MrpError> {
        let (ret, _output) = self.call_c_function_full(code, input, input_len, limit, screen)?;
        Ok(ret)
    }

    /// Like [`Self::call_c_function`], additionally returning the
    /// `(output_ptr, output_len)` the handler wrote into the trailing
    /// stack slots (the `mr_c_function` out-parameters).
    pub fn call_c_function_full(
        &mut self,
        code: i32,
        input: u32,
        input_len: i32,
        limit: u64,
        screen: &mut crate::display::Screen,
    ) -> Result<(i32, (u32, u32)), MrpError> {
        if self.c_function == 0 {
            return Err(MrpError::Unsupported(
                "ext has not registered its event handler".to_string(),
            ));
        }
        // The SDK runtime copies a dispatch shell (its helper entry stub,
        // holding the real-handler pointer cell) to `SP - 0x4C` of the call
        // that runs the init — observed at HANDLER_SP - 0x4C for several
        // ext versions. The dispatched callbacks' deeper frames write
        // stack addresses over that shell, so the second call through it
        // reads a garbage target. Capture the shell region after the boot
        // handshake and restore it before every later call.
        const HANDLER_SP: u32 = STACK_TOP - 0x200;
        const SHELL_SPAN: usize = 0x400;
        let shell_start = (HANDLER_SP - SHELL_SPAN as u32) as usize;
        if let Some(snapshot) = &self.stub_snapshot {
            let base = (STACK_TOP - SHELL_SPAN as u32) as usize;
            self.mem[base..base + SHELL_SPAN].copy_from_slice(snapshot);
        }
        // Keep the out-parameters and their storage above the callee's frame.
        let scratch1 = HANDLER_SP + 8;
        let scratch2 = HANDLER_SP + 12;
        self.w32(HANDLER_SP, scratch1);
        self.w32(HANDLER_SP + 4, scratch2);
        self.w32(scratch1, 0);
        self.w32(scratch2, 0);
        self.cpu.reg_set(self.cpu.mode(), R0, self.c_function_p);
        // r9 (static base) must hold the ext's RW globals base — the value
        // at P+0 (start_of_ER_RW), which the ext itself filled during boot
        // via its wrapped mrc_malloc. Compiled ext code addresses globals
        // as r9+offset (fixR9 convention); r9 = P would read garbage.
        let rw_base = self.r32(self.c_function_p);
        self.cpu.reg_set(self.cpu.mode(), 9, rw_base);
        self.cpu.reg_set(self.cpu.mode(), R1, code as u32);
        self.cpu.reg_set(self.cpu.mode(), R2, input);
        self.cpu.reg_set(self.cpu.mode(), R3, input_len as u32);
        let result = self.run_guest_with_sp(self.c_function, HANDLER_SP, limit, screen);
        self.stub_snapshot = Some(self.mem[shell_start..shell_start + SHELL_SPAN].to_vec());
        let output_ptr = self.r32(scratch1);
        let output_len = self.r32(scratch2);
        let result = result?;
        Ok((result as i32, (output_ptr, output_len)))
    }

    // -- mrc_ host implementations (the bridge) ------------------------------

    /// Formats a guest printf-style call: varargs come from r2..r3 and then
    /// the stack; `%s` strings are read from guest memory on demand.
    fn guest_format(&self, format_addr: u32, first_arg: u32) -> Vec<u8> {
        let format = self.guest_cstring(format_addr, 4096);
        let sp = self.cpu.reg_get(self.cpu.mode(), reg::SP);
        let mut stack_words = Vec::new();
        for index in 0..16u32 {
            stack_words.push(self.r32(sp + index * 4));
        }
        let mut register_args: Vec<u32> = Vec::new();
        let registers = [
            self.cpu.reg_get(self.cpu.mode(), R0),
            self.cpu.reg_get(self.cpu.mode(), R1),
            self.cpu.reg_get(self.cpu.mode(), R2),
            self.cpu.reg_get(self.cpu.mode(), R3),
        ];
        register_args.extend_from_slice(&registers[first_arg.min(4) as usize..]);
        let mut all_args = register_args;
        all_args.extend_from_slice(&stack_words);

        fmt::format(&format, |index, conversion| {
            let raw = all_args.get(index).copied().unwrap_or(0);
            match conversion {
                b's' => Arg::Str(self.guest_cstring(raw, 4096)),
                _ => Arg::Num(raw),
            }
        })
    }

    /// Reads stack-passed arguments: word 0 is the first argument after r3.
    fn stack_arg(&self, index: u32) -> u32 {
        let sp = self.cpu.reg_get(self.cpu.mode(), reg::SP);
        self.r32(sp + index * 4)
    }

    fn dispatch(&mut self, slot: usize, screen: &mut crate::display::Screen) -> i32 {
        const MR_SUCCESS: i32 = 0;
        const MR_FAILED: i32 = -1;
        const MR_IGNORE: i32 = 1;

        match slot {
            0 => self.heap_malloc(self.cpu.reg_get(self.cpu.mode(), R0)) as i32,
            1 => {
                let addr = self.cpu.reg_get(self.cpu.mode(), R0);
                let len = self.cpu.reg_get(self.cpu.mode(), R1);
                self.heap_free(addr, len);
                MR_SUCCESS
            }
            2 => {
                let old = self.cpu.reg_get(self.cpu.mode(), R0);
                let old_len = self.cpu.reg_get(self.cpu.mode(), R1);
                let len = self.cpu.reg_get(self.cpu.mode(), R2);
                if len == 0 {
                    self.heap_free(old, old_len);
                    return 0;
                }
                let count = len.min(old_len) as usize;
                let src = old as usize;
                if old != 0 && src.saturating_add(count) > self.mem.len() {
                    return 0;
                }
                let new = self.heap_malloc(len);
                if new != 0 && old != 0 {
                    self.mem.copy_within(src..src + count, new as usize);
                    self.heap_free(old, old_len);
                }
                new as i32
            }
            3..=19 | 27 | 28 => self.dispatch_mem_string(slot),
            25 => {
                // mr_c_function_new(func, len)
                let func = self.cpu.reg_get(self.cpu.mode(), R0);
                let len = self.cpu.reg_get(self.cpu.mode(), R1).max(4);
                let block = self.heap_malloc(len);
                if block == 0 {
                    return MR_FAILED;
                }
                let start = block as usize;
                self.mem[start..start + len as usize].fill(0);
                self.c_function_p = block;
                self.c_function = func;
                // Publish P where the ext expects it: ext base + 4.
                self.w32(self.ext_load_base + 4, block);
                MR_SUCCESS
            }
            26 => {
                // mr_printf(fmt, ...): the blank-class exts spam this from
                // their event loop (407k calls per boot), consuming the
                // instruction budget before the game rendering can start.
                // Skip the format parsing unless MRP_TRACE is active.
                if std::env::var("MRP_TRACE").is_ok() {
                    let message = self.guest_format(self.cpu.reg_get(self.cpu.mode(), R0), 1);
                    log::info!("mr_printf: {}", String::from_utf8_lossy(&message));
                }
                MR_SUCCESS
            }
            29 => {
                // The source is a full framebuffer. The rectangle describes
                // the damaged area and does not alter the framebuffer.
                let src = match self.cpu.reg_get(self.cpu.mode(), R0) {
                    0 => self.framebuffer_address().unwrap_or(0),
                    address => address,
                };
                let x = i32::from(self.cpu.reg_get(self.cpu.mode(), R1) as i16);
                let y = i32::from(self.cpu.reg_get(self.cpu.mode(), R2) as i16);
                let w = i32::from(self.cpu.reg_get(self.cpu.mode(), R3) as u16);
                let h = i32::from(self.stack_arg(0) as u16);
                let width = i32::from(self.config.screen_width);
                let height = i32::from(self.config.screen_height);
                if src != 0
                    && w > 0
                    && h > 0
                    && x < width
                    && y < height
                    && x + w > 0
                    && y + h > 0
                    && usize::try_from(src).is_ok_and(|start| {
                        start + width as usize * height as usize * 2 <= self.mem.len()
                    })
                {
                    self.uses_explicit_video = true;
                    self.copy_screen(src, screen);
                }
                MR_SUCCESS
            }
            31 => {
                // mr_timerStart(time_ms): the SDK runtime schedules its own
                // timer chain and hands the platform the next timeout; r1 is
                // leftover register content, not an argument. The internal
                // `mr_timer_state` cell goes to RUNNING.
                let interval = self.cpu.reg_get(self.cpu.mode(), R0).max(1);
                self.timers.retain(|timer| timer.id != 0);
                self.timers.push(GuestTimer {
                    id: 0,
                    interval: Duration::from_millis(u64::from(interval)),
                    next_fire: self.clock_now() + Duration::from_millis(u64::from(interval)),
                });
                let cell = self.internal_state_cell(5);
                self.w32(cell, 1);
                MR_SUCCESS
            }
            32 => {
                // mr_timerStop(): stops the single platform timer.
                self.timers.retain(|timer| timer.id != 0);
                let cell = self.internal_state_cell(5);
                self.w32(cell, 0);
                MR_SUCCESS
            }
            33 => self.uptime_ms() as i32,
            34 => {
                // The SDK structure stores a 16-bit year and five bytes.
                let buffer = self.cpu.reg_get(self.cpu.mode(), R0);
                let (year, month, day, hour, minute, second) = local_datetime();
                let start = buffer as usize;
                let Some(dest) = self.mem.get_mut(start..start.saturating_add(7)) else {
                    return MR_FAILED;
                };
                dest[..2].copy_from_slice(&(year as u16).to_le_bytes());
                dest[2..].copy_from_slice(&[
                    month as u8,
                    day as u8,
                    hour as u8,
                    minute as u8,
                    second as u8,
                ]);
                MR_SUCCESS
            }
            35 => {
                let buffer = self.cpu.reg_get(self.cpu.mode(), R0);
                let start = buffer as usize;
                let Some(dest) = self.mem.get_mut(start..start.saturating_add(64)) else {
                    return MR_FAILED;
                };
                dest.fill(0);
                for (offset, capacity, value) in [
                    (0, 16, &self.config.imei),
                    (16, 16, &self.config.imsi),
                    (32, 8, &self.config.vendor),
                    (40, 8, &self.config.device),
                ] {
                    let len = value.len().min(capacity - 1);
                    dest[offset..offset + len].copy_from_slice(&value.as_bytes()[..len]);
                }
                MR_SUCCESS
            }
            36 => {
                let delay = self.cpu.reg_get(self.cpu.mode(), R0);
                self.sleep_elapsed += Duration::from_millis(u64::from(delay));
                MR_SUCCESS
            }
            37 => {
                let mode = self.cpu.mode();
                let command = self.cpu.reg_get(mode, R0);
                let parameter = self.cpu.reg_get(mode, R1);
                match command {
                    1011 if parameter <= 1 => MR_SUCCESS, // Incoming SMS notifications.
                    1101 if matches!(parameter, 2..=4) => 1000, // MR_PLAT_VALUE_BASE + 0 messages.
                    1011 | 1101 => MR_FAILED,
                    1106 => 2,    // MR_WAITING: asynchronous SMS service-centre query.
                    1205 => 1000, // MR_NORMAL_SCREEN
                    1206 => 1000, // MR_CHINESE
                    1218 => 1001, // MR_MSDC_OK
                    _ => {
                        log::debug!("mr_plat command {command} not implemented");
                        MR_IGNORE
                    }
                }
            }
            38 => {
                let mode = self.cpu.mode();
                let command = self.cpu.reg_get(mode, R0);
                match command {
                    1001 => {
                        let output = self.cpu.reg_get(mode, R3);
                        let length = self.stack_arg(0);
                        let Some(screen) = self.framebuffer_address() else {
                            return MR_FAILED;
                        };
                        if output != 0 {
                            self.w32(output, screen);
                        }
                        if length != 0 {
                            self.w32(
                                length,
                                u32::from(self.config.screen_width)
                                    * u32::from(self.config.screen_height)
                                    * 4,
                            );
                        }
                        MR_SUCCESS
                    }
                    1002 => MR_SUCCESS, // The display buffer remains owned by the platform.
                    1014 => {
                        let output = self.cpu.reg_get(mode, R3);
                        let length = self.stack_arg(0);
                        if output == 0 || length == 0 {
                            return MR_FAILED;
                        }
                        let requested = self.cpu.reg_get(mode, R2) as i32;
                        let size = if requested > 0 {
                            requested as u32
                        } else {
                            u32::from(self.config.screen_width)
                                * u32::from(self.config.screen_height)
                                * 4
                        };
                        let address = self.allocate_extended_ram(size);
                        self.w32(output, address);
                        if address == 0 {
                            self.w32(length, 0);
                            return MR_FAILED;
                        }
                        self.w32(length, size);
                        MR_SUCCESS
                    }
                    1015 => {
                        let address = self.cpu.reg_get(mode, R1);
                        if self.extended_allocations.remove(&address).is_some() {
                            MR_SUCCESS
                        } else {
                            MR_FAILED
                        }
                    }
                    1201 => {
                        let output = self.cpu.reg_get(mode, R3);
                        let length = self.stack_arg(0);
                        if output == 0 || length == 0 {
                            return MR_FAILED;
                        }
                        self.w32(FONT_METRICS, u32::from_le_bytes([16, 16, 8, 16]));
                        self.w32(output, FONT_METRICS);
                        self.w32(length, 4);
                        MR_SUCCESS
                    }
                    1204 => self.swi_storage_root(),
                    1222 | 1223 => MR_SUCCESS, // The virtual LCD remains readable.
                    _ => {
                        log::debug!("mr_platEx command {command} not implemented");
                        MR_IGNORE
                    }
                }
            }
            39 => 0, // mr_ferrno
            40 => self.swi_open(),
            41 => self.swi_close(),
            42 => self.swi_info(),
            43 => self.swi_write(),
            44 => self.swi_read(),
            45 => self.swi_seek(),
            46 => self.swi_get_len(),
            47 => {
                let name = self.guest_file_path();
                let Some(index) = self.find_file(&name, false) else {
                    return MR_FAILED;
                };
                // Preserve the backing object for handles already open.
                self.files[index].0.clear();
                self.dirty_files.remove(&name);
                MR_SUCCESS
            }
            48 => MR_FAILED, // mr_rename
            49 => {
                let name = self.guest_file_path();
                if name.is_empty() || self.find_file(&name, false).is_some() {
                    return MR_FAILED;
                }
                self.directories.insert(name);
                MR_SUCCESS
            }
            50 => MR_FAILED, // mr_rmDir
            51 => MR_FAILED, // mr_findStart
            52 => MR_FAILED, // mr_findGetNext
            53 => MR_FAILED, // mr_findStop
            54 => {
                self.exit_requested = true;
                log::info!("mr_exit called by the guest");
                MR_SUCCESS
            }
            55 | 56 => MR_SUCCESS, // shake
            57 => {
                // mr_playSound(type, data, len, loop)
                let sound_type = self.cpu.reg_get(self.cpu.mode(), R0);
                let data = self.cpu.reg_get(self.cpu.mode(), R1);
                let len = self.cpu.reg_get(self.cpu.mode(), R2);
                let looped = self.cpu.reg_get(self.cpu.mode(), R3) != 0;
                let bytes = self.guest_bytes(data, len.min(4 * 1024 * 1024) as usize);
                match sound_type {
                    0 => match bytes {
                        Some(bytes) => self.audio_sink.play_midi(&bytes, looped),
                        None => warn_once(
                            "audio-midi",
                            "mr_playSound: invalid MIDI buffer".to_string(),
                        ),
                    },
                    1 => match bytes.and_then(|bytes| crate::audio::decode_wav(&bytes).ok()) {
                        Some(clip) => {
                            self.audio_sink.play_bgm(clip, looped);
                        }
                        None => {
                            warn_once("audio-wav", "mr_playSound: WAV decode failed".to_string());
                        }
                    },
                    2 => match bytes {
                        Some(bytes) => match crate::audio::decode_mp3(bytes) {
                            Ok(clip) => {
                                log::debug!("playing MP3 ({len} bytes, loop={looped})");
                                self.audio_sink.play_bgm(clip, looped);
                            }
                            Err(error) => {
                                warn_once("audio-mp3", format!("mr_playSound: {error}"));
                            }
                        },
                        None => warn_once(
                            "audio-mp3-buffer",
                            "mr_playSound: invalid MP3 buffer".to_string(),
                        ),
                    },
                    5 => {
                        if let Some(bytes) = bytes {
                            match crate::audio::decode_amr(&bytes) {
                                Ok(clip) => {
                                    log::debug!("playing AMR-NB ({len} bytes, loop={looped})");
                                    self.audio_sink.play_bgm(clip, looped);
                                }
                                Err(error) => {
                                    warn_once("audio-amr", format!("mr_playSound: {error}"));
                                }
                            }
                        } else {
                            warn_once(
                                "audio-amr-buffer",
                                "mr_playSound: invalid AMR-NB buffer".to_string(),
                            );
                        }
                    }
                    other => {
                        warn_once(
                            "audio-unsupported",
                            format!("mr_playSound type {other} unsupported"),
                        );
                    }
                }
                MR_SUCCESS
            }
            58 => {
                self.audio_sink.stop_bgm();
                MR_SUCCESS
            }
            59 | 60 => MR_FAILED, // SMS/call: never dial out
            61 => 0,              // network id
            62 => MR_FAILED,      // connectWAP: offline
            // GUI natives: menu/dialog/text/edit/win. Each Create returns a
            // nonzero handle; Set/Show/Refresh return MR_SUCCESS; Release
            // return success. This lets ext code that builds
            // GUI-based UIs proceed past the creation gate.
            63 => {
                // mrc_menuCreate(title, num)
                let title = self.guest_cstring(self.cpu.reg_get(self.cpu.mode(), R0), 128);
                let count = self.cpu.reg_get(self.cpu.mode(), R1) as usize;
                self.next_gui_handle += 1;
                self.menu = Some(MenuState {
                    handle: self.next_gui_handle,
                    title,
                    items: vec![Vec::new(); count],
                    count,
                    selection: 0,
                });
                self.next_gui_handle as i32
            }
            64 => {
                // mrc_menuSetItem(menu, text, index)
                let text_addr = self.cpu.reg_get(self.cpu.mode(), R1);
                let index = self.cpu.reg_get(self.cpu.mode(), R2) as usize;
                let text = self.guest_cstring(text_addr, 128);
                if let Some(menu) = self.menu.as_mut() {
                    if index < menu.items.len() {
                        menu.items[index] = text;
                    }
                }
                MR_SUCCESS
            }
            65 => {
                // mrc_menuShow(menu): render the menu and take key focus.
                let menu = self.menu.get_or_insert_with(MenuState::default);
                menu.handle = self.cpu.reg_get(self.cpu.mode(), R0);
                menu.selection = 0;
                self.render_menu();
                MR_SUCCESS
            }
            67 => {
                // mrc_menuRelease: close the menu.
                self.menu = None;
                MR_SUCCESS
            }
            68 => {
                self.render_menu();
                MR_SUCCESS
            }

            69 => {
                self.next_gui_handle += 1;
                self.next_gui_handle as i32
            }
            70..=71 => MR_SUCCESS, // dialogRelease/Refresh
            72 => {
                self.next_gui_handle += 1;
                self.next_gui_handle as i32
            }
            73..=74 => MR_SUCCESS, // textRelease/Refresh
            75 => self.swi_edit_create(),
            76 => self.swi_edit_release(),
            77 => self.swi_edit_get_text(),
            78 => {
                self.next_gui_handle += 1;
                self.next_gui_handle as i32
            }
            79 => MR_SUCCESS, // winRelease
            // mr_drawRegion(gContext, src, sx, sy, w, h, transform, x, y,
            // anchor): the J2ME-graphics blit. gContext's fields: clipX(0)
            // clipY(4) clipXRight(16) clipYBottom(20) translateX(24)
            // translateY(28); the shadow buffer at +44 with its width at
            // +48. mr_jImageSt: data(0) width(4) height(6) trans(8)
            // transcolor(10). Ported from the reference mr_graphics.c.
            148 => {
                let g = self.cpu.reg_get(self.cpu.mode(), R0);
                let src = self.cpu.reg_get(self.cpu.mode(), R1);
                let sx = self.cpu.reg_get(self.cpu.mode(), R2) as i32;
                let sy = self.cpu.reg_get(self.cpu.mode(), R3) as i32;
                let w = self.stack_arg(0) as i32;
                let h = self.stack_arg(1) as i32;
                let transform = self.stack_arg(2) as i32;
                let mut x = self.stack_arg(3) as i32;
                let mut y = self.stack_arg(4) as i32;
                let anchor = self.stack_arg(5) as i32;
                if g == 0 || src == 0 || w <= 0 || h <= 0 {
                    return MR_SUCCESS;
                }
                let src_data = self.r32(src);
                let src_width = self.r16(src + 4) as i32;
                let src_height = self.r16(src + 6) as i32;
                let trans = self.r16(src + 8) as i32;
                let transcolor = self.r16(src + 10);
                let clip_x = self.r32(g) as i32;
                let clip_y = self.r32(g + 4) as i32;
                let clip_x_right = self.r32(g + 16) as i32;
                let clip_y_bottom = self.r32(g + 20) as i32;
                let translate_x = self.r32(g + 24) as i32;
                let translate_y = self.r32(g + 28) as i32;
                let dst_buf = self.r32(g + 44);
                let dst_width = self.r32(g + 48) as i32;
                // The destination dimensions swap for the 90-degree family.
                let (dw, dh) = if transform >> 2 != 0 { (h, w) } else { (w, h) };
                // Anchoring: 0 and 20 apply the translate; the rest use the
                // 9-point anchor constants (V: TOP=16 BOTTOM=32 BASELINE=64
                // VCENTER=2; H: LEFT=4 HCENTER=1 RIGHT=8).
                if anchor == 0 || anchor == 20 {
                    x += translate_x;
                    y += translate_y;
                } else {
                    let (aw, ah) = if transform >> 2 != 0 { (h, w) } else { (w, h) };
                    match anchor & 0x72 {
                        16 | 0 => {}
                        32 => y -= ah,         // BOTTOM
                        64 => y -= ah / 2 - 3, // BASELINE
                        2 => y -= ah / 2,      // VCENTER
                        _ => {}
                    }
                    match anchor & 0x0D {
                        4 | 0 => {}
                        8 => x -= aw,     // RIGHT
                        1 => x -= aw / 2, // HCENTER
                        _ => {}
                    }
                }
                let min_x = x.max(clip_x);
                let min_y = y.max(clip_y);
                let max_x = (x + dw).min(clip_x_right);
                let max_y = (y + dh).min(clip_y_bottom);
                if min_x >= max_x || min_y >= max_y {
                    return MR_SUCCESS;
                }
                for dy in min_y..max_y {
                    for dx in min_x..max_x {
                        let (i, j) = match transform & 7 {
                            0 => ((dx - x), (dy - y)),
                            1 => ((dx - x), (h - 1 - (dy - y))),
                            2 => ((w - 1 - (dx - x)), (dy - y)),
                            3 => ((w - 1 - (dx - x)), (h - 1 - (dy - y))),
                            4 => ((h - 1 - (dy - y)), (h - 1 - (dx - x))),
                            5 => ((dy - y), (h - 1 - (dx - x))),
                            6 => ((w - 1 - (dy - y)), (dx - x)),
                            _ => ((dy - y), (dx - x)),
                        };
                        let px = sx + i;
                        let py = sy + j;
                        if px < 0 || py < 0 || px >= src_width || py >= src_height {
                            continue;
                        }
                        let value = self.r16(src_data + (py * src_width + px) as u32 * 2);
                        if trans != 0 {
                            let key = if trans == 1 {
                                self.r16(src_data)
                            } else {
                                transcolor
                            };
                            if value == key {
                                continue;
                            }
                        }
                        self.w16(dst_buf + (dy * dst_width + dx) as u32 * 2, value);
                    }
                }
                MR_SUCCESS
            }
            80 => {
                // mr_getScreenInfo(struct)
                let buffer = self.cpu.reg_get(self.cpu.mode(), R0);
                self.w32(buffer, u32::from(self.config.screen_width));
                self.w32(buffer + 4, u32::from(self.config.screen_height));
                self.w32(buffer + 8, 16);
                MR_SUCCESS
            }
            81 => MR_SUCCESS, // Initialize the offline transport.
            82 => {
                self.offline_sockets = 0;
                MR_SUCCESS
            }
            84 => {
                let mode = self.cpu.mode();
                let kind = self.cpu.reg_get(mode, R0);
                let protocol = self.cpu.reg_get(mode, R1);
                let index = self.offline_sockets.trailing_ones();
                if kind > 1 || kind != protocol || index >= u16::BITS {
                    return MR_FAILED;
                }
                self.offline_sockets |= 1 << index;
                index as i32 + 1
            }
            86 => {
                let index = self.cpu.reg_get(self.cpu.mode(), R0).wrapping_sub(1);
                if index >= u16::BITS || self.offline_sockets & (1 << index) == 0 {
                    return MR_FAILED;
                }
                self.offline_sockets &= !(1 << index);
                MR_SUCCESS
            }
            83 | 85 | 87..=90 => {
                warn_once(
                    "net",
                    format!("mr_table slot {slot} (network) is offline; failing"),
                );
                MR_FAILED
            }
            113 => {
                let state = self.cpu.reg_get(self.cpu.mode(), R0);
                // MRP syscall 113 uses MD5 as a platform API digest, not for security.
                self.md5_contexts.insert(state, crate::md5::Md5::new()); // NOSONAR
                MR_SUCCESS
            }
            114 => {
                let state = self.cpu.reg_get(self.cpu.mode(), R0);
                let start = self.cpu.reg_get(self.cpu.mode(), R1) as usize;
                let len = self.cpu.reg_get(self.cpu.mode(), R2) as usize;
                let (Some(context), Some(data)) = (
                    self.md5_contexts.get_mut(&state),
                    self.mem.get(start..start.saturating_add(len)),
                ) else {
                    return MR_FAILED;
                };
                context.update(data);
                MR_SUCCESS
            }
            115 => {
                let state = self.cpu.reg_get(self.cpu.mode(), R0);
                let start = self.cpu.reg_get(self.cpu.mode(), R1) as usize;
                let (Some(context), Some(output)) = (
                    self.md5_contexts.remove(&state),
                    self.mem.get_mut(start..start.saturating_add(16)),
                ) else {
                    return MR_FAILED;
                };
                output.copy_from_slice(&context.finish());
                MR_SUCCESS
            }
            116 | 117 => MR_SUCCESS, // SMS config
            119 => {
                let x = i32::from(self.cpu.reg_get(self.cpu.mode(), R0) as i16);
                let y = i32::from(self.cpu.reg_get(self.cpu.mode(), R1) as i16);
                let color = self.cpu.reg_get(self.cpu.mode(), R2) as u16;
                self.screen_put(x, y, color);
                MR_SUCCESS
            }
            122 => {
                let x = i32::from(self.cpu.reg_get(self.cpu.mode(), R0) as i16);
                let y = i32::from(self.cpu.reg_get(self.cpu.mode(), R1) as i16);
                let w = i32::from(self.cpu.reg_get(self.cpu.mode(), R2) as i16);
                let h = i32::from(self.cpu.reg_get(self.cpu.mode(), R3) as i16);
                let r = self.stack_arg(0) & 0xFF;
                let g = self.stack_arg(1) & 0xFF;
                let b = self.stack_arg(2) & 0xFF;
                self.draw_rect(
                    x.max(0) as u32,
                    y.max(0) as u32,
                    (x + w).max(0) as u32,
                    (y + h).max(0) as u32,
                    r,
                    g,
                    b,
                );
                MR_SUCCESS
            }
            123 => {
                let text_addr = self.cpu.reg_get(self.cpu.mode(), R0);
                let x = i32::from(self.cpu.reg_get(self.cpu.mode(), R1) as i16);
                let y = i32::from(self.cpu.reg_get(self.cpu.mode(), R2) as i16);
                let r = self.cpu.reg_get(self.cpu.mode(), R3) & 0xff;
                let color = rgb888_to_565(r, self.stack_arg(0) & 0xff, self.stack_arg(1) & 0xff);
                let text = self.read_platform_text(text_addr, self.stack_arg(2) != 0);
                self.draw_decoded_text(&text, x, y, color);
                MR_SUCCESS
            }
            128 => self.draw_text_in_rect(),
            145 => {
                let ch = self.cpu.reg_get(self.cpu.mode(), R0) & 0xffff;
                let x = self.cpu.reg_get(self.cpu.mode(), R1) as i32;
                let y = self.cpu.reg_get(self.cpu.mode(), R2) as i32;
                let color = self.cpu.reg_get(self.cpu.mode(), R3) as u16;
                if let Some(glyph) = char::from_u32(ch).and_then(unifont::get_glyph) {
                    self.draw_glyph(glyph, x, y, color, i32::MAX, i32::MAX);
                }
                MR_SUCCESS
            }
            120 => {
                // mr_drawBitmap(p, x, y, w, h, rop, transcolor, sx, sy, mw):
                // the ten-argument sprite blit (rop modes, rotate/flip).
                let p = self.cpu.reg_get(self.cpu.mode(), R0);
                let x = i32::from(self.cpu.reg_get(self.cpu.mode(), R1) as i16);
                let y = i32::from(self.cpu.reg_get(self.cpu.mode(), R2) as i16);
                let w = u32::from(self.cpu.reg_get(self.cpu.mode(), R3) as u16);
                let h = u32::from(self.stack_arg(0) as u16);
                let rop = u32::from(self.stack_arg(1) as u16);
                let transcolor = self.stack_arg(2) as u16;
                let sx = i32::from(self.stack_arg(3) as i16);
                let sy = i32::from(self.stack_arg(4) as i16);
                let mw = u32::from(self.stack_arg(5) as u16);
                self.draw_bitmap_rop(p, x, y, w, h, rop, transcolor, sx, sy, mw);
                MR_SUCCESS
            }
            121 => {
                // mr_drawBitmapEx(srcbmp*, dstbmp*, w, h, trans*, transcolor):
                // affine 2x2-matrix blit between two bitmaps.
                let srcbmp = self.cpu.reg_get(self.cpu.mode(), R0);
                let dstbmp = self.cpu.reg_get(self.cpu.mode(), R1);
                let w = self.cpu.reg_get(self.cpu.mode(), R2);
                let h = self.cpu.reg_get(self.cpu.mode(), R3);
                let p_trans = self.stack_arg(0);
                let transcolor = self.stack_arg(1) as u16;
                self.draw_bitmap_ex(srcbmp, dstbmp, w, h, p_trans, transcolor);
                MR_SUCCESS
            }
            147 => {
                // mr_transbitmapDraw(hTransBmp, dstBuf, destMaxW, destMaxH,
                // sx, sy, width, height, dx, dy): RLE-matrix blit from a
                // MrTransBitmap built by the ext's own transbitmap helpers.
                let h_trans = self.cpu.reg_get(self.cpu.mode(), R0);
                let dst_buf = self.cpu.reg_get(self.cpu.mode(), R1);
                let dest_max_w = self.cpu.reg_get(self.cpu.mode(), R2) as i32;
                let dest_max_h = self.cpu.reg_get(self.cpu.mode(), R3) as i32;
                let sx = self.stack_arg(0) as i32;
                let sy = self.stack_arg(1) as i32;
                let width = self.stack_arg(2) as i32;
                let height = self.stack_arg(3) as i32;
                let dx = self.stack_arg(4) as i32;
                let dy = self.stack_arg(5) as i32;
                self.transbitmap_draw(
                    h_trans, dst_buf, dest_max_w, dest_max_h, sx, sy, width, height, dx, dy,
                );
                MR_SUCCESS
            }
            118 => {
                // _DispUpEx: the helper's mrc_refreshScreen presents the
                // screen buffer rect through this slot.
                self.uses_explicit_video = true;
                self.refresh_screen(screen);
                MR_SUCCESS
            }
            124 => 0, // mr_bitmapCheck
            125 => self.swi_read_file(),
            126 => {
                let text = self.cpu.reg_get(self.cpu.mode(), R0);
                self.guest_ucs2_bytes(text).len() as i32
            }
            127 => 0, // mr_registerAPP
            129 => 0, // mr_effSetCon
            130 => {
                // mr_testCom(state, input0, input1)
                let input0 = self.cpu.reg_get(self.cpu.mode(), R1);
                let input1 = self.cpu.reg_get(self.cpu.mode(), R2);
                if input0 == 800 {
                    self.load_nested_ext(input1, screen).unwrap_or(-1)
                } else {
                    self.dispatch_test_com(input0, input1)
                }
            }
            131 => {
                let command = self.cpu.reg_get(self.cpu.mode(), R1);
                let buffer = self.cpu.reg_get(self.cpu.mode(), R2);
                match command {
                    2 => {
                        let length = self.cpu.reg_get(self.cpu.mode(), R3);
                        if buffer as usize > self.mem.len()
                            || length as usize > self.mem.len() - buffer as usize
                        {
                            return MR_FAILED;
                        }
                        let previous = self.get_data(DataSlot::RamFile);
                        if previous != 0 && previous != buffer {
                            self.heap_free(previous, self.get_data(DataSlot::RamFileLen));
                        }
                        self.set_data(DataSlot::RamFile, buffer);
                        self.set_data(DataSlot::RamFileLen, length);
                        MR_SUCCESS
                    }
                    800 => self.load_nested_ext(buffer, screen).unwrap_or(MR_FAILED),
                    9 => MR_SUCCESS, // The interpreter reads guest instructions directly from RAM.
                    _ => {
                        log::debug!("mr_testCom1 command {command:#x} not implemented");
                        MR_IGNORE
                    }
                }
            }
            132 => self.swi_c2u(),
            133 => {
                let a = self.cpu.reg_get(self.cpu.mode(), R0) as i32;
                let b = self.cpu.reg_get(self.cpu.mode(), R1) as i32;
                if b == 0 {
                    0
                } else {
                    a.wrapping_div(b)
                }
            }
            134 => {
                let a = self.cpu.reg_get(self.cpu.mode(), R0) as i32;
                let b = self.cpu.reg_get(self.cpu.mode(), R1) as i32;
                if b == 0 {
                    0
                } else {
                    a.wrapping_rem(b)
                }
            }
            137 => 0, // mr_updCrc
            141 => self.swi_unzip().map_or(MR_FAILED, |()| MR_SUCCESS),
            // mr_getCharBitmap(ch, font, &width, &height) → glyph bitmap
            // pointer: 1 bit per pixel, rows aligned to 2 bytes, MSB first
            // (the MTK font convention the ext renderers expect).
            30 => {
                let ch = self.cpu.reg_get(self.cpu.mode(), R0);
                let width_ptr = self.cpu.reg_get(self.cpu.mode(), R2);
                let height_ptr = self.cpu.reg_get(self.cpu.mode(), R3);
                let glyph = char::from_u32(ch & 0xFFFF).and_then(unifont::get_glyph);
                let (width, rows) = glyph
                    .as_ref()
                    .map(|glyph| (glyph.get_width(), 16))
                    .unwrap_or((8, 16));
                let row_bytes = width.div_ceil(16) * 2;
                let buffer = self.glyph_buffer();
                for row in 0..rows {
                    for byte in 0..row_bytes {
                        let mut value = 0u8;
                        for bit in 0..8 {
                            let column = byte * 8 + bit;
                            let set = glyph
                                .as_ref()
                                .and_then(|glyph| glyph.get_pixel(column, row).then_some(()))
                                .is_some();
                            if set {
                                value |= 0x80 >> bit;
                            }
                        }
                        self.w8(buffer + (row * row_bytes + byte) as u32, value);
                    }
                }
                self.w32(width_ptr, width as u32);
                self.w32(height_ptr, rows as u32);
                buffer as i32
            }
            other => {
                warn_once(
                    "slot",
                    format!("mr_table slot {other} is not implemented; returning 0"),
                );
                0
            }
        }
    }

    /// Address of a cell in the internal state table
    /// (0=m0_files, 1=vm_state, 2=mr_state, 3=bi, 4=timer_p,
    /// 5=timer_state, 6=timer_run_without_pause).
    fn internal_state_cell(&self, index: u32) -> u32 {
        self.r32(INTERNAL_TABLE_BASE + index * 4)
    }

    pub(crate) fn update_flags(&mut self, set: u32, clear: u32) {
        let address = self.internal_state_cell(3);
        self.w32(address, (self.r32(address) | set) & !clear);
    }

    fn swi_unzip(&mut self) -> Option<()> {
        use std::io::Read;

        // The caller parses the gzip header and provides both buffers and
        // the deflate offset through the internal table.
        let input = self.r32(self.internal_state_cell(7)) as usize;
        let output = self.r32(self.internal_state_cell(8)) as usize;
        let input_offset = self.r32(self.internal_state_cell(9)) as usize;
        if input == 0 || output == 0 || output >= self.mem.len() {
            return None;
        }
        let input_start = input.checked_add(input_offset)?;
        let source = self.mem.get(input_start..)?;
        let mut decoder = flate2::bufread::DeflateDecoder::new(source);
        let mut bytes = Vec::new();
        let capacity = self.mem.len() - output;
        (&mut decoder)
            .take(capacity as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        let consumed = usize::try_from(decoder.total_in()).ok()?;
        let trailer = source.get(consumed..consumed.checked_add(8)?)?;
        let len = u32::from_le_bytes(trailer[4..8].try_into().ok()?) as usize;
        if bytes.len() != len || bytes.len() > capacity {
            return None;
        }
        self.mem[output..output + len].copy_from_slice(&bytes);
        self.w32(
            self.internal_state_cell(9),
            (input_offset + consumed + 8) as u32,
        );
        self.w32(self.internal_state_cell(10), len as u32);
        Some(())
    }

    /// `mr_mem_get`/`mr_mem_free` (slots 27/28) manage an auxiliary block
    /// pool that the reference implements over the same heap; route them to
    /// `mr_malloc`/`mr_free`.
    fn dispatch_mem_string(&mut self, slot: usize) -> i32 {
        const MR_SUCCESS: i32 = 0;
        let r0 = self.cpu.reg_get(self.cpu.mode(), R0);
        let r1 = self.cpu.reg_get(self.cpu.mode(), R1);
        let r2 = self.cpu.reg_get(self.cpu.mode(), R2);
        match slot {
            3 | 4 => {
                let count = r2 as usize;
                let (dst, src) = (r0 as usize, r1 as usize);
                if src.saturating_add(count) <= self.mem.len()
                    && dst.saturating_add(count) <= self.mem.len()
                {
                    self.mem.copy_within(src..src + count, dst);
                }
                r0 as i32
            }
            5 => {
                // strcpy(dst, src)
                let src = self.guest_cstring(r1, 4096);
                let dst = r0 as usize;
                if dst + src.len() < self.mem.len() {
                    self.mem[dst..dst + src.len()].copy_from_slice(&src);
                    self.mem[dst + src.len()] = 0;
                }
                r0 as i32
            }
            6 => {
                // strncpy(dst, src, n): pads with zeros like C.
                let dst = r0 as usize;
                let src = self.guest_cstring(r1, r2.min(4096) as usize);
                if dst + r2 as usize <= self.mem.len() {
                    let copied = src.len().min(r2 as usize);
                    self.mem[dst..dst + copied].copy_from_slice(&src[..copied]);
                    for cell in &mut self.mem[dst + copied..dst + r2 as usize] {
                        *cell = 0;
                    }
                }
                r0 as i32
            }
            7 | 8 => {
                // strcat/strncat(dst, src, [n])
                let dst = r0 as usize;
                let existing = self.guest_cstring(r0, 4096).len();
                let src = self.guest_cstring(r1, 4096);
                let count = if slot == 7 {
                    src.len()
                } else {
                    src.len().min(r2 as usize)
                };
                if dst + existing + count < self.mem.len() {
                    self.mem[dst + existing..dst + existing + count].copy_from_slice(&src[..count]);
                    self.mem[dst + existing + count] = 0;
                }
                r0 as i32
            }
            9..=12 => {
                // memcmp/strcmp/strncmp/strcoll
                let a = self.mem.get(r0 as usize..).unwrap_or_default();
                let b = self.mem.get(r1 as usize..).unwrap_or_default();
                let available = a.len().max(b.len());
                let limit = if matches!(slot, 9 | 11) {
                    (r2 as usize).min(available)
                } else {
                    available
                };
                for index in 0..limit {
                    let x = a.get(index).copied().unwrap_or(0);
                    let y = b.get(index).copied().unwrap_or(0);
                    if x != y {
                        return i32::from(x) - i32::from(y);
                    }
                    if slot != 9 && x == 0 {
                        break;
                    }
                }
                0
            }
            13 => {
                // memchr(src, ch, len)
                let ch = (r1 & 0xFF) as u8;
                let count = r2 as usize;
                for offset in 0..count.min(1 << 20) {
                    if self.r8(r0 + offset as u32) == ch {
                        return (r0 + offset as u32) as i32;
                    }
                }
                0
            }
            14 => {
                // memset(dst, ch, len). A NULL destination is a no-op: the
                // SDK runtime issues memset(0, 0, stack_size) as part of its
                // startup, which on the real device is a benign write to a
                // reserved page but here would wipe the function table,
                // bridge slots, and data slots.
                let count = r2 as usize;
                if r0 != 0 && r0 as usize + count <= self.mem.len() {
                    self.mem[r0 as usize..r0 as usize + count].fill((r1 & 0xFF) as u8);
                }
                r0 as i32
            }
            15 => self.guest_cstring(r0, 1 << 20).len() as i32,
            16 => {
                // strstr(haystack, needle)
                let haystack = self.guest_cstring(r0, 1 << 20);
                let needle = self.guest_cstring(r1, 4096);
                if needle.is_empty() {
                    return r0 as i32;
                }
                haystack
                    .windows(needle.len())
                    .position(|window| window == needle)
                    .map_or(0, |offset| (r0 + offset as u32) as i32)
            }
            17 => {
                // sprintf(dst, fmt, ...): varargs follow r3 on the stack.
                let formatted = self.guest_format(r1, 2);
                let dst = r0 as usize;
                if dst + formatted.len() < self.mem.len() {
                    self.mem[dst..dst + formatted.len()].copy_from_slice(&formatted);
                    self.mem[dst + formatted.len()] = 0;
                }
                formatted.len() as i32
            }
            18 => {
                // atoi
                let text = self.guest_cstring(r0, 64);
                let text = String::from_utf8_lossy(&text);
                trimmed_number(&text).parse::<i32>().unwrap_or(0)
            }
            19 => {
                // strtoul
                let text = self.guest_cstring(r0, 64);
                let text = String::from_utf8_lossy(&text);
                let base = if r2 == 16 { 16 } else { 10 };
                u32::from_str_radix(trimmed_number(&text).trim_start_matches("0x"), base)
                    .unwrap_or(0) as i32
            }
            20 => {
                // rand: simple LCG, seeded once from the clock.
                if self.rand_state == 0 {
                    self.rand_state = 0x1234_5678;
                }
                self.rand_state = self
                    .rand_state
                    .wrapping_mul(1_103_515_245)
                    .wrapping_add(12_345);
                ((self.rand_state >> 16) & 0x7FFF) as i32
            }
            27 => self.heap_malloc(r0) as i32,
            28 => {
                self.heap_free(r0, r1);
                MR_SUCCESS
            }
            _ => -1,
        }
    }

    pub fn refresh_screen(&mut self, screen: &mut crate::display::Screen) {
        if let Some(source) = self.framebuffer_address() {
            self.copy_screen(source, screen);
        } else {
            screen.fill(0);
        }
    }

    pub(crate) fn present_frame(&mut self, screen: &mut crate::display::Screen) {
        if !self.uses_explicit_video {
            self.refresh_screen(screen);
        }
    }

    fn copy_screen(&self, source: u32, screen: &mut crate::display::Screen) {
        let (width, height) = (self.config.screen_width, self.config.screen_height);
        if (screen.width(), screen.height()) != (width, height) {
            *screen = crate::display::Screen::new(width, height);
        }
        let pixels = screen.pixels_mut();
        let source = self.mem.get(source as usize..).unwrap_or_default();
        let byte_count = (pixels.len() * 2).min(source.len());
        let source = &source[..byte_count];
        for (pixel, bytes) in pixels.iter_mut().zip(source.as_chunks::<2>().0) {
            *pixel = u16::from_le_bytes(*bytes);
        }
        pixels[byte_count / 2..].fill(0);
    }

    fn swi_open(&mut self) -> i32 {
        let name = self.guest_file_path();
        let mode = self.cpu.reg_get(self.cpu.mode(), R1);
        log::info!("mr_open({name:?}, mode={mode:#x})");
        let writable = mode & (2 | 4) != 0;
        let readable = mode & 2 == 0 || mode & 4 != 0;
        self.open_file(&name, readable, writable, mode & 8 != 0)
            .map_or(0, |handle| handle as i32)
    }

    fn swi_close(&mut self) -> i32 {
        let handle = self.cpu.reg_get(self.cpu.mode(), R0) as usize;
        if self.close_file(handle) {
            0
        } else {
            -1
        }
    }

    fn swi_info(&mut self) -> i32 {
        let name = self.guest_file_path();
        self.ensure_system_font(&name);
        let result = if self.find_file(&name, false).is_some() {
            1 // MR_IS_FILE
        } else if self.directories.contains(&name) {
            2 // MR_IS_DIR
        } else {
            8 // MR_IS_INVALID
        };
        log::info!("mr_info({name:?}) -> {result}");
        result
    }

    fn swi_write(&mut self) -> i32 {
        let handle = self.cpu.reg_get(self.cpu.mode(), R0) as usize;
        let source = self.cpu.reg_get(self.cpu.mode(), R1) as usize;
        let len = self.cpu.reg_get(self.cpu.mode(), R2) as usize;
        let Some(data) = self.mem.get(source..source.saturating_add(len)) else {
            return -1;
        };
        let bytes = data.to_vec();
        if self.file_write(handle, &bytes) {
            len as i32
        } else {
            -1
        }
    }

    fn swi_read(&mut self) -> i32 {
        let handle = self.cpu.reg_get(self.cpu.mode(), R0) as usize;
        let buffer = self.cpu.reg_get(self.cpu.mode(), R1);
        let len = self.cpu.reg_get(self.cpu.mode(), R2) as usize;
        let Some(file) = self.open_files.get_mut(handle.wrapping_sub(1)) else {
            return -1;
        };
        if file.closed || !file.readable {
            return -1;
        }
        let (name, data) = &self.files[file.file_index];
        let start = file.pos.min(data.len());
        let remaining = data.len() - start;
        let count = len.min(remaining);
        log::info!(
            "mr_read(handle={handle} {:?} pos={:#x} len={count})",
            name,
            file.pos
        );
        let base = buffer as usize;
        let Some(destination) = self.mem.get_mut(base..base.saturating_add(count)) else {
            return -1;
        };
        destination.copy_from_slice(&data[start..start + count]);
        file.pos += count;
        count as i32
    }

    fn swi_seek(&mut self) -> i32 {
        let handle = self.cpu.reg_get(self.cpu.mode(), R0) as usize;
        let offset = self.cpu.reg_get(self.cpu.mode(), R1) as i32;
        let whence = self.cpu.reg_get(self.cpu.mode(), R2) as i32;
        let Some(file) = self.open_files.get_mut(handle.wrapping_sub(1)) else {
            return -1;
        };
        if file.closed {
            return -1;
        }
        let base = match whence {
            0 => 0,
            1 => file.pos as i64,
            2 => self.files[file.file_index].1.len() as i64,
            _ => return -1,
        };
        let Ok(target) = usize::try_from(base + i64::from(offset)) else {
            return -1;
        };
        log::info!("mr_seek(handle={handle} offset={offset} whence={whence} -> {target:#x})",);
        file.pos = target;
        0
    }

    fn swi_get_len(&mut self) -> i32 {
        let path = self.guest_file_path();
        self.ensure_system_font(&path);
        self.find_file(&path, true)
            .map_or(-1, |index| self.files[index].1.len() as i32)
    }

    fn swi_edit_create(&mut self) -> i32 {
        let mode = self.cpu.mode();
        let title = self.cpu.reg_get(mode, R0);
        let text = self.cpu.reg_get(mode, R1);
        let kind = self.cpu.reg_get(mode, R2);
        let max_chars = self.cpu.reg_get(mode, R3);
        if self.edit.is_some() || kind > 3 || max_chars > LG_MEM_LEN / 2 - 1 {
            return -1;
        }

        let buffer_size = (max_chars + 1) * 2;
        let buffer = self.heap_malloc(buffer_size);
        if buffer == 0 {
            return -1;
        }
        let title = self.read_platform_text(title, true);
        let text = self
            .guest_ucs2_bytes(text)
            .as_chunks::<2>()
            .0
            .iter()
            .take(max_chars as usize)
            .copied()
            .map(u16::from_be_bytes)
            .collect();
        self.next_gui_handle += 1;
        self.edit = Some(EditState {
            handle: self.next_gui_handle,
            title,
            text,
            kind,
            max_chars: max_chars as usize,
            buffer,
            buffer_size,
            visible: true,
            confirmation: None,
        });
        self.next_gui_handle as i32
    }

    fn swi_edit_release(&mut self) -> i32 {
        let handle = self.cpu.reg_get(self.cpu.mode(), R0);
        let Some(edit) = self.edit.take_if(|edit| edit.handle == handle) else {
            return -1;
        };
        self.heap_free(edit.buffer, edit.buffer_size);
        0
    }

    fn swi_edit_get_text(&mut self) -> i32 {
        let handle = self.cpu.reg_get(self.cpu.mode(), R0);
        let Some(edit) = self.edit.as_ref().filter(|edit| edit.handle == handle) else {
            return 0;
        };
        let start = edit.buffer as usize;
        let bytes = edit.text.len() * 2;
        for (word, cell) in edit
            .text
            .iter()
            .zip(self.mem[start..start + bytes].as_chunks_mut::<2>().0)
        {
            cell.copy_from_slice(&word.to_be_bytes());
        }
        self.mem[start + bytes..start + bytes + 2].fill(0);
        edit.buffer as i32
    }

    fn ensure_system_font(&mut self, path: &str) {
        let name = path.strip_prefix("mythroad/").unwrap_or(path);
        if !name.starts_with("system/") || self.find_file(path, false).is_some() {
            return;
        }
        if let Some(font) = crate::display::system_font(name) {
            self.files.push((path.to_string(), font.to_vec()));
        }
    }

    fn guest_file_path(&self) -> String {
        let path = self.guest_cstring(self.cpu.reg_get(self.cpu.mode(), R0), 512);
        self.resolve_storage_path(&encoding_rs::GBK.decode(&path).0)
    }

    fn resolve_storage_path(&self, name: &str) -> String {
        let name = normalize_file_path(name);
        if name.is_empty() {
            return name;
        }
        let full = if name.as_bytes().get(1) == Some(&b':') {
            name
        } else if self.storage_root == DEFAULT_STORAGE_ROOT {
            return name;
        } else {
            format!("{}{name}", self.storage_root)
        };
        let drive = char::from(full.as_bytes()[0].to_ascii_lowercase());
        let tail = full[2..].trim_start_matches('/');
        if drive == 'c' {
            if tail == "mythroad" {
                return String::new();
            }
            if let Some(path) = tail.strip_prefix("mythroad/") {
                return path.to_string();
            }
        }
        format!("disk/{drive}/{tail}")
            .trim_end_matches('/')
            .to_string()
    }

    fn swi_storage_root(&mut self) -> i32 {
        let mode = self.cpu.mode();
        let input = self.cpu.reg_get(mode, R1);
        let length = self.cpu.reg_get(mode, R2) as usize;
        let path = self.guest_cstring(input, length.min(510));
        let Some(drive) = path.first().map(u8::to_ascii_lowercase) else {
            return 1; // MR_IGNORE
        };
        match drive {
            b'y' => {
                let output = self.cpu.reg_get(mode, R3);
                let length = self.stack_arg(0);
                if output == 0 || length == 0 {
                    return -1;
                }
                let (bytes, _, _) = encoding_rs::GBK.encode(&self.storage_root);
                let start = STORAGE_PATH_BUFFER as usize;
                self.mem[start..start + bytes.len()].copy_from_slice(&bytes);
                self.mem[start + bytes.len()] = 0;
                let size = bytes.len() as u32;
                self.w32(output, STORAGE_PATH_BUFFER);
                self.w32(length, size);
            }
            b'z' => self.storage_root = DEFAULT_STORAGE_ROOT.to_string(),
            b'x' => self.storage_root = "x:/".to_string(),
            b'a' | b'b' | b'c' => {
                if path.len() > 1 && !matches!(path.get(1..3), Some(b":/" | b":\\")) {
                    return 1;
                }
                let suffix = path.get(3..).unwrap_or_default();
                let (suffix, _, invalid) = encoding_rs::GBK.decode(suffix);
                if invalid {
                    return -1;
                }
                let suffix = normalize_file_path(&suffix);
                self.storage_root = if suffix.is_empty() {
                    format!("{}:/", char::from(drive))
                } else {
                    format!("{}:/{suffix}/", char::from(drive))
                };
            }
            _ => return 1,
        }
        0
    }

    fn guest_ucs2_bytes(&self, address: u32) -> &[u8] {
        if address == 0 {
            return &[];
        }
        let data = self.mem.get(address as usize..).unwrap_or_default();
        let words = data
            .as_chunks::<2>()
            .0
            .iter()
            .take_while(|word| **word != [0, 0])
            .count();
        &data[..words * 2]
    }

    fn swi_c2u(&mut self) -> i32 {
        let mode = self.cpu.mode();
        let source = self.cpu.reg_get(mode, R0);
        let error = self.cpu.reg_get(mode, R1);
        let size = self.cpu.reg_get(mode, R2);
        if error != 0 {
            self.w32(error, u32::MAX);
        }
        if source == 0 || source as usize >= self.mem.len() {
            return 0;
        }
        let gbk = self.guest_cstring(source, self.mem.len() - source as usize);
        let (text, _, _) = encoding_rs::GBK.decode(&gbk);
        let bytes: Vec<_> = text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_be_bytes)
            .collect();
        let length = bytes.len() as u32;
        let output = self.heap_malloc(length);
        if output != 0 {
            self.mem[output as usize..output as usize + bytes.len()].copy_from_slice(&bytes);
        }
        if size != 0 {
            self.w32(size, if output == 0 { 0 } else { length });
        }
        output as i32
    }

    /// `mr_readFile(name, *len, lookfor)` reads the active package member.
    fn swi_read_file(&mut self) -> i32 {
        let name_addr = self.cpu.reg_get(self.cpu.mode(), R0);
        let len_ptr = self.cpu.reg_get(self.cpu.mode(), R1);
        let lookfor = self.cpu.reg_get(self.cpu.mode(), R2);
        let name = self.guest_cstring(name_addr, 512);
        let pack_name = self.guest_cstring(self.data_slot_address(DataSlot::PackFilename), 128);
        if pack_name.starts_with(b"$") {
            return self.swi_read_ram_entry(&name, len_ptr, lookfor);
        }
        let name = encoding_rs::GBK.decode(&name).0;
        let pack_name = self.resolve_storage_path(&encoding_rs::GBK.decode(&pack_name).0);
        let content = if pack_name != self.config.pack_filename {
            if let Some(index) = self.find_file(&pack_name, false) {
                crate::package::MrpPackage::parse(self.files[index].1.clone())
                    .and_then(|package| package.read_entry(&name))
                    .ok()
            } else {
                self.file_lookup(&name)
            }
        } else {
            self.file_lookup(&name)
        };
        if lookfor == 1 {
            let found = content.is_some() as i32;
            log::info!("mr_readFile({name:?}, lookfor={lookfor}) -> {found}");
            return found;
        }
        log::info!(
            "mr_readFile({name:?}, lookfor={lookfor}) -> {} bytes",
            content.as_ref().map(|data| data.len()).unwrap_or(0)
        );
        let Some(content) = content else {
            self.w32(len_ptr, 0);
            return 0;
        };
        self.allocate_file_bytes(&content, len_ptr)
    }

    fn allocate_file_bytes(&mut self, content: &[u8], len_ptr: u32) -> i32 {
        let block = self.heap_malloc(content.len() as u32);
        if block == 0 {
            return 0;
        }
        let base = block as usize;
        self.mem[base..base + content.len()].copy_from_slice(content);
        self.w32(len_ptr, content.len() as u32);
        block as i32
    }

    fn swi_read_ram_entry(&mut self, name: &[u8], len_ptr: u32, lookfor: u32) -> i32 {
        use std::io::Read;

        let base = self.get_data(DataSlot::RamFile);
        let length = self.get_data(DataSlot::RamFileLen);
        let Some(range) = base
            .checked_add(length)
            .and_then(|end| self.mem.get(base as usize..end as usize))
            .and_then(|package| ram_entry_range(package, name))
        else {
            log::debug!("RAM package member {name:?} missing at {base:#x} ({length} bytes)");
            if lookfor != 1 {
                self.w32(len_ptr, 0);
            }
            return 0;
        };
        if lookfor == 1 {
            return 1;
        }
        let start = base as usize + range.start;
        let raw = &self.mem[start..base as usize + range.end];
        let raw_len = raw.len() as u32;
        if lookfor == 2 || !raw.starts_with(&[0x1f, 0x8b]) {
            self.w32(len_ptr, raw_len);
            return start as i32;
        }
        let Some(size) = raw.last_chunk::<4>().copied().map(u32::from_le_bytes) else {
            return 0;
        };
        let mut decoded = Vec::new();
        let decoder = flate2::read::GzDecoder::new(raw);
        let success = size <= HEAP_MAX_ALLOC
            && decoder
                .take(u64::from(size) + 1)
                .read_to_end(&mut decoded)
                .is_ok()
            && decoded.len() == size as usize;
        self.w32(len_ptr, size);
        if success {
            self.allocate_file_bytes(&decoded, len_ptr)
        } else {
            log::debug!("invalid compressed RAM package member {name:?}");
            0
        }
    }

    /// Looks up `name` in the file table seeded at boot (package members
    /// and VFS content). Falls back to a basename match so guests opening
    /// `mythroad/x.png` find the packaged `x.png`, and resolves any other
    /// `<something>.mrp` query to the running package itself (guests open
    /// the pack under its platform-assigned disk name, which is not a
    /// member of its own file list).
    pub fn file_lookup(&self, name: &str) -> Option<Vec<u8>> {
        let index = self.find_file(&normalize_file_path(name), true)?;
        Some(self.files[index].1.clone())
    }

    fn find_file(&self, name: &str, allow_basename: bool) -> Option<usize> {
        if name.is_empty() {
            return None;
        }
        let exact = self.files.iter().position(|(entry, _)| entry == name);
        if exact.is_some() || !allow_basename {
            return exact;
        }
        let base = name.rsplit('/').next().unwrap_or(name);
        if let Some(index) = self.files.iter().position(|(entry, _)| entry == base) {
            return Some(index);
        }
        if base == name && base.to_ascii_lowercase().ends_with(".mrp") {
            return self.files.iter().position(|(entry, _)| entry == "app.mrp");
        }
        None
    }

    /// Internal-table bridge entry points, indexed by `_mr_c_internal_table`
    /// slot. Entries 12-15 are the SMS/isMr helpers; 16-76 are the Lua C API
    /// (`asm_mrp_*` in the reference runtime); 77 is NULL.
    const VM_BRIDGE_NAMES: [&str; 65] = [
        // 12-15: SMS/isMr helpers.
        "smsSetBytes",
        "smsAddNum",
        "newSIMInd",
        "isMr",
        // 16-76: the Lua C API, in reference table order.
        "mrp_gettop",            // 16
        "mrp_settop",            // 17
        "mrp_pushvalue",         // 18
        "mrp_remove",            // 19
        "mrp_insert",            // 20
        "mrp_replace",           // 21
        "mrp_isnumber",          // 22
        "mrp_isstring",          // 23
        "mrp_iscfunction",       // 24
        "mrp_isuserdata",        // 25
        "mrp_type",              // 26
        "mrp_typename",          // 27
        "mrp_shorttypename",     // 28
        "mrp_equal",             // 29
        "mrp_rawequal",          // 30
        "mrp_lessthan",          // 31
        "mrp_tonumber",          // 32
        "mrp_toboolean",         // 33
        "mrp_tostring",          // 34
        "mrp_strlen",            // 35
        "mrp_tostring_t",        // 36
        "mrp_strlen_t",          // 37
        "mrp_tocfunction",       // 38
        "mrp_touserdata",        // 39
        "mrp_tothread",          // 40
        "mrp_topointer",         // 41
        "mrp_pushnil",           // 42
        "mrp_pushnumber",        // 43
        "mrp_pushlstring",       // 44
        "mrp_pushstring",        // 45
        "mrp_pushvfstring",      // 46
        "mrp_pushfstring",       // 47
        "mrp_pushboolean",       // 48
        "mrp_pushcclosure",      // 49
        "mrp_gettable",          // 50
        "mrp_rawget",            // 51
        "mrp_rawgeti",           // 52
        "mrp_newtable",          // 53
        "mrp_getmetatable",      // 54
        "mrp_settable",          // 55
        "mrp_rawset",            // 56
        "mrp_rawseti",           // 57
        "mrp_call",              // 58
        "mrp_pcall",             // 59
        "mrp_load",              // 60
        "mrp_getgcthreshold",    // 61
        "mrp_setgcthreshold",    // 62
        "mrp_error",             // 63
        "mrp_checkstack",        // 64
        "mrp_newuserdata",       // 65
        "mrp_getfenv",           // 66
        "mrp_setfenv",           // 67
        "mrp_setmetatable",      // 68
        "mrp_cpcall",            // 69
        "mrp_next",              // 70
        "mrp_concat",            // 71
        "mrp_pushlightuserdata", // 72
        "mrp_getgccount",        // 73
        "mrp_dump",              // 74
        "mrp_yield",             // 75
        "mrp_resume",            // 76
    ];

    /// Dispatches one internal-table bridge call. `slot` is the
    /// `_mr_c_internal_table` index (12-76).
    fn dispatch_vm_bridge(&mut self, slot: usize, screen: &mut crate::display::Screen) -> i32 {
        let name = Self::VM_BRIDGE_NAMES
            .get(slot)
            .copied()
            .unwrap_or("<out of range>");
        if std::env::var("MRP_TRACE").is_ok() {
            let r0 = self.cpu.reg_get(self.cpu.mode(), R0);
            let r1 = self.cpu.reg_get(self.cpu.mode(), R1);
            let r2 = self.cpu.reg_get(self.cpu.mode(), R2);
            let r3 = self.cpu.reg_get(self.cpu.mode(), R3);
            eprintln!(
                "[vmbridge {name}] slot={slot} L={r0:#010x} a1={r1:#010x} a2={r2:#010x} a3={r3:#010x} top={}",
                self.lua.stack.len()
            );
        }
        self.lua_bridge(slot, screen);
        self.vm_bridge_ret as i32
    }

    /// Host implementation of the internal-table bridge (the `asm_mrp_*`
    /// Lua C API and the SMS helpers). Arguments arrive in r0-r3 plus the
    /// stack; results are left in [`Self::vm_bridge_ret`].
    fn lua_bridge(&mut self, slot: usize, screen: &mut crate::display::Screen) {
        use crate::luastate::*;
        use crate::vm::Value;
        let r0 = self.cpu.reg_get(self.cpu.mode(), R0);
        let r1 = self.cpu.reg_get(self.cpu.mode(), R1);
        let r2 = self.cpu.reg_get(self.cpu.mode(), R2);
        let trace = std::env::var("MRP_TRACE").is_ok();
        let stack_len = self.lua.stack.len();
        // Most ops answer through this closure over the shared state; a
        // few (cclosure entry) set the return value themselves.
        #[allow(unused_assignments)]
        let mut ret: u32 = 0;
        match slot {
            // 12-15: SMS/isMr helpers — unused by game flows so far.
            12..=15 => {
                if trace {
                    eprintln!("[vmbridge] helper slot {slot} args {r0:#x} {r1:#x} {r2:#x}");
                }
                ret = 0;
            }
            // 16: mrp_gettop
            16 => {
                let base = self.lua.frames.last().copied().unwrap_or(0);
                ret = (stack_len - base) as u32;
            }
            // 17: mrp_settop(idx)
            17 => {
                self.lua.set_top(r1 as i32);
                ret = 0;
            }
            // 18: mrp_pushvalue(idx)
            18 => {
                let value = self.lua.peek(r1 as i32).unwrap_or(Value::Nil);
                self.lua.push(value);
                ret = 0;
            }
            // 19: mrp_remove(idx) — discards the element, shifting the
            // elements above it down.
            19 => {
                if let Some(pos) = self.lua.abs_index(r1 as i32) {
                    self.lua.stack.remove(pos);
                }
                ret = 0;
            }
            // 20: mrp_insert(idx) — moves the top element into `idx`,
            // shifting the others up.
            20 => {
                if let Some(pos) = self.lua.abs_index(r1 as i32) {
                    let top = stack_len - 1;
                    if pos < top {
                        let value = self.lua.stack.remove(top);
                        self.lua.stack.insert(pos, value);
                    }
                }
                ret = 0;
            }
            // 21: mrp_replace(idx) — moves the top element into `idx`.
            21 => {
                if let Some(pos) = self.lua.abs_index(r1 as i32) {
                    let top = self.lua.stack.len() - 1;
                    if pos != top {
                        let value = self.lua.stack.pop().unwrap_or(Value::Nil);
                        self.lua.stack[pos] = value;
                    }
                }
                ret = 0;
            }
            // 22: mrp_isnumber(idx)
            22 => {
                ret = (self.lua.type_at(r1 as i32) == T_NUMBER) as u32;
            }
            // 23: mrp_isstring(idx) — true for strings and numbers
            // (convertible).
            23 => {
                let t = self.lua.type_at(r1 as i32);
                ret = ((t == T_STRING) || (t == T_NUMBER)) as u32;
            }
            // 24: mrp_iscfunction(idx)
            24 => {
                let is_c = matches!(self.lua.peek(r1 as i32), Some(Value::Native(_)));
                ret = is_c as u32;
            }
            // 25: mrp_isuserdata(idx)
            25 => {
                let t = self.lua.type_at(r1 as i32);
                ret = ((t == T_USERDATA) || (t == T_LIGHTUSERDATA)) as u32;
            }
            // 26: mrp_type(idx)
            26 => {
                ret = self.lua.type_at(r1 as i32) as u32;
            }
            // 27/28: mrp_typename(t) / mrp_shorttypename(t) — the argument
            // is a type CODE; the answer is a static guest string.
            27 | 28 => {
                let code = r1 as i32;
                let index = if (0..=8).contains(&code) {
                    code as u32
                } else {
                    0
                };
                ret = if slot == 27 {
                    VM_STRINGS_BASE + index * 16
                } else {
                    VM_STRINGS_BASE + 9 * 16 + index * 16
                };
            }
            // 29/30: mrp_equal / mrp_rawequal
            29 | 30 => {
                let a = self.lua.peek(r1 as i32);
                let b = self.lua.peek(r2 as i32);
                ret = (a.is_some() && a == b) as u32;
            }
            // 31: mrp_lessthan
            31 => {
                let a = self.lua.peek(r1 as i32);
                let b = self.lua.peek(r2 as i32);
                ret = match (a, b) {
                    (Some(Value::Num(x)), Some(Value::Num(y))) => (x < y) as u32,
                    (Some(Value::Str(x)), Some(Value::Str(y))) => (x.as_ref() < y.as_ref()) as u32,
                    _ => 0,
                };
            }
            // 32: mrp_tonumber(idx) — the answer travels as float bits.
            32 => {
                ret = match self.lua.peek(r1 as i32) {
                    Some(value) => value.as_num().map(|n| n.to_bits()).unwrap_or(0),
                    None => 0,
                };
            }
            // 33: mrp_toboolean(idx)
            33 => {
                let truthy = !matches!(
                    self.lua.peek(r1 as i32),
                    None | Some(Value::Nil) | Some(Value::Bool(false))
                );
                ret = truthy as u32;
            }
            // 34/36: mrp_tostring / mrp_tostring_t — convert and return a
            // guest-readable pointer (interned through the heap).
            34 | 36 => match self.lua.peek(r1 as i32) {
                Some(value) => {
                    let bytes = self.lua_string_bytes(&value);
                    ret = self.lua_intern_string(&bytes);
                }
                None => ret = 0,
            },
            // 35/37: mrp_strlen / mrp_strlen_t
            35 | 37 => {
                ret = match self.lua.peek(r1 as i32) {
                    Some(value) => self.lua_string_bytes(&value).len() as u32,
                    None => 0,
                };
            }
            // 38: mrp_tocfunction(idx)
            38 => {
                ret = self
                    .lua
                    .peek(r1 as i32)
                    .and_then(|value| self.lua.cclosure_ptr(&value))
                    .unwrap_or(0);
            }
            // 39/41: mrp_touserdata / mrp_topointer
            39 | 41 => {
                ret = self
                    .lua
                    .peek(r1 as i32)
                    .and_then(|value| self.lua.userdata_ptr(&value))
                    .unwrap_or(0);
            }
            // 40: mrp_tothread — no coroutine support.
            40 => ret = 0,
            // 42: mrp_pushnil
            42 => {
                self.lua.push(Value::Nil);
                ret = 0;
            }
            // 43: mrp_pushnumber(n) — n arrives as float bits in r1.
            43 => {
                self.lua.push(Value::Num(f32::from_bits(r1)));
                ret = 0;
            }
            // 44: mrp_pushlstring(s, len)
            44 => {
                let bytes = self.guest_bytes(r1, r2 as usize).unwrap_or_default();
                self.lua.push(Value::str_from(&bytes));
                ret = 0;
            }
            // 45: mrp_pushstring(s)
            45 => {
                let bytes = self.guest_cstring(r1, 4096);
                self.lua.push(Value::str_from(&bytes));
                ret = 0;
            }
            // 46/47: mrp_pushvfstring / mrp_pushfstring(fmt, ...)
            46 | 47 => {
                let bytes = self.guest_format(r1, 2);
                ret = self.lua_intern_string(&bytes);
            }
            // 48: mrp_pushboolean(b)
            48 => {
                self.lua.push(Value::Bool(r1 != 0));
                ret = 0;
            }
            // 49: mrp_pushcclosure(fn, n)
            49 => {
                if r2 > 0 && trace {
                    eprintln!("[vmbridge] pushcclosure with {r2} upvalues (ignored)");
                }
                self.lua.push_cclosure(r1, r2 as i32);
                ret = 0;
            }
            // 50: mrp_gettable(idx), 51: mrp_rawget(idx)
            50 | 51 => {
                self.lua.get_table(r1 as i32);
                ret = 0;
            }
            // 52: mrp_rawgeti(idx, n)
            52 => {
                self.lua.raw_geti(r1 as i32, r2 as i32);
                ret = 0;
            }
            // 53: mrp_newtable
            53 => {
                self.lua.push(Value::Table(Rc::new(RefCell::new(
                    crate::vm::Table::default(),
                ))));
                ret = 0;
            }
            // 54: mrp_getmetatable — no metatable support yet.
            54 => ret = 0,
            // 55: mrp_settable(idx), 56: mrp_rawset(idx)
            55 | 56 => {
                self.lua.set_table(r1 as i32);
                ret = 0;
            }
            // 57: mrp_rawseti(idx, n)
            57 => {
                self.lua.raw_seti(r1 as i32, r2 as i32);
                ret = 0;
            }
            // 58: mrp_call(nargs, nresults), 59: mrp_pcall(nargs, nresults, err)
            58 | 59 => {
                let nargs = r1 as i32;
                let nresults = r2 as i32;
                if nargs < 0 || (stack_len as i32) < nargs + 1 {
                    if trace {
                        eprintln!("[vmbridge] call with nargs={nargs} but top={stack_len}");
                    }
                    ret = 0;
                } else {
                    self.lua_call(nargs, nresults, screen);
                    ret = 0;
                }
            }
            // 60: mrp_load(reader, data, chunkname) — deferred until the
            // corpus shows a package reaching it.
            60 => {
                if trace {
                    let r3 = self.cpu.reg_get(self.cpu.mode(), R3);
                    eprintln!(
                        "[vmbridge] mrp_load reader={r1:#010x} data={r2:#010x} name={r3:#010x} (unimplemented)"
                    );
                }
                ret = 1; // MRP_ERR_RUN: caller falls back gracefully
            }
            // 61/73: gc queries; 62: threshold set; 64: checkstack.
            61 | 73 => ret = 0,
            62 => ret = 0,
            64 => ret = 1,
            // 63: mrp_error — record and return.
            63 => {
                let message = self.lua.peek(-1).map(|value| self.lua_string_bytes(&value));
                self.lua.error = message.map(|bytes| String::from_utf8_lossy(&bytes).to_string());
                if trace {
                    eprintln!("[vmbridge] mrp_error: {:?}", self.lua.error);
                }
                ret = 0;
            }
            // 65: mrp_newuserdata(size)
            65 => {
                let block = self.heap_malloc(r1.max(1));
                if block != 0 {
                    for offset in 0..r1 {
                        self.w8(block + offset, 0);
                    }
                    self.lua.new_userdata(block, r1);
                }
                ret = block;
            }
            // 66-69: fenv/metatable/cpcall — cpcall is a direct guest call.
            66..=68 => ret = 0,
            69 => {
                // mrp_cpcall(func, ud): call func(L, ud); func returns 0 on
                // success or an error code.
                ret = self.lua_enter_guest(r1, r2, screen) as u32;
            }
            // 70: mrp_next — iteration unimplemented; nil ends the loop.
            70 => {
                if trace {
                    eprintln!("[vmbridge] mrp_next (unimplemented; terminates iteration)");
                }
                self.lua.push(Value::Nil);
                ret = 0;
            }
            // 71: mrp_concat(n)
            71 => {
                let n = (r1 as i32).max(1) as usize;
                let top = self.lua.stack.len();
                if top >= n {
                    let mut joined: Vec<u8> = Vec::new();
                    for value in &self.lua.stack[top - n..] {
                        joined.extend_from_slice(&self.lua_string_bytes(value));
                    }
                    self.lua.stack.truncate(top - n + 1);
                    self.lua.push(Value::str_from(&joined));
                }
                ret = 0;
            }
            // 72: mrp_pushlightuserdata(p)
            72 => {
                self.lua.push_lightuserdata(r1);
                ret = 0;
            }
            // 74: mrp_dump; 75: mrp_yield; 76: mrp_resume.
            74 => ret = 1,
            75 | 76 => {
                if trace {
                    eprintln!("[vmbridge] coroutine op slot {slot} (unimplemented)");
                }
                ret = 0;
            }
            _ => {
                if trace {
                    eprintln!("[vmbridge] unknown slot {slot}");
                }
                ret = 0;
            }
        }
        self.vm_bridge_ret = ret;
    }

    /// The guest-readable bytes of a value (number → Lua `%.14g` formatting,
    /// string → its bytes).
    fn lua_string_bytes(&self, value: &crate::vm::Value) -> Vec<u8> {
        match value {
            crate::vm::Value::Str(bytes) => bytes.as_ref().clone(),
            crate::vm::Value::Num(n) => {
                let d = *n as f64;
                if d.fract() == 0.0 && d.abs() < 1e15 {
                    format!("{}", d as i64).into_bytes()
                } else {
                    format!("{:.14}", d)
                        .trim_end_matches('0')
                        .trim_end_matches('.')
                        .as_bytes()
                        .to_vec()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Interns bytes into guest memory so the Lua bridge can hand back a
    /// stable `const char*`; equal content shares one block (Lua interning).
    fn lua_intern_string(&mut self, bytes: &[u8]) -> u32 {
        if let Some(ptr) = self.lua_string_ptrs.get(bytes) {
            return *ptr;
        }
        let block = self.heap_malloc(bytes.len() as u32 + 1);
        if block == 0 {
            return 0;
        }
        let base = block as usize;
        self.mem[base..base + bytes.len()].copy_from_slice(bytes);
        self.mem[base + bytes.len()] = 0;
        self.lua_string_ptrs.insert(bytes.to_vec(), block);
        block
    }

    /// `mrp_call`/`mrp_pcall`: the function is at `-nargs-1`. Guest C
    /// closures run re-entrantly on a fresh guest stack region; bytecode
    /// closures would need the MiniJ VM (deferred until the corpus shows
    /// `mrp_load` in use).
    fn lua_call(&mut self, nargs: i32, nresults: i32, screen: &mut crate::display::Screen) {
        let top = self.lua.stack.len();
        let fn_pos = top - nargs as usize - 1;
        let function = self.lua.stack[fn_pos].clone();
        let trace = std::env::var("MRP_TRACE").is_ok();
        match function {
            crate::vm::Value::Native(index) if index >= crate::luastate::CCLOSURE_BASE => {
                let fn_ptr = self
                    .lua
                    .cclosures
                    .get((index - crate::luastate::CCLOSURE_BASE) as usize)
                    .copied()
                    .unwrap_or(0);
                if fn_ptr == 0 {
                    self.lua.stack.truncate(fn_pos);
                    self.lua.push(Value::Nil);
                    return;
                }
                // C-function call frame: args live above the function;
                // negative indices inside resolve against this base.
                self.lua.frames.push(fn_pos + 1);
                let count = self.lua_enter_guest(fn_ptr, 0, screen);
                self.lua.frames.pop();
                self.lua_collect_results(fn_pos, count.max(0) as usize, nresults);
            }
            crate::vm::Value::Closure(closure) => {
                // Bytecode: run through the MiniJ VM. Guest cclosure calls
                // from bytecode would need re-entrant guest entry inside the
                // VM loop; the corpus has not shown that path yet.
                let args: Vec<crate::vm::Value> = self.lua.stack[fn_pos + 1..].to_vec();
                self.lua.stack.truncate(fn_pos);
                let vm = std::mem::take(&mut self.lua_vm);
                let mut vm = vm.unwrap_or_default();
                vm.globals = Rc::clone(&self.lua.globals);
                let result = vm.call_closure(closure, &args);
                self.lua_vm = Some(vm);
                match result {
                    Ok(results) => {
                        for value in results {
                            self.lua.push(value);
                        }
                        self.lua_set_results_count(fn_pos, nresults);
                    }
                    Err(error) => {
                        self.lua.error = Some(error);
                        if trace {
                            eprintln!("[vmbridge] call error: {:?}", self.lua.error);
                        }
                        self.lua.push(Value::Nil);
                        if nresults >= 0 {
                            self.lua_set_results_count(fn_pos, nresults);
                        }
                    }
                }
            }
            other => {
                if trace {
                    eprintln!(
                        "[vmbridge] call on non-callable ({})",
                        crate::luastate::type_name(crate::luastate::type_code(&other))
                    );
                }
                self.lua.stack.truncate(fn_pos);
                self.lua.push(Value::Nil);
            }
        }
    }

    /// Finishes a direct guest-cclosure call: replaces the function and its
    /// arguments with the `count` results the callee left on the stack,
    /// padded/truncated to `nresults` when it is non-negative.
    fn lua_collect_results(&mut self, fn_pos: usize, count: usize, nresults: i32) {
        let top = self.lua.stack.len();
        // Results are the top `count` values; drop everything below them.
        if count < top - fn_pos {
            let keep_from = top - count;
            let results: Vec<crate::vm::Value> = self.lua.stack[keep_from..].to_vec();
            self.lua.stack.truncate(fn_pos);
            for value in results {
                self.lua.push(value);
            }
        }
        if nresults < 0 {
            return;
        }
        let want = nresults as usize;
        let have = self.lua.stack.len() - fn_pos;
        if have < want {
            for _ in 0..want - have {
                self.lua.push(Value::Nil);
            }
        } else {
            self.lua.stack.truncate(fn_pos + want);
        }
    }

    /// Applies `nresults` padding/truncation to a results block that the
    /// caller already pushed above `fn_pos`.
    fn lua_set_results_count(&mut self, fn_pos: usize, nresults: i32) {
        if nresults < 0 {
            return;
        }
        let want = nresults as usize;
        let have = self.lua.stack.len() - fn_pos;
        if have < want {
            for _ in 0..want - have {
                self.lua.push(Value::Nil);
            }
        } else {
            self.lua.stack.truncate(fn_pos + want);
        }
    }

    /// Enters a guest C function re-entrantly: suspends the current guest
    /// state, runs the callee on a fresh stack region below the suspended
    /// frame, and restores the suspended state afterwards. Returns r0.
    fn lua_enter_guest(
        &mut self,
        fn_ptr: u32,
        arg1: u32,
        screen: &mut crate::display::Screen,
    ) -> i32 {
        if fn_ptr == 0 {
            return 0;
        }
        // Snapshot the suspended execution: r0-r12, SP, LR, PC, CPSR (the
        // reg file maps indices 0-16 in the current mode).
        let mode = self.cpu.mode();
        let mut snapshot = [0u32; 17];
        for (index, slot) in snapshot.iter_mut().enumerate() {
            *slot = self.cpu.reg_get(mode, index as u8);
        }
        let suspended_sp = snapshot[13];
        // The callee's stack grows below the suspended frame; 64 KB keeps
        // the regions disjoint for any realistic C function depth.
        let nested_sp = suspended_sp.min(STACK_TOP) - 0x1_0000;
        // Second argument (cpcall's `ud`); the Lua-stack args are read by
        // the callee through the bridge, not registers.
        self.cpu.reg_set(self.cpu.mode(), R1, arg1);
        let result = self.run_guest_with_sp(fn_ptr, nested_sp, 20_000_000, screen);
        // Restore the suspended execution regardless of the callee outcome.
        let mode = self.cpu.mode();
        for (index, value) in snapshot.iter().enumerate() {
            self.cpu.reg_set(mode, index as u8, *value);
        }
        match result {
            Ok(value) => value as i32,
            Err(error) => {
                let trace = std::env::var("MRP_TRACE").is_ok();
                if trace {
                    eprintln!("[vmbridge] guest cclosure failed: {error}");
                }
                0
            }
        }
    }

    fn dispatch_test_com(&mut self, input0: u32, input1: u32) -> i32 {
        match input0 {
            0x02 => {
                self.event_function = input1;
                0
            }
            0x03 => {
                self.timer_function = input1;
                0
            }
            0x04 => {
                self.stop_function = input1;
                0
            }
            0x05 => {
                self.pause_app_function = input1;
                0
            }
            0x06 => {
                self.resume_app_function = input1;
                0
            }
            0x07 => input1 as i32,
            0x64 => self.heap_min as i32,
            0x65 => self.heap_top as i32,
            0x66 => self.heap_left as i32,
            0x12c => {
                self.set_data(DataSlot::SoundOn, u32::from(input1 as i8 as u8));
                0
            }
            0x12d => {
                self.set_data(DataSlot::ShakeOn, u32::from(input1 as i8 as u8));
                0
            }
            0x12e => {
                self.update_flags(0x4, 0);
                0
            }
            0x12f => {
                self.update_flags(0, 0x4);
                0
            }
            0x130 => {
                self.update_flags(0x8, 0);
                0
            }
            0x131 => {
                self.update_flags(0, 0x8);
                0
            }
            0x132 => {
                self.set_data(DataSlot::SmsReturnFlag, 1);
                self.set_data(DataSlot::SmsReturnVal, input1);
                0
            }
            0x133 => {
                self.set_data(DataSlot::SmsReturnFlag, 0);
                0
            }
            0x190 => 0, // sleep
            0x191 => {
                let old = self.get_data(DataSlot::ScreenWidth);
                self.set_data(DataSlot::ScreenWidth, input1);
                old as i32
            }
            0x196 => {
                let old = self.get_data(DataSlot::ScreenHeight);
                self.set_data(DataSlot::ScreenHeight, input1);
                old as i32
            }
            0xe2d if input1 == 0xb61 => {
                self.update_flags(0x1, 0);
                0
            }
            0xf51 if input1 == 0x18030 => {
                self.update_flags(0x2, 0);
                0
            }
            0xcb3 if input1 == 0x9E67A => {
                self.update_flags(0, 0x2);
                0
            }
            other => {
                warn_once(
                    "testcom",
                    format!(
                        "mr_testCom got unknown code {other:#x} (arg {input1:#x}, stack {:#x})",
                        self.stack_arg(0)
                    ),
                );
                1 // MR_IGNORE
            }
        }
    }

    fn load_nested_ext(&mut self, buffer: u32, screen: &mut crate::display::Screen) -> Option<i32> {
        if buffer < EXT_BASE || buffer as usize > self.mem.len() - 12 || buffer & 3 != 0 {
            return None;
        }
        let caller = self.cpu;
        let caller_sp = caller.reg_get(caller.mode(), reg::SP);
        let caller_base = self.ext_load_base;
        let registration = (self.c_function, self.c_function_p);
        self.ext_load_base = buffer;
        self.w32(buffer, TABLE_BASE);
        self.cpu.reg_set(self.cpu.mode(), R0, 0);
        let result =
            self.run_guest_with_sp(buffer + 8, caller_sp & !7, BOOT_INSTRUCTION_LIMIT, screen);
        self.cpu = caller;
        if !matches!(result, Ok(value) if value != u32::MAX) {
            self.ext_load_base = caller_base;
            (self.c_function, self.c_function_p) = registration;
        }
        match result {
            Ok(value) => Some(value as i32),
            Err(error) => {
                log::debug!("nested ext load failed: {error}");
                None
            }
        }
    }

    /// Loads an ext image at `addr` (script-driven flow): writes the
    /// `mr_table` pointer at base+0 and calls `mr_c_function_load` with the
    /// script-supplied mode selector. After the ext registers its handler,
    /// the platform builds the `mrc_extChunk` descriptor in the ext's P
    /// block (mirroring the reference shell's `mrc_extLoad`): it carries the
    /// `sendAppEvent` callback the ext's timer wrappers call, so timer
    /// traffic reaches the platform instead of dying on a null pointer.
    pub fn load_ext_at(
        &mut self,
        ext: &[u8],
        addr: u32,
        code: i32,
        screen: &mut crate::display::Screen,
    ) -> Result<i32, MrpError> {
        if ext.len() as u32 > EXT_MAX_SIZE || addr + ext.len() as u32 > MEM_SIZE {
            return Err(MrpError::Unsupported(format!(
                "ext too large or out of range at {addr:#010x}"
            )));
        }
        let base = addr as usize;
        self.ext_load_base = addr;
        self.mem[base..base + ext.len()].copy_from_slice(ext);
        self.w32(addr, TABLE_BASE);
        self.cpu.reg_set(self.cpu.mode(), R0, code as u32);
        let result = self
            .run_guest(addr + 8, BOOT_INSTRUCTION_LIMIT, screen)
            .map(|value| value as i32)?;
        self.install_ext_chunk(addr, ext.len() as u32);
        Ok(result)
    }

    /// Builds the `mrc_extChunk` descriptor for the ext loaded at `addr`,
    /// linking it through `P->mrc_extChunk` (offset 12) the way the
    /// reference loader shell does before the first helper call.
    fn install_ext_chunk(&mut self, addr: u32, len: u32) {
        if self.c_function == 0 || self.c_function_p == 0 {
            return;
        }
        let chunk = self.heap_malloc(0x30);
        if chunk == 0 {
            return;
        }
        self.w32(chunk, 0x7FD8_54EB); // check marker
        self.w32(chunk + 0x04, addr + 8); // init_func
        self.w32(chunk + 0x08, self.c_function); // event
        self.w32(chunk + 0x0c, addr); // code_buf
        self.w32(chunk + 0x10, len); // code_len
        self.w32(chunk + 0x14, self.r32(self.c_function_p)); // var_buf
        self.w32(chunk + 0x18, self.r32(self.c_function_p + 4)); // var_len
        self.w32(chunk + 0x1c, self.c_function_p); // global_p_buf
        self.w32(chunk + 0x20, 20); // global_p_len
        self.w32(chunk + 0x24, 1); // timer
        self.w32(chunk + 0x28, SEND_APP_EVENT_ADDR); // sendAppEvent
        self.w32(chunk + 0x2c, TABLE_BASE); // extMrTable
        self.w32(self.c_function_p + 12, chunk);
    }

    /// Boot handshake used by the reference loader: register the table,
    /// call `mr_c_function_load`, then run the 6/8/0 call sequence.
    pub fn boot_ext(
        &mut self,
        ext: &[u8],
        app_id: u32,
        app_version: u32,
        screen: &mut crate::display::Screen,
    ) -> Result<(), MrpError> {
        if ext.len() as u32 > EXT_MAX_SIZE {
            return Err(MrpError::Unsupported(format!(
                "ext too large: {} bytes",
                ext.len()
            )));
        }
        let base = EXT_BASE as usize;
        self.ext_load_base = EXT_BASE;
        self.mem[base..base + ext.len()].copy_from_slice(ext);
        self.w32(EXT_BASE, TABLE_BASE);

        // mr_testCom(0, 0xe2d, 0xb61): the loader's flag handshake.
        self.update_flags(0x1, 0);

        // mr_c_function_load(code): the ext compares r0 against 1 and only
        // takes the mr_c_function_new registration path when the argument
        // is 1 (observed in the reference ext's disassembly); other values
        // jump to an alternate entry.
        self.cpu.reg_set(self.cpu.mode(), R0, 1);
        self.run_guest(EXT_BASE + 8, BOOT_INSTRUCTION_LIMIT, screen)?;
        self.install_ext_chunk(EXT_BASE, ext.len() as u32);
        if std::env::var("MRP_TRACE").is_ok() {
            eprintln!(
                "[boot] load done, c_function={:#010x} p={:#010x}",
                self.c_function, self.c_function_p
            );
        }
        if self.c_function == 0 {
            return Err(MrpError::Unsupported(
                "ext did not register an event handler via mr_c_function_new".to_string(),
            ));
        }

        // code 6: loader handshake with the raw ext region.
        self.call_c_function(
            6,
            EXT_BASE,
            MYTHROAD_MINI_VERSION,
            BOOT_INSTRUCTION_LIMIT,
            screen,
        )?;
        // code 8: application info block (id, version, sidName, reserved).
        let app_info = self.heap_malloc(16);
        if app_info == 0 {
            return Err(MrpError::Unsupported(
                "app info allocation failed".to_string(),
            ));
        }
        self.w32(app_info, app_id);
        self.w32(app_info + 4, app_version);
        self.w32(app_info + 8, 0);
        self.w32(app_info + 12, 0);
        self.call_c_function(8, app_info, 16, BOOT_INSTRUCTION_LIMIT, screen)?;
        // code 0: mrc_init.
        self.call_c_function(
            0,
            EXT_BASE,
            MYTHROAD_MINI_VERSION,
            BOOT_INSTRUCTION_LIMIT,
            screen,
        )?;
        Ok(())
    }

    /// Delivers an event through whichever channel the ext registered.
    pub fn send_event(
        &mut self,
        event_type: i32,
        param0: i32,
        param1: i32,
        screen: &mut crate::display::Screen,
    ) -> Result<i32, MrpError> {
        if std::env::var("MRP_TRACE").is_ok() {
            eprintln!(
                "[send_event] type={event_type} p0={param0} p1={param1} evt_fn={:#x} cfn={:#x}",
                self.event_function, self.c_function
            );
        }
        if self.event_function != 0 {
            self.cpu.reg_set(self.cpu.mode(), R0, event_type as u32);
            self.cpu.reg_set(self.cpu.mode(), R1, param0 as u32);
            self.cpu.reg_set(self.cpu.mode(), R2, param1 as u32);
            return self
                .run_guest(self.event_function, DEFAULT_INSTRUCTION_LIMIT, screen)
                .map(|value| value as i32);
        }
        if self.c_function != 0 {
            // Mini uses a five-word event record.
            self.w32(EVENT_RECORD, event_type as u32);
            self.w32(EVENT_RECORD + 4, param0 as u32);
            self.w32(EVENT_RECORD + 8, param1 as u32);
            self.w32(EVENT_RECORD + 12, 0);
            self.w32(EVENT_RECORD + 16, 0);
            return self.call_c_function(1, EVENT_RECORD, 20, DEFAULT_INSTRUCTION_LIMIT, screen);
        }
        Ok(1)
    }

    /// Fires one due timer. Ext apps receive the platform timer through
    /// helper message 2 (`mrc_timerTimeout`); the SDK runtime stored the
    /// guest callback when `mrc_timerStart` ran. Script-level apps fall
    /// back to a key-style timer event.
    pub fn fire_timer(&mut self, screen: &mut crate::display::Screen) -> Result<i32, MrpError> {
        log::debug!(
            "fire_timer: cfn={:#x} timers={}",
            self.c_function,
            self.timers.len()
        );
        // SDK wrappers can initialize resources on their first timer tick.
        if self.c_function != 0 {
            return self.call_c_function(2, 0, 1, BOOT_INSTRUCTION_LIMIT, screen);
        }
        if self.timer_function != 0 {
            return self
                .run_guest(self.timer_function, BOOT_INSTRUCTION_LIMIT, screen)
                .map(|value| value as i32);
        }
        self.send_event(1, 0, 0, screen)
    }
}

impl Guest {
    /// LCG random source shared with the script's `GetRand`.
    pub fn rand_next(&mut self) -> u32 {
        self.rand_state = self
            .rand_state
            .wrapping_mul(1_103_515_245)
            .wrapping_add(12_345);
        (self.rand_state >> 16) & 0x7FFF
    }

    /// (IMEI, IMSI, vendor, device) strings for `GetSysInfo`.
    pub fn sysinfo_strings(&self) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
        (
            self.config.imei.clone().into_bytes(),
            self.config.imsi.clone().into_bytes(),
            self.config.vendor.clone().into_bytes(),
            self.config.device.clone().into_bytes(),
        )
    }

    pub fn timer_start(&mut self, id: i32, interval_ms: u32) {
        let interval = std::time::Duration::from_millis(interval_ms.max(1) as u64);
        let next_fire = self.clock_now() + interval;
        if let Some(timer) = self.timers.iter_mut().find(|timer| timer.id == id) {
            timer.interval = interval;
            timer.next_fire = next_fire;
        } else {
            self.timers.push(GuestTimer {
                id,
                interval,
                next_fire,
            });
        }
        // Mirror MR_TIMER_STATE_RUNNING so helper-side paths unblock.
        let cell = self.internal_state_cell(5);
        self.w32(cell, 1);
    }

    pub fn timer_stop(&mut self, id: i32) {
        self.timers.retain(|timer| timer.id != id);
        let cell = self.internal_state_cell(5);
        self.w32(cell, 0);
    }

    pub fn draw_point(&mut self, x: u32, y: u32, color: u32) {
        self.screen_put(x as i32, y as i32, rgb_to_565(color));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_rect(&mut self, x1: u32, y1: u32, x2: u32, y2: u32, r: u32, g: u32, b: u32) {
        let Some(base) = self.framebuffer_address() else {
            return;
        };
        let color = rgb888_to_565(r, g, b);
        let width = u32::from(self.config.screen_width);
        let height = u32::from(self.config.screen_height);
        for y in y1.min(height)..y2.min(height) {
            for x in x1.min(width)..x2.min(width) {
                self.w16(base + (y * width + x) * 2, color);
            }
        }
    }

    pub fn fill_screenbuf(&mut self, color: u16) {
        let Some(base) = self.framebuffer_address() else {
            return;
        };
        let width = u32::from(self.config.screen_width);
        let height = u32::from(self.config.screen_height);
        for y in 0..height {
            for x in 0..width {
                self.w16(base + (y * width + x) * 2, color);
            }
        }
    }

    pub fn screen_width(&self) -> u16 {
        self.config.screen_width
    }

    pub fn screen_height(&self) -> u16 {
        self.config.screen_height
    }

    /// Renders the active platform menu into the screen buffer: a blue
    /// title bar, white background, and items with the current selection
    /// drawn in inverse video.
    pub fn render_menu(&mut self) {
        self.uses_explicit_video = false;
        let Some(base) = self.framebuffer_address() else {
            return;
        };
        let width = u32::from(self.config.screen_width);
        let height = u32::from(self.config.screen_height);
        for y in 0..height {
            for x in 0..width {
                let color = if y < 20 { 0x001F } else { 0xFFFF };
                self.w16(base + (y * width + x) * 2, color);
            }
        }
        let Some(menu) = self.menu.clone() else {
            return;
        };
        self.draw_text(&menu.title, 4, 2, 0xFFFF);
        for (index, item) in menu.items.iter().enumerate() {
            let row_y = 24 + index as i32 * 18;
            if row_y + 16 >= height as i32 {
                break;
            }
            let selected = index == menu.selection;
            if selected {
                for y in 0..16u32 {
                    for x in 0..width {
                        let py = (row_y as u32 + y) * width + x;
                        self.w16(base + py * 2, 0x001F);
                    }
                }
            }
            let color = if selected { 0xFFFF } else { 0x0000 };
            self.draw_text(item, 12, row_y, color);
        }
        if menu.selection < menu.items.len() {
            self.draw_text(b">", 2, 24 + menu.selection as i32 * 18, 0x03E0);
        }
    }

    /// Moves the menu selection by `delta` (wrapping) and re-renders.
    pub fn move_menu_selection(&mut self, delta: i32) {
        if let Some(menu) = self.menu.as_mut() {
            let count = menu.count.max(menu.items.len()).max(1);
            let next = (menu.selection as i32 + delta).rem_euclid(count as i32) as usize;
            menu.selection = next;
        }
        self.render_menu();
    }

    /// Decodes GBK text and renders it into the screen buffer at (x, y)
    /// using the embedded Unicode bitmap font.
    pub fn draw_text(&mut self, text: &[u8], x: i32, y: i32, color: u16) {
        let (decoded, _, _) = encoding_rs::GBK.decode(text);
        self.draw_decoded_text(&decoded, x, y, color);
    }

    fn draw_decoded_text(&mut self, text: &str, x: i32, y: i32, color: u16) {
        let mut cursor_x = x;
        for ch in text.chars() {
            let Some(glyph) = unifont::get_glyph(ch) else {
                cursor_x = cursor_x.saturating_add(8);
                continue;
            };
            self.draw_glyph(glyph, cursor_x, y, color, i32::MAX, i32::MAX);
            cursor_x = cursor_x.saturating_add(glyph.get_width() as i32);
        }
    }

    fn draw_glyph(
        &mut self,
        glyph: &unifont::Glyph,
        x: i32,
        y: i32,
        color: u16,
        right: i32,
        bottom: i32,
    ) {
        for row in 0..16 {
            let dy = y.saturating_add(row);
            if dy >= bottom {
                break;
            }
            for col in 0..glyph.get_width() {
                let dx = x.saturating_add(col as i32);
                if dx >= right {
                    break;
                }
                if glyph.get_pixel(col, row as usize) {
                    self.screen_put(dx, dy, color);
                }
            }
        }
    }

    fn read_platform_text(&self, address: u32, unicode: bool) -> String {
        if unicode {
            let bytes = self.mem.get(address as usize..).unwrap_or_default();
            let units = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .take(512)
                .map(|bytes| u16::from_be_bytes(*bytes))
                .take_while(|&unit| unit != 0);
            char::decode_utf16(units)
                .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect()
        } else {
            let bytes = self.guest_cstring(address, 1024);
            encoding_rs::GBK.decode(&bytes).0.into_owned()
        }
    }

    fn draw_text_in_rect(&mut self) -> i32 {
        let mode = self.cpu.mode();
        let x = i32::from(self.cpu.reg_get(mode, R1) as i16);
        let y = i32::from(self.cpu.reg_get(mode, R2) as i16);
        let size = self.stack_arg(0);
        let right = x + i32::from(size as u16);
        let bottom = y + i32::from((size >> 16) as u16);
        let color = self.stack_arg(1).to_le_bytes();
        let color = rgb888_to_565(color[0].into(), color[1].into(), color[2].into());
        let flags = self.stack_arg(2);
        let wrap = flags & 2 != 0;
        let text = self.read_platform_text(self.cpu.reg_get(mode, R0), flags & 1 != 0);
        let (mut cursor_x, mut cursor_y) = (x, y);
        let mut consumed = 0;
        for ch in text.chars() {
            let glyph = unifont::get_glyph(ch);
            let width = glyph.map_or(8, |glyph| glyph.get_width() as i32);
            if wrap && (ch == '\n' || cursor_x + width > right) {
                cursor_x = x;
                cursor_y += 18;
            }
            if cursor_y >= bottom || (!wrap && (ch == '\n' || cursor_x + width > right)) {
                break;
            }
            if !matches!(ch, '\n' | '\r') {
                if let Some(glyph) = glyph {
                    self.draw_glyph(glyph, cursor_x, cursor_y, color, right, bottom);
                }
                cursor_x += width;
            }
            consumed += (ch.len_utf16() * 2) as i32;
        }
        consumed
    }

    /// Blits an RGB565 bitmap to the screen buffer at (x, y), with an
    /// optional transparency color (mr_transBitmapDraw uses this).
    pub fn blit_bitmap(
        &mut self,
        pixels: &[u16],
        width: u32,
        x: u32,
        y: u32,
        transparent: Option<u16>,
    ) {
        let Some(base) = self.framebuffer_address() else {
            return;
        };
        let s_width = u32::from(self.config.screen_width);
        let s_height = u32::from(self.config.screen_height);
        let bitmap_width = width;
        for (index, pixel) in pixels.iter().enumerate() {
            if let Some(color) = transparent {
                if *pixel == color {
                    continue;
                }
            }
            let col = index as u32 % bitmap_width;
            let row = index as u32 / bitmap_width;
            let px = x + col;
            let py = y + row;
            if px < s_width && py < s_height {
                self.w16(base + (py * s_width + px) * 2, *pixel);
            }
        }
    }

    /// Copies `data` into a guest scratch area and returns its address.
    pub fn scratch_buffer(&mut self, data: &[u8]) -> u32 {
        let address = LG_MEM_BASE + LG_MEM_LEN - 4096;
        let base = address as usize;
        let count = data.len().min(4095);
        self.mem[base..base + count].copy_from_slice(&data[..count]);
        self.mem[base + count] = 0;
        address
    }

    /// Opens a package/VFS file, returning a 1-based handle.
    pub fn file_open(&mut self, name: &str) -> Option<usize> {
        let name = self.resolve_storage_path(name);
        self.open_file(&name, true, true, false)
    }

    fn open_file(
        &mut self,
        name: &str,
        readable: bool,
        writable: bool,
        create: bool,
    ) -> Option<usize> {
        if name.is_empty() || self.directories.contains(name) {
            return None;
        }
        if !create {
            self.ensure_system_font(name);
        }
        let file_index = match self.find_file(name, !create) {
            Some(index) => index,
            None if create => {
                self.files.push((name.to_string(), Vec::new()));
                self.dirty_files.insert(name.to_string());
                self.files.len() - 1
            }
            None => return None,
        };
        self.open_files.push(OpenFile {
            file_index,
            pos: 0,
            readable,
            writable,
            closed: false,
        });
        Some(self.open_files.len())
    }

    pub fn file_read(&mut self, handle: usize, len: usize) -> Option<Vec<u8>> {
        let file = self.open_files.get_mut(handle.wrapping_sub(1))?;
        if file.closed || !file.readable {
            return None;
        }
        let data = &self.files[file.file_index].1;
        let start = file.pos.min(data.len());
        let remaining = data.len() - start;
        let count = len.min(remaining);
        let bytes = data[start..start + count].to_vec();
        file.pos += count;
        Some(bytes)
    }

    pub fn file_close(&mut self, handle: usize) {
        self.close_file(handle);
    }

    fn close_file(&mut self, handle: usize) -> bool {
        let Some(file) = self.open_files.get_mut(handle.wrapping_sub(1)) else {
            return false;
        };
        if file.closed {
            return false;
        }
        file.closed = true;
        true
    }

    pub fn file_len(&self, handle: usize) -> Option<usize> {
        let file = self.open_files.get(handle.wrapping_sub(1))?;
        (!file.closed).then(|| self.files[file.file_index].1.len())
    }

    /// Writes bytes at the current position; returns whether it landed.
    pub fn file_write(&mut self, handle: usize, data: &[u8]) -> bool {
        let Some(file) = self.open_files.get_mut(handle.wrapping_sub(1)) else {
            return false;
        };
        if file.closed || !file.writable {
            return false;
        }
        if data.is_empty() {
            return true;
        }
        let end = file.pos.saturating_add(data.len());
        if end > MEM_SIZE as usize {
            return false;
        }
        let (name, bytes) = &mut self.files[file.file_index];
        if end > bytes.len() {
            bytes.resize(end, 0);
        }
        bytes[file.pos..end].copy_from_slice(data);
        file.pos = end;
        self.dirty_files.insert(name.clone());
        true
    }

    /// Seeks like `mr_seek`: whence 0 = set, 1 = current, 2 = end. Returns
    /// the resulting position (MR_FAILED as -1).
    pub fn file_seek(&mut self, handle: usize, offset: i32, whence: i32) -> i32 {
        let Some(file) = self.open_files.get_mut(handle.wrapping_sub(1)) else {
            return -1;
        };
        if file.closed {
            return -1;
        }
        let len = self.files[file.file_index].1.len() as i64;
        let base = match whence {
            0 => 0,
            1 => file.pos as i64,
            _ => len,
        };
        let target = (base + i64::from(offset)).clamp(0, len);
        file.pos = target as usize;
        file.pos as i32
    }

    /// Persistent buffer for `mr_getCharBitmap` glyph exports: 32 rows
    /// × 2 bytes covers a 16-pixel-wide, 32-row glyph at 1 bit per pixel.
    fn glyph_buffer(&mut self) -> u32 {
        if self.glyph_buf == 0 {
            self.glyph_buf = self.heap_malloc(64);
        }
        self.glyph_buf
    }

    /// Milliseconds since the guest engine started (`mr_getTime` base).
    pub fn uptime_ms(&self) -> u32 {
        self.clock_now().duration_since(self.started_at).as_millis() as u32
    }

    pub(crate) fn set_frame_time(&mut self, elapsed: Option<Duration>) {
        self.frame_time = elapsed;
    }

    pub(crate) fn clock_now(&self) -> Instant {
        self.frame_time
            .and_then(|elapsed| self.started_at.checked_add(elapsed))
            .unwrap_or_else(Instant::now)
            .checked_add(self.sleep_elapsed)
            .unwrap_or(self.started_at)
    }

    /// Names of files the guest created or modified (not package members).
    pub fn dirty_file_names(&self) -> Vec<String> {
        self.dirty_files.iter().cloned().collect()
    }

    /// Reads a file's current content from the file table.
    pub fn file_data(&self, name: &str) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|(entry, _)| entry == name)
            .map(|(_, data)| data.as_slice())
    }

    /// Writes all dirty files (guest-created/modified, not package
    /// members) into `dir`. Creates the directory if needed.
    pub fn flush_persistence(&self, dir: &std::path::Path) -> std::io::Result<usize> {
        std::fs::create_dir_all(dir)?;
        let mut count = 0usize;
        for name in &self.dirty_files {
            // Normalize separators to avoid directory escapes.
            let safe = name.replace('\\', "/");
            let safe = safe.trim_start_matches('/');
            if safe.is_empty() || safe.contains("..") {
                continue;
            }
            let Some(data) = self
                .files
                .iter()
                .find(|(entry, _)| entry == name)
                .map(|(_, d)| d)
            else {
                continue;
            };
            let path = dir.join(safe);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, data)?;
            count += 1;
        }
        Ok(count)
    }

    /// Loads files from `dir` into the guest file table (overriding any
    /// same-named entries). Mirrors [`Guest::flush_persistence`]: the
    /// directory tree maps onto guest paths with `/` separators, so files
    /// written to nested host directories restore under their original
    /// guest names. Returns the number of files loaded.
    pub fn load_persistence(&mut self, dir: &std::path::Path) -> std::io::Result<usize> {
        let mut count = 0usize;
        Self::walk_persistence(self, dir, "", &mut count);
        Ok(count)
    }

    fn walk_persistence(guest: &mut Guest, dir: &std::path::Path, prefix: &str, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.is_empty() || name.contains("..") {
                continue;
            }
            let guest_path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if path.is_dir() {
                // Register every ancestor segment so guest lookups see the
                // same directory layout they wrote.
                let mut segment = String::new();
                for part in guest_path.split('/') {
                    if !segment.is_empty() {
                        segment.push('/');
                    }
                    segment.push_str(part);
                    guest.directories.insert(segment.clone());
                }
                Self::walk_persistence(guest, &path, &guest_path, count);
                continue;
            }
            let Ok(data) = std::fs::read(&path) else {
                continue;
            };
            // The guest may hold the same file under backslash separators;
            // update that twin first so existing handles stay coherent.
            let twin = guest_path.replace('/', "\\");
            let existing = guest
                .files
                .iter_mut()
                .find(|(entry, _)| *entry == guest_path || *entry == twin);
            if let Some(existing) = existing {
                existing.1 = data;
            } else {
                guest.files.push((guest_path.clone(), data));
            }
            *count += 1;
        }
    }

    fn framebuffer_address(&self) -> Option<u32> {
        let base = self.get_data(DataSlot::ScreenBufPtr);
        let size = u64::from(self.config.screen_width) * u64::from(self.config.screen_height) * 2;
        (base != 0 && u64::from(base) + size <= self.mem.len() as u64).then_some(base)
    }

    /// Screen-buffer plot with clipping.
    fn screen_put(&mut self, x: i32, y: i32, color: u16) {
        let width = u32::from(self.config.screen_width);
        let height = u32::from(self.config.screen_height);
        if x >= 0 && y >= 0 && (x as u32) < width && (y as u32) < height {
            let Some(base) = self.framebuffer_address() else {
                return;
            };
            let (x, y) = (x as u32, y as u32);
            self.w16(base + (y * width + x) * 2, color);
        }
    }

    /// Reads a guest byte range (bounds-safe).
    pub fn guest_bytes(&self, addr: u32, len: usize) -> Option<Vec<u8>> {
        let base = addr as usize;
        self.mem
            .get(base..base.checked_add(len)?)
            .map(<[u8]>::to_vec)
    }

    /// The platform `mr_drawBitmap`: a 16bpp blit with the bitmap raster
    /// operations (OR/XOR/COPY/NOT/MERGENOT/ANDNOT/TRANSPARENT/AND/GRAY/
    /// REVERSE) and, above `MR_SPRITE_TRANSPARENT`, the tile path with
    /// rotate/flip bits. Mirrors the reference `_DrawBitmap`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_bitmap_rop(
        &mut self,
        p: u32,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        rop: u32,
        transcolor: u16,
        sx: i32,
        sy: i32,
        mw: u32,
    ) {
        let Some(base) = self.framebuffer_address() else {
            return;
        };
        let screen_w = u32::from(self.config.screen_width) as i32;
        let screen_h = u32::from(self.config.screen_height) as i32;
        let (w, h) = (w as i32, h as i32);
        let (min_x, min_y) = (x.max(0), y.max(0));
        let (max_x, max_y) = (screen_w.min(x + w), screen_h.min(y + h));
        if min_x >= max_x || min_y >= max_y {
            return;
        }
        let sprite_path = rop > 0x400;
        let bitmap_rop = rop & 0x3FF;
        let rotate = (rop >> 11) & 0x3;
        let flip = (rop >> 11) & 0x4 != 0;
        let bitmap_stride = if mw == 0 { w } else { mw as i32 };
        let screen_stride = u32::from(self.config.screen_width);
        let dst = |x: u32, y: u32| base + (y * screen_stride + x) * 2;
        for dy in min_y..max_y {
            for dx in min_x..max_x {
                let lx = dx - x;
                let ly = dy - y;
                let (row, col, stride) = if sprite_path {
                    let (row, col) = match (bitmap_rop, rotate, flip) {
                        (2, 0, true) => (h - 1 - ly, lx),
                        (2, 1, false) => (lx, w - 1 - ly),
                        (2, 1, true) => (h - 1 - lx, w - 1 - ly),
                        (2, 2, false) => (h - 1 - ly, w - 1 - lx),
                        (2, 2, true) => (ly, w - 1 - lx),
                        (2, 3, false) => (h - 1 - lx, ly),
                        (2, 3, true) => (lx, ly),
                        _ => (ly, lx),
                    };
                    (row, col, w)
                } else {
                    (ly + sy, lx + sx, bitmap_stride)
                };
                if row < 0 || col < 0 || col >= stride {
                    continue;
                }
                let offset = (i64::from(row) * i64::from(stride) + i64::from(col)) * 2;
                let Ok(src) = u32::try_from(i64::from(p) + offset) else {
                    continue;
                };
                let value = self.r16(src);
                let under = self.r16(dst(dx as u32, dy as u32));
                let next = match (sprite_path, bitmap_rop) {
                    (_, 2) | (_, 6) => {
                        // BM_COPY / BM_TRANSPARENT (with the wrapper color).
                        if bitmap_rop == 6 && value == transcolor {
                            None
                        } else {
                            Some(value)
                        }
                    }
                    (_, 0) => Some(under | value),  // BM_OR
                    (_, 1) => Some(under ^ value),  // BM_XOR
                    (_, 3) => Some(!value),         // BM_NOT
                    (_, 4) => Some(under | !value), // BM_MERGENOT
                    (_, 5) => Some(under & !value), // BM_ANDNOT
                    (_, 7) => Some(under & value),  // BM_AND
                    (_, 8) => {
                        // BM_GRAY: luma to an equal-channel RGB565 gray.
                        let red = ((value >> 11) & 0x1F) as u32;
                        let green = ((value >> 5) & 0x3F) as u32;
                        let blue = (value & 0x1F) as u32;
                        let luma = (red * 8 * 299 + green * 4 * 587 + blue * 8 * 114) / 2000;
                        let gray = luma.min(31) as u16;
                        Some(gray << 11 | gray << 6 | gray)
                    }
                    (_, 9) => Some(!value), // BM_REVERSE
                    _ => Some(value),
                };
                if let Some(value) = next {
                    self.screen_put(dx, dy, value);
                }
            }
        }
    }

    /// The platform `mr_drawBitmapEx`: affine blit between two bitmap
    /// descriptors (`MrBitmapDraw{p,w,h,x,y}`) through a 2x2 matrix
    /// (`MrTransMatrix{a,b,c,d,rop}`), 8.8 fixed point per the reference.
    pub fn draw_bitmap_ex(
        &mut self,
        srcbmp_ptr: u32,
        dstbmp_ptr: u32,
        w: u32,
        h: u32,
        p_trans: u32,
        transcolor: u16,
    ) {
        let read_bitmap = |guest: &Self, addr: u32| -> (u32, u32, u32, u32, u32) {
            // {p:u32, w:u16, h:u16, x:u16, y:u16} -> (p, w, h, x, y)
            (
                guest.r32(addr),
                guest.r16(addr + 4) as u32,
                guest.r16(addr + 6) as u32,
                guest.r16(addr + 8) as u32,
                guest.r16(addr + 10) as u32,
            )
        };
        let (src_p, src_w, _src_h, src_x, src_y) = read_bitmap(self, srcbmp_ptr);
        let (dst_p, dst_w, dst_h, dst_x, dst_y) = read_bitmap(self, dstbmp_ptr);
        if src_p == 0 || dst_p == 0 || src_w == 0 || dst_w == 0 || w == 0 || h == 0 {
            return;
        }
        let a = i32::from(self.r16(p_trans) as i16);
        let b = i32::from(self.r16(p_trans + 2) as i16);
        let c = i32::from(self.r16(p_trans + 4) as i16);
        let d = i32::from(self.r16(p_trans + 6) as i16);
        let rop = self.r16(p_trans + 8);
        let determinant = a * d - b * c;
        if determinant == 0 {
            return;
        }
        let (w, h) = (w as i32, h as i32);
        let center_x = dst_x as i32 + w / 2;
        let center_y = dst_y as i32 + h / 2;
        let max_y = ((c.abs() * w + d.abs() * h) >> 9).min(dst_h as i32 - center_y);
        let min_y = 0i32.max(-(center_y));
        for dy in min_y..max_y {
            let half_w_det = (w * determinant) >> 9;
            let half_h_det = (h * determinant) >> 9;
            let (max_by_d, min_by_d) = if d == 0 {
                (999, -999)
            } else {
                (
                    ((half_w_det + b * dy) / d).max((b * dy - half_w_det) / d),
                    ((b * dy - half_w_det) / d).min((half_w_det + b * dy) / d),
                )
            };
            let (max_by_c, min_by_c) = if c == 0 {
                (999, -999)
            } else {
                (
                    ((a * dy + half_h_det) / c).max((a * dy - half_h_det) / c),
                    ((a * dy - half_h_det) / c).min((a * dy + half_h_det) / c),
                )
            };
            let max_x = max_by_d.min(max_by_c).min(dst_w as i32 - center_x);
            let min_x = min_by_d.min(min_by_c).max(-center_x);
            if min_x >= max_x {
                continue;
            }
            for dx in min_x..max_x {
                let offset_y = ((((a * dy - c * dx) as i64) << 8) / i64::from(determinant)
                    + i64::from(h / 2)) as i32;
                let offset_x = ((((d * dx - b * dy) as i64) << 8) / i64::from(determinant)
                    + i64::from(w / 2)) as i32;
                if offset_y < 0 || offset_y >= h || offset_x < 0 || offset_x >= w {
                    continue;
                }
                let src = src_p
                    + (((offset_y + src_y as i32) * src_w as i32 + offset_x + src_x as i32) as u32)
                        * 2;
                let value = self.r16(src);
                if rop & 0x3FF == 6 && value == transcolor {
                    continue;
                }
                let dst = dst_p + (((dy + center_y) * dst_w as i32 + dx + center_x) as u32) * 2;
                self.w16(dst, value);
            }
        }
    }

    /// The platform `mr_transbitmapDraw`: blit from an `MrTransBitmap`
    /// (run-length matrix of opaque spans) into `dstBuf` with clipping,
    /// mirroring the reference implementation.
    #[allow(clippy::too_many_arguments)]
    pub fn transbitmap_draw(
        &mut self,
        h_trans: u32,
        dst_buf: u32,
        dest_max_w: i32,
        dest_max_h: i32,
        sx: i32,
        sy: i32,
        width: i32,
        height: i32,
        dx: i32,
        dy: i32,
    ) {
        // MrTransBitmap { p_data, width, height, max_width,
        // trans_color(u16 + pad), p_matrix, mark_count, p_zion }
        if h_trans == 0 || dst_buf == 0 {
            return;
        }
        let p_data = self.r32(h_trans);
        let max_width = self.r32(h_trans + 12) as i32;
        let p_matrix = self.r32(h_trans + 20);
        let mark_count = self.r32(h_trans + 24) as i32;
        if p_data == 0
            || p_matrix == 0
            || dest_max_w <= 0
            || dest_max_h <= 0
            || max_width <= 0
            || mark_count <= 0
        {
            return;
        }
        let (mut sx, mut sy, mut width, mut height, mut dx, mut dy) =
            (sx, sy, width, height, dx, dy);
        if dx < 0 {
            sx -= dx;
            width += dx;
            dx = 0;
        }
        if dy < 0 {
            sy -= dy;
            height += dy;
            dy = 0;
        }
        if sx < 0 {
            width += sx;
            dx -= sx;
            sx = 0;
        }
        if sy < 0 {
            height += sy;
            dy -= sy;
            sy = 0;
        }
        width = width.min(dest_max_w - dx);
        height = height.min(dest_max_h - dy);
        if width <= 0 || height <= 0 || dx >= dest_max_w || dy >= dest_max_h {
            return;
        }
        for row in 0..height {
            let matrix = p_matrix + ((sy + row) * 2 * mark_count) as u32;
            let src_line = p_data + (max_width * (sy + row)) as u32;
            let dst_line = dst_buf + (dest_max_w * (dy + row) + dx) as u32 * 2;
            let pos1_e = sx + width;
            for mark in 0..mark_count {
                let start = i16::try_from(self.r16(matrix + (mark * 2) as u32)).unwrap_or(0) as i32;
                if start < 0 {
                    break;
                }
                let mut len =
                    i16::try_from(self.r16(matrix + (mark * 2 + 2) as u32)).unwrap_or(0) as i32;
                if mark == mark_count - 1 {
                    // The last mark's top bit chains into the next row.
                    len &= 0x3FFF;
                }
                let pos2_e = start + len;
                if pos2_e < start || pos1_e < sx {
                    continue;
                }
                let start = start.max(sx);
                let end = pos2_e.min(pos1_e);
                for offset in 0..(end - start) {
                    let value = self.r16(src_line + (start + offset) as u32 * 2);
                    self.w16(dst_line + (start - sx + offset) as u32 * 2, value);
                }
            }
        }
    }

    /// Bresenham line into the screen buffer (clipped per pixel).
    pub fn draw_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, color: u16) {
        let Some(base) = self.framebuffer_address() else {
            return;
        };
        let width = u32::from(self.config.screen_width);
        let height = u32::from(self.config.screen_height);
        let plot = |guest: &mut Self, x: i32, y: i32| {
            if x >= 0 && y >= 0 && (x as u32) < width && (y as u32) < height {
                let (x, y) = (x as u32, y as u32);
                guest.w16(base + (y * width + x) * 2, color);
            }
        };
        let dx = (x2 - x1).abs();
        let dy = -(y2 - y1).abs();
        let (mut x, mut y) = (x1, y1);
        let step_x = if x1 < x2 { 1 } else { -1 };
        let step_y = if y1 < y2 { 1 } else { -1 };
        let mut error = dx + dy;
        loop {
            plot(self, x, y);
            if x == x2 && y == y2 {
                break;
            }
            let doubled = error * 2;
            if doubled >= dy {
                error += dy;
                x += step_x;
            }
            if doubled <= dx {
                error += dx;
                y += step_y;
            }
        }
    }
}

/// Value-range write watch: captures guest word stores whose value falls
/// in `[lo, hi)`. Used to trace which store clobbers an ext's dispatch
/// cells with pointers into its own function-pointer tables.
#[derive(Default)]
pub struct ValueWatch {
    pub lo: u32,
    pub hi: u32,
    /// Exact addresses to watch for any write.
    pub addrs: Vec<u32>,
    /// `(address, value)` pairs, oldest first; drained by the step loop.
    pub hits: Vec<(u32, u32)>,
}

impl ValueWatch {
    fn note(&mut self, addr: u32, val: u32) {
        let value_hit = (self.lo != 0 || self.hi != 0) && val >= self.lo && val < self.hi;
        let addr_hit = self.addrs.contains(&addr);
        if value_hit || addr_hit {
            self.hits.push((addr, val));
        }
    }
}

/// Adapter implementing `armv4t_emu::Memory` over the guest memory vector.
struct GuestMem<'a>(&'a mut Vec<u8>, &'a mut ValueWatch);

impl Memory for GuestMem<'_> {
    fn r8(&mut self, addr: u32) -> u8 {
        self.0.get(addr as usize).copied().unwrap_or(0)
    }

    fn r16(&mut self, addr: u32) -> u16 {
        u16::from_le_bytes([self.r8(addr), self.r8(addr.wrapping_add(1))])
    }

    fn r32(&mut self, addr: u32) -> u32 {
        u32::from_le_bytes([
            self.r8(addr),
            self.r8(addr.wrapping_add(1)),
            self.r8(addr.wrapping_add(2)),
            self.r8(addr.wrapping_add(3)),
        ])
    }

    fn w8(&mut self, addr: u32, val: u8) {
        if let Some(cell) = self.0.get_mut(addr as usize) {
            *cell = val;
        }
    }

    fn w16(&mut self, addr: u32, val: u16) {
        self.w8(addr, val as u8);
        self.w8(addr.wrapping_add(1), (val >> 8) as u8);
    }

    fn w32(&mut self, addr: u32, val: u32) {
        self.1.note(addr, val);
        self.w8(addr, val as u8);
        self.w8(addr.wrapping_add(1), (val >> 8) as u8);
        self.w8(addr.wrapping_add(2), (val >> 16) as u8);
        self.w8(addr.wrapping_add(3), (val >> 24) as u8);
    }
}

/// Extracts the next conversion byte from the format, skipping literals.
/// Logs each distinct diagnostic key once, so chatty guest calls do not
/// flood the log.
fn warn_once(key: &str, message: String) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let mut guard = SEEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let seen = guard.get_or_insert_with(HashSet::new);
    if seen.insert(key.to_string()) {
        log::warn!("{message}");
    }
}

fn trimmed_number(text: &str) -> &str {
    text.trim()
}

/// Packs 24-bit RGB (as stored in a u32 by the guest) into RGB565.
fn rgb_to_565(value: u32) -> u16 {
    let r = (value >> 16) & 0xFF;
    let g = (value >> 8) & 0xFF;
    let b = value & 0xFF;
    rgb888_to_565(r, g, b)
}

fn ram_entry_range(package: &[u8], name: &[u8]) -> Option<std::ops::Range<usize>> {
    fn take_u32(bytes: &[u8], start: usize) -> Option<usize> {
        Some(u32::from_le_bytes(bytes.get(start..)?.first_chunk::<4>().copied()?) as usize)
    }

    let mut index = 8usize.checked_add(take_u32(package, 4)?)?;
    while index < package.len() {
        let name_len = take_u32(package, index)?;
        if !(1..=512).contains(&name_len) {
            return None;
        }
        let name_start = index + 4;
        let name_end = name_start + name_len;
        let entry_name = package.get(name_start..name_end)?;
        let data_len = take_u32(package, name_end)?;
        let data_start = name_end + 4;
        let data_end = data_start.checked_add(data_len)?;
        if data_end > package.len() {
            return None;
        }
        let entry_name = entry_name.split(|&byte| byte == 0).next()?;
        if entry_name == name {
            return Some(data_start..data_end);
        }
        index = data_end;
    }
    None
}

fn normalize_file_path(path: &str) -> String {
    path.split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

pub fn rgb888_to_565(r: u32, g: u32, b: u32) -> u16 {
    ((r >> 3) as u16) << 11 | ((g >> 2) as u16) << 5 | (b >> 3) as u16
}

/// Current local time as (year, month, day, hour, minute, second); the
/// guest datetime comes from the host clock.
#[allow(clippy::type_complexity)]
pub fn local_datetime() -> (u32, u32, u32, u32, u32, u32) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-from-days algorithm (Howard Hinnant) without external crates.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (
        year as u32,
        month as u32,
        day as u32,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_play_sound_routes_midi_and_wav_by_sdk_type() {
        use crate::audio::{AudioSink, PcmClip};
        use std::{cell::RefCell, rc::Rc};

        #[derive(Default)]
        struct Calls {
            midi: Option<(Vec<u8>, bool)>,
            wav: Option<(PcmClip, bool)>,
            stopped: bool,
        }

        struct RecordingSink(Rc<RefCell<Calls>>);

        impl AudioSink for RecordingSink {
            fn play_midi(&mut self, data: &[u8], looped: bool) {
                self.0.borrow_mut().midi = Some((data.to_vec(), looped));
            }

            fn play_bgm(&mut self, clip: PcmClip, looped: bool) {
                self.0.borrow_mut().wav = Some((clip, looped));
            }

            fn play_effect(&mut self, _clip: PcmClip) {}

            fn stop_bgm(&mut self) {
                self.0.borrow_mut().stopped = true;
            }
        }

        let midi =
            b"MThd\0\0\0\x06\0\0\0\x01\0\x60MTrk\0\0\0\x0c\0\x90\x3c\x64\x60\x80\x3c\0\0\xff\x2f\0";
        let wav = b"RIFF\x26\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0data\x02\0\0\0\x34\x12";
        let calls = Rc::new(RefCell::new(Calls::default()));
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        guest.audio_sink = Box::new(RecordingSink(Rc::clone(&calls)));
        let mut screen = crate::display::Screen::new(240, 320);
        let mode = guest.cpu.mode();

        for (sound_type, data, looped) in [(0, midi.as_slice(), 1), (1, wav.as_slice(), 0)] {
            let address = guest.scratch_buffer(data);
            guest.cpu.reg_set(mode, R0, sound_type);
            guest.cpu.reg_set(mode, R1, address);
            guest.cpu.reg_set(mode, R2, data.len() as u32);
            guest.cpu.reg_set(mode, R3, looped);
            assert_eq!(guest.dispatch(57, &mut screen), 0);
        }
        assert_eq!(guest.dispatch(58, &mut screen), 0);

        let calls = calls.borrow();
        assert_eq!(calls.midi, Some((midi.to_vec(), true)));
        assert_eq!(
            calls.wav,
            Some((
                PcmClip {
                    sample_rate: 8_000,
                    channels: 1,
                    samples: vec![0x1234]
                },
                false
            ))
        );
        assert!(calls.stopped);
    }

    #[test]
    fn native_mp3_sound_plays_loops_and_stops() {
        use crate::audio::AudioPlayer;

        // A synthetic mono 1 kHz tone encoded at 16 kHz with an ID3 tag.
        let mp3 = include_bytes!("../tests/fixtures/tone.mp3");
        let player = AudioPlayer::new(48_000);
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        guest.audio_sink = Box::new(player.clone());
        let mut screen = crate::display::Screen::new(240, 320);
        let address = guest.scratch_buffer(mp3);
        let mode = guest.cpu.mode();
        let mut output = vec![[0.0; 2]; 48_000];

        for looped in [0, 1] {
            guest.cpu.reg_set(mode, R0, 2);
            guest.cpu.reg_set(mode, R1, address);
            guest.cpu.reg_set(mode, R2, mp3.len() as u32);
            guest.cpu.reg_set(mode, R3, looped);
            assert_eq!(guest.dispatch(57, &mut screen), 0);

            player.render(&mut output);
            assert!(output[..12_000].iter().any(|[left, _]| left.abs() > 0.01));
            assert!(output.iter().all(|[left, right]| left == right));
            assert_eq!(
                output[36_000..].iter().any(|[left, _]| left.abs() > 0.01),
                looped != 0
            );
        }

        assert_eq!(guest.dispatch(58, &mut screen), 0);
        player.render(&mut output);
        assert!(output.iter().all(|frame| *frame == [0.0; 2]));
    }

    #[test]
    fn native_amr_sound_renders_mono_as_stereo() {
        use crate::audio::AudioPlayer;
        use amr_nb::{rfc4867, Dtx, Encoder, Mode};

        let tone = std::array::from_fn(|i| {
            let phase = i as f32 * (std::f32::consts::TAU * 400.0 / 8_000.0);
            (phase.sin() * 8_000.0) as i16
        });
        let mut encoder = Encoder::new(Dtx::Off);
        let frames: Vec<_> = (0..4).map(|_| encoder.encode(Mode::MR122, &tone)).collect();
        let amr = rfc4867::write_storage(&frames).unwrap();

        let player = AudioPlayer::new(48_000);
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        guest.audio_sink = Box::new(player.clone());
        let mut screen = crate::display::Screen::new(240, 320);
        let address = guest.scratch_buffer(&amr);
        let mode = guest.cpu.mode();
        guest.cpu.reg_set(mode, R0, 5);
        guest.cpu.reg_set(mode, R1, address);
        guest.cpu.reg_set(mode, R2, amr.len() as u32);
        assert_eq!(guest.dispatch(57, &mut screen), 0);

        let mut output = vec![[0.0; 2]; 4_800];
        player.render(&mut output);
        assert!(output[..3_840].iter().any(|[left, _]| left.abs() > 0.01));
        assert!(output.iter().all(|[left, right]| left == right));
        assert!(output[3_841..].iter().all(|&frame| frame == [0.0; 2]));
    }

    #[test]
    fn native_asset_tables_have_separate_storage_and_a_screen_entry() {
        let config = GuestConfig {
            screen_width: 240,
            screen_height: 400,
            ..GuestConfig::default()
        };
        let mut guest = Guest::new(config, Vec::new());
        let bitmaps = guest.r32(TABLE_BASE + 95 * 4);
        let screen_bitmap = bitmaps + 30 * 16;
        assert_eq!(guest.r16(screen_bitmap), 240);
        assert_eq!(guest.r16(screen_bitmap + 2), 400);
        assert_eq!(guest.r32(screen_bitmap + 4), 240 * 400 * 2);
        assert_eq!(guest.r32(screen_bitmap + 12), SCREENBUF);

        for (offset, size) in [
            (95, 31 * 16),
            (96, 3 * 20),
            (97, 3 * 4),
            (98, 5 * 12),
            (99, 10 * 2),
        ] {
            let address = guest.r32(TABLE_BASE + offset * 4);
            guest.w8(address + size - 1, offset as u8);
        }
        assert_eq!(
            guest.r32(TABLE_BASE + 100 * 4),
            guest.data_slot_address(DataSlot::PackFilename)
        );
        assert_eq!(
            guest.guest_cstring(guest.r32(TABLE_BASE + 100 * 4), 128),
            b"app.mrp"
        );
        let tables: std::collections::HashSet<_> = (95..100)
            .map(|slot| guest.r32(TABLE_BASE + slot * 4))
            .collect();
        assert_eq!(tables.len(), 5);
    }

    #[test]
    fn native_text_conversion_returns_big_endian_ucs2_and_byte_lengths() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let mut screen = crate::display::Screen::new(240, 320);
        let mode = guest.cpu.mode();
        let err = STACK_TOP - 8;
        let size = STACK_TOP - 4;
        let source = guest.scratch_buffer(b"A\xd6\xd0");
        guest.cpu.reg_set(mode, R0, source);
        guest.cpu.reg_set(mode, R1, err);
        guest.cpu.reg_set(mode, R2, size);
        let encoded = guest.dispatch(132, &mut screen) as u32;
        assert_ne!(encoded, 0);
        assert_eq!(guest.r32(err), u32::MAX);
        assert_eq!(guest.r32(size), 6);
        assert_eq!(
            guest.guest_bytes(encoded, 6).unwrap(),
            [0, 0x41, 0x4e, 0x2d, 0, 0]
        );
        guest.cpu.reg_set(mode, R0, encoded);
        assert_eq!(guest.dispatch(126, &mut screen), 4);
    }

    #[test]
    fn native_read_file_follows_the_active_ram_package() {
        use std::io::Write;

        let mut guest = Guest::new(
            GuestConfig::default(),
            vec![("abc".to_string(), b"disk package".to_vec())],
        );
        let mut screen = crate::display::Screen::new(240, 320);
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(b"RAM package").unwrap();
        let payload = gzip.finish().unwrap();
        let mut package = b"MRPG\x04\x00\x00\x00".to_vec();
        package.extend_from_slice(&[0; 4]);
        package.extend_from_slice(&4u32.to_le_bytes());
        package.extend_from_slice(b"abc\0");
        package.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        let payload_offset = package.len() as u32;
        package.extend_from_slice(&payload);
        let address = guest.heap_malloc(package.len() as u32);
        guest.mem[address as usize..address as usize + package.len()].copy_from_slice(&package);
        let mode = guest.cpu.mode();
        guest.cpu.reg_set(mode, R1, 2);
        guest.cpu.reg_set(mode, R2, address);
        guest.cpu.reg_set(mode, R3, package.len() as u32);
        assert_eq!(guest.dispatch(131, &mut screen), 0);
        guest.store_guest_string(DataSlot::PackFilename, "$");

        let name = guest.scratch_buffer(b"abc");
        let length = STACK_TOP - 4;
        guest.cpu.reg_set(mode, R0, name);
        guest.cpu.reg_set(mode, R1, length);
        guest.cpu.reg_set(mode, R2, 1);
        assert_eq!(guest.dispatch(125, &mut screen), 1);
        guest.cpu.reg_set(mode, R2, 2);
        assert_eq!(
            guest.dispatch(125, &mut screen) as u32,
            address + payload_offset
        );
        assert_eq!(guest.r32(length), payload.len() as u32);
        guest.cpu.reg_set(mode, R2, 0);
        let unpacked = guest.dispatch(125, &mut screen) as u32;
        assert_eq!(
            guest
                .guest_bytes(unpacked, guest.r32(length) as usize)
                .unwrap(),
            b"RAM package"
        );
    }

    #[test]
    fn native_read_file_uses_the_selected_disk_archive() {
        let mut archive = vec![0; 240];
        let entry_start = archive.len();
        let bytes = b"inner file";
        let name = b"data\0";
        let end = entry_start + 4 + name.len() + 12;
        archive.extend_from_slice(&(name.len() as u32).to_le_bytes());
        archive.extend_from_slice(name);
        archive.extend_from_slice(&(end as u32).to_le_bytes());
        archive.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        archive.extend_from_slice(&[0; 4]);
        archive.extend_from_slice(bytes);
        let length = archive.len() as u32;
        archive[..4].copy_from_slice(b"MRPG");
        archive[4..8].copy_from_slice(&((end - 8) as u32).to_le_bytes());
        archive[8..12].copy_from_slice(&length.to_le_bytes());
        archive[12..16].copy_from_slice(&(entry_start as u32).to_le_bytes());

        let mut guest = Guest::new(
            GuestConfig::default(),
            vec![
                ("data".into(), b"outer file".to_vec()),
                ("plugins/demo.mrp".into(), archive),
            ],
        );
        guest.store_guest_string(DataSlot::PackFilename, "plugins/demo.mrp");
        let mut screen = crate::display::Screen::new(240, 320);
        let input = guest.scratch_buffer(b"data");
        let mode = guest.cpu.mode();
        let size = STACK_TOP - 4;
        guest.cpu.reg_set(mode, R0, input);
        guest.cpu.reg_set(mode, R1, size);
        guest.cpu.reg_set(mode, R2, 0);
        let output = guest.dispatch(125, &mut screen) as u32;
        assert_eq!(
            guest.guest_bytes(output, guest.r32(size) as usize).unwrap(),
            bytes
        );
    }

    #[test]
    fn native_string_comparison_distinguishes_resource_names_with_a_common_prefix() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let short = HEAP_BASE + 32;
        let long = HEAP_BASE + 64;
        guest.mem[short as usize..short as usize + 3].copy_from_slice(b"m\0z");
        guest.mem[long as usize..long as usize + 8].copy_from_slice(b"minimap\0");
        let mode = guest.cpu.mode();
        guest.cpu.reg_set(mode, R0, short);
        guest.cpu.reg_set(mode, R1, long);
        assert!(guest.dispatch_mem_string(10) < 0);
        guest.cpu.reg_set(mode, R2, 1);
        assert_eq!(guest.dispatch_mem_string(11), 0);
        guest.cpu.reg_set(mode, R2, 2);
        assert!(guest.dispatch_mem_string(11) < 0);
        guest.mem[long as usize..long as usize + 3].copy_from_slice(b"m\0a");
        guest.cpu.reg_set(mode, R2, 3);
        assert_eq!(guest.dispatch_mem_string(10), 0);
        assert!(guest.dispatch_mem_string(9) > 0);
    }

    #[test]
    fn native_platform_allocates_and_releases_requested_extended_ram() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let mut screen = crate::display::Screen::default();
        let mode = guest.cpu.mode();
        let output = STACK_TOP - 16;
        let length = output + 4;
        let requested = 10 * 1024 * 1024;
        guest.w32(output + 8, length);
        guest.cpu.reg_set(mode, reg::SP, output + 8);
        guest.cpu.reg_set(mode, R0, 1014);
        guest.cpu.reg_set(mode, R2, requested);
        guest.cpu.reg_set(mode, R3, output);
        assert_eq!(guest.dispatch(38, &mut screen), 0);
        let address = guest.r32(output);
        assert_ne!(address, 0);
        assert_eq!(guest.r32(length), requested);
        guest.w32(address + requested - 4, 0xaa55aa55);
        guest.cpu.reg_set(mode, R0, 1015);
        guest.cpu.reg_set(mode, R1, address);
        assert_eq!(guest.dispatch(38, &mut screen), 0);
        assert_eq!(guest.heap_malloc(128), HEAP_BASE);
        guest.cpu.reg_set(mode, R0, 1014);
        assert_eq!(guest.dispatch(38, &mut screen), 0);
        assert_eq!(guest.r32(output), address);
    }

    #[test]
    fn offline_transport_initializes_but_cannot_connect() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let mut screen = crate::display::Screen::default();
        let mode = guest.cpu.mode();
        assert_eq!(guest.dispatch(81, &mut screen), 0);
        let socket = guest.dispatch(84, &mut screen);
        assert!(socket > 0);
        guest.cpu.reg_set(mode, R0, socket as u32);
        assert_eq!(guest.dispatch(85, &mut screen), -1);
        assert_eq!(guest.dispatch(86, &mut screen), 0);
        assert_eq!(guest.dispatch(86, &mut screen), -1);
        assert_eq!(guest.dispatch(82, &mut screen), 0);
    }

    #[test]
    #[ignore = "executes a long native timer; run in release mode"]
    fn native_timer_finishes_first_use_initialization() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let mut screen = crate::display::Screen::default();
        guest.timer_function = EXT_BASE;
        for (index, instruction) in [
            0xe3a00401, // mov r0, #0x01000000
            0xe2500001, // subs r0, r0, #1
            0x1afffffd, // bne to subs
            0xe3a00007, // mov r0, #7
            0xe12fff1e, // bx lr
        ]
        .into_iter()
        .enumerate()
        {
            guest.w32(EXT_BASE + index as u32 * 4, instruction);
        }
        assert_eq!(guest.fire_timer(&mut screen).unwrap(), 7);
    }

    #[test]
    fn native_allocator_tracks_a_guest_extension_into_display_scratch() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let tail = SCREENBUF + SCREENBUF_SIZE;
        let end = tail + 64;
        guest.set_data(DataSlot::LgMemEnd, end);
        guest.set_data(DataSlot::LgMemLeft, HEAP_LEN + 64);
        guest.set_data(DataSlot::LgMemFree, tail - HEAP_BASE);
        guest.w32(tail, 0);
        guest.w32(tail + 4, 64);
        guest.w32(HEAP_BASE, end - HEAP_BASE);
        assert_eq!(guest.heap_malloc(48), tail);
        assert_eq!(guest.r32(HEAP_BASE), end - HEAP_BASE);
        guest.heap_free(tail, 48);
        assert_eq!(guest.heap_malloc(64), HEAP_BASE);
        assert_eq!(guest.heap_malloc(HEAP_LEN - 64), HEAP_BASE + 64);
        assert_eq!(guest.heap_malloc(56), tail);
        assert_eq!(guest.r32(tail + 56), end - HEAP_BASE);
    }

    #[test]
    fn guest_writing_a_released_buffer_cannot_damage_future_allocations() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let first = guest.heap_malloc(32);
        let second = guest.heap_malloc(32);
        guest.heap_free(first, 32);
        guest.w32(first, 0);
        guest.w32(first + 4, 0);

        assert_eq!(guest.heap_malloc(8), first);
        assert_eq!(guest.r32(first + 8), second + 32 - HEAP_BASE);
        assert_eq!(guest.r32(first + 12), 24);
        assert_eq!(guest.heap_malloc(24), first + 8);
    }

    #[test]
    fn native_allocator_uses_a_block_temporarily_published_by_the_guest() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let first = guest.heap_malloc(64);
        let _reserved = guest.heap_malloc(64);
        let sentinel = guest.data_slot_address(DataSlot::LgMemFree);
        let old_head = guest.r32(sentinel);
        let old_left = guest.get_data(DataSlot::LgMemLeft);

        // The unpacker temporarily adds its output region to the device heap.
        guest.w32(first, old_head);
        guest.w32(first + 4, 64);
        guest.w32(sentinel, 0);
        guest.set_data(DataSlot::LgMemLeft, old_left + 64);
        assert_eq!(guest.heap_malloc(64), first);
        assert_eq!(guest.r32(sentinel), old_head);
        assert_eq!(guest.get_data(DataSlot::LgMemLeft), old_left);
    }

    #[test]
    fn native_sms_commands_accept_notification_modes_and_report_empty_folders() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let mut screen = crate::display::Screen::new(240, 320);
        let mode = guest.cpu.mode();

        for (command, parameter, expected) in [
            (1011, 0, 0),
            (1011, 1, 0),
            (1011, 2, -1),
            (1101, 0, -1),
            (1101, 1, -1),
            (1101, 2, 1000),
            (1101, 3, 1000),
            (1101, 4, 1000),
            (1101, 5, -1),
        ] {
            guest.cpu.reg_set(mode, R0, command);
            guest.cpu.reg_set(mode, R1, parameter);
            assert_eq!(
                guest.dispatch(37, &mut screen),
                expected,
                "mr_plat({command}, {parameter})"
            );
        }
    }

    #[test]
    fn native_platform_queries_report_keypad_device_and_glyph_metrics() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let mut screen = crate::display::Screen::new(240, 320);
        let mode = guest.cpu.mode();
        guest.cpu.reg_set(mode, R0, 1205);
        assert_eq!(guest.dispatch(37, &mut screen), 1000);
        guest.cpu.reg_set(mode, R0, 1206);
        assert_eq!(guest.dispatch(37, &mut screen), 1000);
        guest.cpu.reg_set(mode, R0, 1218);
        assert_eq!(guest.dispatch(37, &mut screen), 1001);

        let output = STACK_TOP - 32;
        let length = output + 4;
        let sp = output + 8;
        guest.cpu.reg_set(mode, R0, 1201);
        guest.cpu.reg_set(mode, R3, output);
        guest.cpu.reg_set(mode, reg::SP, sp);
        guest.w32(sp, length);
        assert_eq!(guest.dispatch(38, &mut screen), 0);
        assert_eq!(guest.r32(length), 4);
        assert_eq!(
            guest.guest_bytes(guest.r32(output), 4).unwrap(),
            [16, 16, 8, 16]
        );
    }

    #[test]
    fn native_drawing_follows_the_current_framebuffer_pointer() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let replacement = guest.heap_malloc(SCREENBUF_SIZE);
        guest.set_data(DataSlot::ScreenBufPtr, replacement);
        guest.draw_point(5, 6, 0x00ff00);
        guest.draw_rect(0, 1, 3, 2, 255, 0, 0);
        let mut screen = crate::display::Screen::new(240, 320);
        guest.refresh_screen(&mut screen);
        assert_eq!(screen.pixels()[6 * 240 + 5], 0x07e0);
        assert_eq!(screen.pixels()[240 + 2], 0xf800);
        assert_eq!(guest.r16(SCREENBUF + 2 * (6 * 240 + 5)), 0);
    }

    #[test]
    fn native_present_accepts_the_null_framebuffer_alias() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let mut screen = crate::display::Screen::new(240, 320);
        let mode = guest.cpu.mode();
        guest.w16(SCREENBUF, 0x1234);
        guest.cpu.reg_set(mode, R0, 0);
        guest.cpu.reg_set(mode, R1, 0);
        guest.cpu.reg_set(mode, R2, 0);
        guest.cpu.reg_set(mode, R3, 240);
        guest.cpu.reg_set(mode, reg::SP, STACK_TOP - 4);
        guest.w32(STACK_TOP - 4, 320);
        assert_eq!(guest.dispatch(29, &mut screen), 0);
        assert_eq!(screen.pixels()[0], 0x1234);
    }

    #[test]
    fn native_filesystem_provides_a_unicode_bitmap_font() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let mut screen = crate::display::Screen::new(240, 320);
        let mode = guest.cpu.mode();
        let path = guest.scratch_buffer(b"system/gb12.uc2");
        guest.cpu.reg_set(mode, R0, path);
        assert_eq!(guest.dispatch(42, &mut screen), 1);

        let file = guest.file_data("system/gb12.uc2").unwrap();
        assert_eq!(file.len(), 65535 * 18);
        let cjk = 0x4e2d * 18;
        assert!(file[cjk..cjk + 18].iter().any(|&byte| byte != 0));
        let space = usize::from(b' ') * 18;
        assert!(file[space..space + 18].iter().all(|&byte| byte == 0));

        let companion = guest.scratch_buffer(b"system/gb12_uc2.adl");
        guest.cpu.reg_set(mode, R0, companion);
        assert_eq!(guest.dispatch(42, &mut screen), 1);
        assert_eq!(
            guest.file_data("system/gb12_uc2.adl").unwrap(),
            [1, 0, 0, 0, 0xa0, 0xd9, 8, 0]
        );
    }

    #[test]
    fn native_storage_root_switch_keeps_drive_paths_distinct() {
        let mut guest = Guest::new(
            GuestConfig::default(),
            vec![
                ("disk/a/saves/state.dat".to_string(), vec![1]),
                ("disk/b/saves/state.dat".to_string(), vec![2]),
            ],
        );
        let mut screen = crate::display::Screen::new(240, 320);
        let mode = guest.cpu.mode();
        let sp = STACK_TOP - 32;
        guest.cpu.reg_set(mode, reg::SP, sp);
        guest.cpu.reg_set(mode, R0, 1204);
        guest.cpu.reg_set(mode, R3, sp + 4);
        guest.w32(sp, sp + 8);
        for (root, bytes) in [
            (b"A:/saves/".as_slice(), [1]),
            (b"B:/saves/".as_slice(), [2]),
        ] {
            let input = guest.scratch_buffer(root);
            guest.cpu.reg_set(mode, R1, input);
            guest.cpu.reg_set(mode, R2, root.len() as u32);
            assert_eq!(guest.dispatch(38, &mut screen), 0);
            let handle = guest.file_open("state.dat").unwrap();
            assert_eq!(guest.file_read(handle, 1).unwrap(), bytes);
        }
        let query = guest.scratch_buffer(b"Y");
        guest.cpu.reg_set(mode, R1, query);
        guest.cpu.reg_set(mode, R2, 1);
        assert_eq!(guest.dispatch(38, &mut screen), 0);
        assert_eq!(guest.r32(sp + 8), 9);
        assert_eq!(guest.guest_cstring(guest.r32(sp + 4), 32), b"b:/saves/");
    }

    #[test]
    fn synchronous_guest_timer_poll_advances_with_a_fixed_frame_clock() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let mut screen = crate::display::Screen::new(240, 320);
        guest.set_frame_time(Some(Duration::ZERO));
        for (index, instruction) in [
            0xe92d4000, // push {lr}
            0xe59f400c, // ldr r4, [pc, #12]
            0xe12fff34, // blx r4
            0xe3500000, // cmp r0, #0
            0x0afffffc, // beq to blx
            0xe8bd8000, // pop {pc}
            BRIDGE_BASE + 33 * 4,
        ]
        .into_iter()
        .enumerate()
        {
            guest.w32(EXT_BASE + index as u32 * 4, instruction);
        }
        assert_eq!(guest.run_guest(EXT_BASE, 50_000, &mut screen).unwrap(), 1);
        assert_eq!(guest.uptime_ms(), 1);
    }

    #[test]
    fn native_allocator_preserves_the_order_of_guest_spliced_blocks() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let low = guest.heap_malloc(64);
        let high = guest.heap_malloc(64);
        let sentinel = guest.data_slot_address(DataSlot::LgMemFree);
        let old_head = guest.r32(sentinel);
        let old_left = guest.get_data(DataSlot::LgMemLeft);

        guest.w32(high, low - HEAP_BASE);
        guest.w32(high + 4, 64);
        guest.w32(low, old_head);
        guest.w32(low + 4, 64);
        guest.w32(sentinel, high - HEAP_BASE);
        guest.set_data(DataSlot::LgMemLeft, old_left + 128);
        assert_eq!(guest.heap_malloc(64), high);
        assert_eq!(guest.heap_malloc(64), low);
        assert_eq!(guest.r32(sentinel), old_head);
    }

    fn guest() -> Guest {
        Guest::new(GuestConfig::default(), Vec::new())
    }

    #[test]
    fn table_layout_matches_reference_indices() {
        // Spot-check the guest-visible ABI: slot 25 is mr_c_function_new
        // (0x64/4 per the EXT notes), 91-94 are the screen data slots.
        assert_eq!(SLOTS[25], Slot::Func);
        assert_eq!(SLOTS[91], Slot::Data(DataSlot::ScreenBufPtr));
        assert_eq!(SLOTS[92], Slot::Data(DataSlot::ScreenWidth));
        assert_eq!(SLOTS[93], Slot::Data(DataSlot::ScreenHeight));
        assert_eq!(SLOTS[94], Slot::Data(DataSlot::ScreenBit));
        assert_eq!(SLOTS.len(), 149);
    }

    #[test]
    fn table_points_at_bridge_and_data_slots() {
        let g = guest();
        assert_eq!(g.r32(TABLE_BASE), BRIDGE_BASE);
        assert_eq!(g.r32(TABLE_BASE + 25 * 4), BRIDGE_BASE + 25 * 4);
        assert_eq!(
            g.r32(TABLE_BASE + 92 * 4),
            g.data_slot_address(DataSlot::ScreenWidth)
        );
        assert_eq!(g.r32(g.data_slot_address(DataSlot::ScreenHeight)), 320);
        assert_eq!(
            g.r32(g.data_slot_address(DataSlot::ScreenBufPtr)),
            SCREENBUF
        );
    }

    #[test]
    fn native_file_calls_report_status_and_read_from_seek_position() {
        let mut g = Guest::new(
            GuestConfig::default(),
            vec![("sample.dat".to_string(), b"abcdef".to_vec())],
        );
        let name = g.scratch_buffer(b"sample.dat");
        let handle = g.file_open("sample.dat").expect("file exists") as u32;
        let mode = g.cpu.mode();
        g.cpu.reg_set(mode, R0, handle);
        g.cpu.reg_set(mode, R1, 2);
        g.cpu.reg_set(mode, R2, 0);
        assert_eq!(g.swi_seek(), 0);
        g.cpu.reg_set(mode, R1, HEAP_BASE);
        g.cpu.reg_set(mode, R2, 2);
        assert_eq!(g.swi_read(), 2);
        assert_eq!(g.guest_bytes(HEAP_BASE, 2).unwrap(), b"cd");

        g.cpu.reg_set(mode, R0, name);
        assert_eq!(g.swi_get_len(), 6);
        assert_eq!(g.swi_info(), 1);
    }

    #[test]
    fn native_file_write_is_visible_to_other_handles() {
        let mut g = guest();
        let path = g.scratch_buffer(b"cache\\data.bin");
        let mode = g.cpu.mode();
        g.cpu.reg_set(mode, R0, path);
        g.cpu.reg_set(mode, R1, 12);
        let writer = g.swi_open() as u32;
        assert_ne!(writer, 0);
        let path = g.scratch_buffer(b"cache/data.bin");
        g.cpu.reg_set(mode, R0, path);
        g.cpu.reg_set(mode, R1, 1);
        let reader = g.swi_open() as u32;
        assert_ne!(reader, 0);

        let bytes = g.scratch_buffer(b"frame");
        g.cpu.reg_set(mode, R0, writer);
        g.cpu.reg_set(mode, R1, bytes);
        g.cpu.reg_set(mode, R2, 5);
        assert_eq!(g.swi_write(), 5);
        g.cpu.reg_set(mode, R0, reader);
        g.cpu.reg_set(mode, R1, HEAP_BASE + 16);
        assert_eq!(g.swi_read(), 5);
        assert_eq!(g.guest_bytes(HEAP_BASE + 16, 5).unwrap(), b"frame");
        g.cpu.reg_set(mode, R0, writer);
        assert_eq!(g.swi_close(), 0);
        assert_eq!(g.swi_write(), -1);
        assert_eq!(g.file_data("cache/data.bin"), Some(&b"frame"[..]));
    }

    #[test]
    fn native_realloc_uses_old_and_new_lengths() {
        let mut g = guest();
        let mut screen = crate::display::Screen::new(240, 320);
        let mode = g.cpu.mode();
        let old = g.heap_malloc(8);
        let neighbor = g.heap_malloc(8);
        g.w32(old, 0xdead_beef);
        g.w32(neighbor, 0xbadc_0ffe);
        g.cpu.reg_set(mode, R0, old);
        g.cpu.reg_set(mode, R1, 8);
        g.cpu.reg_set(mode, R2, 24);
        let new = g.dispatch(2, &mut screen) as u32;
        assert_eq!(g.r32(new), 0xdead_beef);
        assert_eq!(g.heap_left, HEAP_LEN - 32);
        assert_eq!(g.r32(neighbor), 0xbadc_0ffe);
        assert_eq!(g.heap_malloc(8), old);
    }

    #[test]
    fn native_datetime_does_not_overwrite_adjacent_stack_words() {
        let mut g = guest();
        let mut screen = crate::display::Screen::new(240, 320);
        let mode = g.cpu.mode();
        let datetime = STACK_TOP - 16;
        g.w32(datetime + 8, 0xa5a5_a5a5);
        g.w32(datetime + 12, 0x0008_0123);
        g.cpu.reg_set(mode, R0, datetime);
        assert_eq!(g.dispatch(34, &mut screen), 0);
        assert!((2000..=2100).contains(&g.r16(datetime)));
        assert!((1..=12).contains(&g.r8(datetime + 2)));
        assert_eq!(g.r32(datetime + 8), 0xa5a5_a5a5);
        assert_eq!(g.r32(datetime + 12), 0x0008_0123);
    }

    #[test]
    fn guest_internal_table_exposes_timer_and_inflate_state() {
        use std::io::Write;

        let mut g = guest();
        let table = g.r32(TABLE_BASE + 23 * 4);
        assert_eq!(table, INTERNAL_TABLE_BASE);
        let timer_state = g.r32(table + 5 * 4);
        g.timer_start(0, 20);
        assert_eq!(g.r32(timer_state), 1);
        g.timer_stop(0);
        assert_eq!(g.r32(timer_state), 0);

        let payload = b"resource data";
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(payload).unwrap();
        let compressed = gzip.finish().unwrap();
        let input = g.heap_malloc(compressed.len() as u32);
        let output = g.heap_malloc(payload.len() as u32);
        g.mem[input as usize..input as usize + compressed.len()].copy_from_slice(&compressed);
        for (index, value) in [(7, input), (8, output), (9, 10)] {
            g.w32(g.r32(table + index * 4), value);
        }
        let mut screen = crate::display::Screen::new(240, 320);
        assert_eq!(g.dispatch(141, &mut screen), 0);
        assert_eq!(g.guest_bytes(output, payload.len()).unwrap(), payload);
        assert_eq!(g.r32(g.r32(table + 9 * 4)), compressed.len() as u32);
        assert_eq!(g.r32(g.r32(table + 10 * 4)), payload.len() as u32);
    }

    #[test]
    fn native_present_reads_full_stride_without_writing_guest_pixels() {
        let mut g = Guest::new(
            GuestConfig {
                screen_width: 4,
                screen_height: 3,
                ..Default::default()
            },
            Vec::new(),
        );
        let mut screen = crate::display::Screen::new(4, 3);
        let format = g.cpu.mode();
        let pixels = [
            0xf800, 0x07e0, 0x001f, 0, 0x1082, 0x39e7, 0xffff, 0, 0, 0, 0, 0,
        ];
        for (index, pixel) in pixels.iter().enumerate() {
            g.w16(SCREENBUF + index as u32 * 2, *pixel);
        }
        g.cpu.reg_set(format, R0, SCREENBUF);
        g.cpu.reg_set(format, R1, 1);
        g.cpu.reg_set(format, R2, 1);
        g.cpu.reg_set(format, R3, 2);
        g.cpu.reg_set(format, reg::SP, STACK_TOP - 16);
        g.w32(STACK_TOP - 16, 1);
        assert_eq!(g.dispatch(29, &mut screen), 0);
        assert_eq!(screen.pixels(), pixels);
        assert_eq!(g.r16(SCREENBUF + (4 + 1) * 2), 0x39e7);
        // Native applications may clear or reuse their source after submitting it.
        g.fill_screenbuf(0);
        g.present_frame(&mut screen);
        assert_eq!(screen.pixels(), pixels);
    }

    #[test]
    fn nested_native_loader_returns_to_the_guest_caller() {
        let mut g = guest();
        g.w32(EXT_BASE, 0xe52d_e004); // push {lr}
        g.w32(EXT_BASE + 4, 0xe12f_ff33); // blx r3
        g.w32(EXT_BASE + 8, 0xe280_0001); // add r0, r0, #1
        g.w32(EXT_BASE + 12, 0xe49d_f004); // pop {pc}
        g.w32(HEAP_BASE + 8, 0xe280_0007); // add r0, r0, #7
        g.w32(HEAP_BASE + 12, 0xe12f_ff1e); // bx lr
        let mode = g.cpu.mode();
        g.cpu.reg_set(mode, R0, 100);
        g.cpu.reg_set(mode, R1, 800);
        g.cpu.reg_set(mode, R2, HEAP_BASE);
        g.cpu.reg_set(mode, R3, BRIDGE_BASE + 130 * 4);
        let mut screen = crate::display::Screen::new(240, 320);
        assert_eq!(g.run_guest(EXT_BASE, 16, &mut screen).unwrap(), 8);
        assert_eq!(g.r32(HEAP_BASE), TABLE_BASE);
    }

    #[test]
    fn native_callbacks_receive_mini_event_sizes() {
        let mut g = guest();
        g.c_function = EXT_BASE;
        g.c_function_p = g.heap_malloc(20);
        assert_ne!(g.heap_malloc(HEAP_LEN - 40), 0);
        g.w32(EXT_BASE, 0xe1a0_0003); // mov r0, r3
        g.w32(EXT_BASE + 4, 0xe12f_ff1e); // bx lr
        let mut screen = crate::display::Screen::new(240, 320);
        assert_eq!(g.send_event(0, 20, 0, &mut screen).unwrap(), 20);
        assert_eq!(g.fire_timer(&mut screen).unwrap(), 1);
    }

    #[test]
    fn native_point_and_rectangle_use_sdk_color_and_dimensions() {
        let mut g = guest();
        let mode = g.cpu.mode();
        let mut screen = crate::display::Screen::new(240, 320);
        g.cpu.reg_set(mode, R0, 4);
        g.cpu.reg_set(mode, R1, 6);
        g.cpu.reg_set(mode, R2, 0xf800);
        assert_eq!(g.dispatch(119, &mut screen), 0);
        assert_eq!(g.r16(SCREENBUF + (6 * 240 + 4) * 2), 0xf800);

        g.cpu.reg_set(mode, R0, u32::MAX);
        g.cpu.reg_set(mode, R1, 1);
        g.cpu.reg_set(mode, R2, 4);
        g.cpu.reg_set(mode, R3, 2);
        let sp = STACK_TOP - 16;
        g.cpu.reg_set(mode, reg::SP, sp);
        g.w32(sp, 0);
        g.w32(sp + 4, 255);
        g.w32(sp + 8, 0);
        assert_eq!(g.dispatch(122, &mut screen), 0);
        assert_eq!(g.r16(SCREENBUF + (2 * 240 + 2) * 2), 0x07e0);
        assert_eq!(g.r16(SCREENBUF + (2 * 240 + 3) * 2), 0);
    }

    #[test]
    fn native_bitmap_copy_observes_source_stride_and_offsets() {
        let mut g = guest();
        let src = HEAP_BASE + 32;
        for index in 0..12 {
            g.w16(src + index * 2, index as u16 + 1);
        }
        g.draw_bitmap_rop(src, 0, 0, 2, 2, 2, 0, 1, 1, 4);
        assert_eq!(g.r32(SCREENBUF), 0x0007_0006);
        assert_eq!(g.r32(SCREENBUF + 480), 0x000b_000a);
    }

    #[test]
    fn native_text_handles_big_endian_unicode_and_register_color() {
        let mut g = guest();
        let mode = g.cpu.mode();
        let address = HEAP_BASE + 32;
        g.w32(address, 0x0000_4100); // UCS-2BE 'A' followed by NUL
        g.cpu.reg_set(mode, R0, address);
        g.cpu.reg_set(mode, R3, 255);
        let sp = STACK_TOP - 16;
        g.cpu.reg_set(mode, reg::SP, sp);
        g.w32(sp + 8, 1);
        let mut screen = crate::display::Screen::new(240, 320);
        assert_eq!(g.dispatch(123, &mut screen), 0);
        g.refresh_screen(&mut screen);
        assert!(screen.pixels().contains(&0xf800));
    }

    #[test]
    fn heap_malloc_and_free_roundtrip() {
        let mut g = guest();
        let a = g.heap_malloc(100);
        assert_ne!(a, 0);
        assert_eq!(a % 8, 0);
        let left_after_alloc = g.heap_left;
        g.heap_free(a, 100);
        assert!(g.heap_left > left_after_alloc);
    }

    #[test]
    fn heap_double_coalesce_keeps_total_size() {
        // Freeing two adjacent blocks must merge all three regions (the two
        // freed blocks plus any following free node) into one correctly
        // sized node — the reference allocator's probe pattern depends on it.
        let mut g = guest();
        let before = g.heap_left;
        let a = g.heap_malloc(8);
        let b = g.heap_malloc(8);
        let c = g.heap_malloc(8);
        assert_ne!((a, b, c), (0, 0, 0));
        let used = before - g.heap_left;
        g.heap_free(a, 8);
        g.heap_free(b, 8);
        g.heap_free(c, 8);
        assert_eq!(g.heap_left, before);
        // The whole allocated span must now be one free block: a malloc of
        // the full used size succeeds and returns the original base.
        let whole = g.heap_malloc(used);
        assert_eq!(whole, a);
    }

    #[test]
    fn heap_free_before_keeps_list_ordered() {
        // Freeing in descending address order must keep the list walkable
        // (guests probe the list expecting address order).
        let mut g = guest();
        let a = g.heap_malloc(8);
        let b = g.heap_malloc(8);
        let c = g.heap_malloc(8);
        g.heap_free(c, 8);
        g.heap_free(b, 8);
        g.heap_free(a, 8);
        assert_eq!(g.heap_left, HEAP_LEN);
        let whole = g.heap_malloc(24);
        assert_eq!(whole, a);
    }

    #[test]
    fn heap_exhaustion_is_reported_as_zero() {
        let mut g = guest();
        assert_eq!(g.heap_malloc(HEAP_MAX_ALLOC + 1), 0);
    }

    #[test]
    fn heap_double_free_preserves_the_free_list() {
        let mut g = guest();
        let first = g.heap_malloc(16);
        let second = g.heap_malloc(16);
        g.heap_free(first, 16);
        let free_bytes = g.heap_left;
        g.heap_free(first, 16);
        assert_eq!(g.heap_left, free_bytes);
        assert_eq!(g.heap_malloc(16), first);
        assert_eq!(g.heap_malloc(16), second + 16);
    }

    #[test]
    fn heap_keeps_an_eight_byte_remainder_reusable() {
        let mut g = guest();
        let first = g.heap_malloc(16);
        let _next = g.heap_malloc(16);
        g.heap_free(first, 16);
        assert_eq!(g.heap_malloc(8), first);
        assert_eq!(g.heap_malloc(8), first + 8);
    }

    #[test]
    fn sprintf_writes_guest_memory() {
        let mut g = guest();
        // dst = DATA area scratch, fmt points at a "%d items" string built
        // inside guest memory.
        let dst = g.data_slot_address(DataSlot::RamFile);
        let fmt_addr = SCREENBUF + SCREENBUF_SIZE + 16; // free scratch past the buffer
        let scratch = fmt_addr as usize;
        let format = b"count=%d done";
        g.mem[scratch..scratch + format.len()].copy_from_slice(format);
        g.mem[scratch + format.len()] = 0;
        g.cpu.reg_set(g.cpu.mode(), R0, dst);
        g.cpu.reg_set(g.cpu.mode(), R1, fmt_addr);
        g.cpu.reg_set(g.cpu.mode(), R2, 7);
        assert_eq!(g.dispatch_mem_string(17), 12);
        assert_eq!(&g.mem[dst as usize..dst as usize + 12], b"count=7 done");
    }

    #[test]
    fn test_com_registrations_are_recorded() {
        let mut g = guest();
        g.cpu.reg_set(g.cpu.mode(), R1, 0x02);
        g.cpu.reg_set(g.cpu.mode(), R2, 0xCAFE);
        assert_eq!(g.dispatch_test_com(0x02, 0xCAFE), 0);
        assert_eq!(g.event_function, 0xCAFE);
        g.cpu.reg_set(g.cpu.mode(), R1, 0x03);
        g.cpu.reg_set(g.cpu.mode(), R2, 0xBEEF);
        assert_eq!(g.dispatch_test_com(0x03, 0xBEEF), 0);
        assert_eq!(g.timer_function, 0xBEEF);
    }

    /// Boots the real hello.mrp ext through the full load/handshake/init
    /// sequence. Requires the local reference clone, so it stays ignored.
    /// Boots every reference sample through the real MrpMachine script
    /// path: start.mr runs on the VM, and the script loads ext images via
    /// _com(800). This is the corpus gate.
    /// Visual verification: boot hello.mrp, run 60 frames, check the
    /// screen buffer has non-background pixels (drawText output).
    #[test]
    #[ignore = "requires the local reference corpus under tmp/"]
    fn hello_screen_has_text() {
        let machine =
            crate::emulator::MrpMachine::from_path("../../tmp/mrpemu/sdcard/mythroad/hello.mrp")
                .expect("hello.mrp must load");
        let mut machine = machine;
        for _ in 0..60 {
            machine.run_frame();
        }
        // Log which table slots the ext actually called.
        if let Some(guest) = machine.guest_ref() {
            for (slot, count) in guest.slot_calls.iter().enumerate() {
                if *count > 0 {
                    eprintln!("slot {slot}: called {count} times");
                }
            }
        }
        // The screen must not be all-background (the placeholder or blank).
        let screen = machine.screen();
        let pixels = screen.pixels();
        let unique: std::collections::HashSet<u16> = pixels.iter().copied().collect();
        eprintln!("unique colors on screen: {}", unique.len());
        eprintln!(
            "sample colors: {:?}",
            unique.iter().take(10).collect::<Vec<_>>()
        );
        // The ext replaced the placeholder (0x1082/0x39E7) with its own
        // drawing. Black+white = the classic terminal text rendering.
        // drawText IS rendering if we see non-placeholder colors.
        let has_placeholder = unique.contains(&0x1082) || unique.contains(&0x39E7);
        assert!(
            !has_placeholder && unique.len() >= 2,
            "screen still shows placeholder colors ({:?}); drawText did not render",
            unique
        );
        eprintln!(
            "✓ drawText rendered: {} colors (black bg + white text)",
            unique.len()
        );
    }

    /// Writes the machine screen as a 24-bit BMP for visual inspection.
    fn write_screen_bmp(machine: &crate::emulator::MrpMachine, path: &str) {
        let screen = machine.screen();
        let pixels = screen.pixels();
        let (width, height) = (screen.width() as u32, screen.height() as u32);
        let row_size = width * 3;
        let padding = (4 - (row_size % 4)) % 4;
        let pixel_bytes = (row_size + padding) * height;
        let mut bmp = Vec::with_capacity(54 + pixel_bytes as usize);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(54 + pixel_bytes).to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&width.to_le_bytes());
        bmp.extend_from_slice(&height.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&pixel_bytes.to_le_bytes());
        bmp.extend_from_slice(&2835u32.to_le_bytes());
        bmp.extend_from_slice(&2835u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        for y in (0..height).rev() {
            for x in 0..width {
                let pixel = pixels[(y * width + x) as usize];
                let red = ((pixel >> 11) & 0x1F) as u8;
                let green = ((pixel >> 5) & 0x3F) as u8;
                let blue = (pixel & 0x1F) as u8;
                bmp.extend_from_slice(&[
                    (blue << 3) | (blue >> 2),
                    (green << 2) | (green >> 4),
                    (red << 3) | (red >> 2),
                ]);
            }
            bmp.resize(bmp.len() + padding as usize, 0);
        }
        let _ = std::fs::write(path, &bmp);
    }

    /// Survey: run every reference sample through 240 frames with periodic
    /// input, then dump per-sample diagnostics (boot path, screen histogram,
    /// top slots, missing globals) and a BMP snapshot each.
    #[test]
    #[ignore = "requires the local reference corpus under tmp/"]
    fn samples_runtime_survey() {
        let _ = env_logger::try_init();
        let samples = [
            "../../tmp/mrpemu/sdcard/mythroad/hello.mrp",
            "../../tmp/mrpemu/sdcard/mythroad/gymls.mrp",
            "../../tmp/mrpemu/sdcard/mythroad/test.mrp",
            "../../tmp/vmrp/wasm/dist/fs/mythroad/dsm_gm.mrp",
            "../../tmp/vmrp/wasm/dist/fs/mythroad/mpc.mrp",
            "../../tmp/work/Maze.mrp",
            "../../tmp/vmrp/mrc/asm/asm.mrp",
            "../../tmp/vmrp/wasm/dist/fs/mythroad/ydqtwo.mrp",
        ];
        let _ = std::fs::create_dir_all("../../tmp/work/survey");
        // SURVEY_ONLY=<substring> restricts the run to matching samples.
        let only = std::env::var("SURVEY_ONLY").ok();
        for sample in &samples {
            let name = sample.rsplit('/').next().unwrap_or(sample);
            let stem = name.trim_end_matches(".mrp");
            if let Some(only) = &only {
                if !name.contains(only.as_str()) {
                    continue;
                }
            }
            let Ok(mut machine) = crate::emulator::MrpMachine::from_path(sample) else {
                eprintln!("=== {name}: LOAD FAILED ===");
                continue;
            };
            for frame in 0..240 {
                machine.run_frame();
                for (press_frame, key) in [
                    (40u32, crate::input::MrpKey::Down),
                    (60, crate::input::MrpKey::Down),
                    (80, crate::input::MrpKey::Ok),
                    (120, crate::input::MrpKey::Ok),
                    (160, crate::input::MrpKey::Right),
                    (180, crate::input::MrpKey::Up),
                ] {
                    if frame == press_frame {
                        machine.input_mut().press(key);
                    }
                    if frame == press_frame + 3 {
                        machine.input_mut().release(key);
                    }
                }
            }
            let screen = machine.screen();
            let pixels = screen.pixels();
            let mut histogram: std::collections::HashMap<u16, usize> =
                std::collections::HashMap::new();
            for &pixel in pixels {
                *histogram.entry(pixel).or_insert(0) += 1;
            }
            let mut colors: Vec<_> = histogram.iter().collect();
            colors.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
            let top: Vec<String> = colors
                .iter()
                .take(5)
                .map(|(color, count)| {
                    format!(
                        "{color:04x}:{:.1}%",
                        **count as f64 / pixels.len() as f64 * 100.0
                    )
                })
                .collect();
            let mut slots: Vec<String> = machine
                .guest_ref()
                .map(|guest| {
                    guest
                        .slot_calls
                        .iter()
                        .enumerate()
                        .filter(|(_, count)| **count > 0)
                        .map(|(slot, count)| format!("{slot}:{count}"))
                        .collect()
                })
                .unwrap_or_default();
            slots.truncate(12);
            let missing = machine.missing_globals().len();
            eprintln!(
                "=== {name}: ext={} colors={} top=[{}] slots=[{}] missing={} ===",
                machine.ext_handler_registered(),
                histogram.len(),
                top.join(" "),
                slots.join(" "),
                missing
            );
            write_screen_bmp(&machine, &format!("../../tmp/work/survey/{stem}.bmp"));
        }
    }

    /// Diagnostic: boot Maze.mrp, run 60 frames, dump all slot calls with
    /// return values, screen color histogram, and missing natives.
    #[test]
    #[ignore = "requires the local reference corpus under tmp/"]
    fn maze_slot_trace() {
        let _ = env_logger::try_init();
        let mut machine = crate::emulator::MrpMachine::from_path("../../tmp/work/Maze.mrp")
            .expect("Maze.mrp must load");
        for frame in 0..400 {
            machine.run_frame();
            for (press_frame, key) in [
                (20u32, crate::input::MrpKey::Down),
                (25, crate::input::MrpKey::Up),
                (30, crate::input::MrpKey::Ok),
            ] {
                if frame == press_frame {
                    machine.input_mut().press(key);
                }
                if frame == press_frame + 3 {
                    machine.input_mut().release(key);
                }
            }
        }
        // Dump slot call counts
        if let Some(guest) = machine.guest_ref() {
            eprintln!("=== Maze.mrp slot calls after 60 frames ===");
            for (slot, count) in guest.slot_calls.iter().enumerate() {
                if *count > 0 {
                    eprintln!("  slot {slot}: {count} calls");
                }
            }
            eprintln!("  bitmaps loaded: {}", guest.bitmaps.len());
            eprintln!("  exit_requested: {}", guest.exit_requested);
        }
        // Screen state
        let screen = machine.screen();
        let pixels = screen.pixels();
        let mut histogram: std::collections::HashMap<u16, usize> = std::collections::HashMap::new();
        for &pixel in pixels {
            *histogram.entry(pixel).or_insert(0) += 1;
        }
        let mut colors: Vec<_> = histogram.iter().collect();
        colors.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
        eprintln!("=== screen histogram (top 10) ===");
        for (color, count) in colors.iter().take(10) {
            eprintln!(
                "  {color:#06x}: {count} pixels ({:.1}%)",
                **count as f64 / pixels.len() as f64 * 100.0
            );
        }
        eprintln!("unique colors: {}", histogram.len());
        // Dump the final frame as a 24-bit BMP for visual inspection.
        let (width, height) = (screen.width() as u32, screen.height() as u32);
        let row_size = width * 3;
        let padding = (4 - (row_size % 4)) % 4;
        let pixel_bytes = (row_size + padding) * height;
        let mut bmp = Vec::with_capacity(54 + pixel_bytes as usize);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(54 + pixel_bytes).to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&width.to_le_bytes());
        bmp.extend_from_slice(&height.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&pixel_bytes.to_le_bytes());
        bmp.extend_from_slice(&2835u32.to_le_bytes());
        bmp.extend_from_slice(&2835u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        for y in (0..height).rev() {
            for x in 0..width {
                let pixel = pixels[(y * width + x) as usize];
                let red = ((pixel >> 11) & 0x1F) as u8;
                let green = ((pixel >> 5) & 0x3F) as u8;
                let blue = (pixel & 0x1F) as u8;
                bmp.extend_from_slice(&[
                    (blue << 3) | (blue >> 2),
                    (green << 2) | (green >> 4),
                    (red << 3) | (red >> 2),
                ]);
            }
            bmp.resize(bmp.len() + padding as usize, 0);
        }
        let _ = std::fs::write("../../tmp/work/maze_screen.bmp", &bmp);
        let _ = machine.missing_globals();
    }

    #[test]
    #[ignore = "requires the local reference corpus under tmp/"]
    fn boots_reference_samples() {
        let _ = env_logger::try_init();
        let samples = [
            "../../tmp/mrpemu/sdcard/mythroad/hello.mrp",
            "../../tmp/mrpemu/sdcard/mythroad/gymls.mrp",
            "../../tmp/mrpemu/sdcard/mythroad/test.mrp",
            "../../tmp/vmrp/wasm/dist/fs/mythroad/dsm_gm.mrp",
            "../../tmp/vmrp/wasm/dist/fs/mythroad/mpc.mrp",
            "../../tmp/work/Maze.mrp",
            "../../tmp/vmrp/mrc/asm/asm.mrp",
            "../../tmp/vmrp/wasm/dist/fs/mythroad/ydqtwo.mrp",
        ];
        let mut failures = Vec::new();
        for sample in &samples {
            let name = sample.rsplit('/').next().unwrap_or(sample);
            let mut machine = match crate::emulator::MrpMachine::from_path(sample) {
                Ok(machine) => machine,
                Err(error) => {
                    failures.push(format!("{name}: load failed: {error}"));
                    continue;
                }
            };
            for _ in 0..5 {
                machine.run_frame();
            }
            // Frame stability: run 60 frames, then 5 more after input.
            let mut frame_ok = true;
            for frame in 0..60 {
                machine.run_frame();
                if frame == 30 {
                    machine.input_mut().press(crate::input::MrpKey::Ok);
                }
                if frame == 45 {
                    machine.input_mut().release(crate::input::MrpKey::Ok);
                }
            }
            if let Some(error) = machine.boot_error() {
                frame_ok = false;
                failures.push(format!("{name}: frame loop error: {error}"));
            }
            if frame_ok {
                let status = if machine.ext_handler_registered() {
                    "ext handler registered"
                } else {
                    "script-only"
                };
                log::info!("{name}: 60-frame stability OK ({status})");
            }
        }
        assert!(
            failures.is_empty(),
            "samples failed to boot:
{}",
            failures.join(
                "
"
            )
        );
    }

    #[test]
    fn run_guest_times_out_deterministically() {
        let mut g = guest();
        // An infinite loop: `B .` at the entry.
        g.w32(EXT_BASE, 0xEAFFFFFE);
        let mut screen = crate::display::Screen::new(240, 320);
        assert!(matches!(
            g.run_guest(EXT_BASE, 1_000, &mut screen),
            Err(MrpError::Unsupported(_))
        ));
    }

    #[test]
    fn arm_doubleword_transfers_leave_signed_byte_loads_intact() {
        let mut g = guest();
        let data = HEAP_BASE + 32;
        g.w32(data + 8, 0x1122_80ff);
        g.w32(data + 12, 0x5566_7788);
        g.w32(EXT_BASE, 0xe1d2_50d9); // ldrsb r5, [r2, #9]
        g.w32(EXT_BASE + 4, 0xe1e2_00d8); // ldrd r0, r1, [r2, #8]!
        g.w32(EXT_BASE + 8, 0xe1a2_00f3); // strd r0, r1, [r2, r3]!
        g.w32(EXT_BASE + 12, 0xe12f_ff1e); // bx lr
        let mode = g.cpu.mode();
        g.cpu.reg_set(mode, R2, data);
        g.cpu.reg_set(mode, R3, 16);
        let mut screen = crate::display::Screen::new(240, 320);
        assert_eq!(g.run_guest(EXT_BASE, 16, &mut screen).unwrap(), 0x1122_80ff);
        assert_eq!(g.cpu.reg_get(mode, 5), 0xffff_ff80);
        assert_eq!(g.cpu.reg_get(mode, R1), 0x5566_7788);
        assert_eq!(g.cpu.reg_get(mode, R2), data + 24);
        assert_eq!(g.r32(data + 24), 0x1122_80ff);
        assert_eq!(g.r32(data + 28), 0x5566_7788);
    }

    #[test]
    fn smlabb_computes_row_width_col_index() {
        let mut g = guest();
        // The maze generator computes `col + row * width` with an ARMv5TE
        // SMLABB (16x16 signed multiply-accumulate), which armv4t_emu
        // cannot execute: r0 = r2(low16) * r0(low16) + r1 = 3*15 + 1 = 46.
        g.w32(LG_MEM_BASE, 0xE3A0_000F); // mov    r0, #15      ; width
        g.w32(LG_MEM_BASE + 4, 0xE3A0_2003); // mov    r2, #3       ; row
        g.w32(LG_MEM_BASE + 8, 0xE3A0_1001); // mov    r1, #1       ; col
        g.w32(LG_MEM_BASE + 12, 0xE100_1082); // smlabb r0, r2, r0, r1
        g.w32(LG_MEM_BASE + 16, 0xE12F_FF1E); // bx     lr
        let mut screen = crate::display::Screen::new(240, 320);
        let result = g.run_guest(LG_MEM_BASE, 1_000, &mut screen);
        assert!(
            matches!(result, Ok(46)),
            "smlabb index math, got {result:?}"
        );
    }

    #[test]
    fn conditional_dsp_multiply_skips_when_condition_fails() {
        let mut g = guest();
        // A failed condition must leave the destination untouched.
        g.w32(LG_MEM_BASE, 0xE3A0_0002); // mov    r0, #2
        g.w32(LG_MEM_BASE + 4, 0xE350_0002); // cmp    r0, #2       ; equal: Z=1
        g.w32(LG_MEM_BASE + 8, 0xE3A0_1005); // mov    r1, #5
        g.w32(LG_MEM_BASE + 12, 0xE3A0_2003); // mov    r2, #3
        g.w32(LG_MEM_BASE + 16, 0x1100_1082); // smlabbne r0, r2, r0, r1 (NE fails)
        g.w32(LG_MEM_BASE + 20, 0xE12F_FF1E); // bx     lr
        let mut screen = crate::display::Screen::new(240, 320);
        let result = g.run_guest(LG_MEM_BASE, 1_000, &mut screen);
        assert!(
            matches!(result, Ok(2)),
            "failed-cond SMLA is a no-op, got {result:?}"
        );
    }

    #[test]
    fn conditional_blx_skips_when_condition_fails() {
        // The SDK runtime guards its optional hooks with
        // `cmp r0, #0; blxne r0` (e.g. inside mrc_refreshScreen). A failed
        // condition must skip the call, not execute it as an unconditional
        // BLX — otherwise a null hook would jump to zero and abort the
        // caller mid-function.
        let mut g = guest();
        g.w32(LG_MEM_BASE, 0xE350_0000); // cmp   r0, #0   (r0 == 0 at entry)
        g.w32(LG_MEM_BASE + 4, 0x112F_FF30); // blxne r0   (condition fails)
        g.w32(LG_MEM_BASE + 8, 0xE3A0_002A); // mov   r0, #42
        g.w32(LG_MEM_BASE + 12, 0xE12F_FF1E); // bx    lr    (lr == 0: return)
        let mut screen = crate::display::Screen::new(240, 320);
        let result = g.run_guest(LG_MEM_BASE, 1_000, &mut screen);
        assert!(
            matches!(result, Ok(42)),
            "fall through past the failed blxne, got {result:?}"
        );
    }

    /// Writes `data` into a guest file through the public file API so the
    /// dirty tracking behaves like a real `mr_write` session.
    fn write_guest_file(guest: &mut Guest, name: &str, data: &[u8]) {
        let handle = guest.file_open(name).expect("open seeded file");
        assert!(guest.file_write(handle, data), "write {name}");
        guest.file_close(handle);
    }

    fn temp_persistence_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mrpemu-persist-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create persistence dir");
        dir
    }

    #[test]
    fn persistence_flush_and_load_roundtrips_top_level_files() {
        let dir = temp_persistence_dir("top");
        let mut guest = Guest::new(
            GuestConfig::default(),
            vec![("save0.sav".to_string(), b"old".to_vec())],
        );
        write_guest_file(&mut guest, "save0.sav", b"progress-v1");
        assert_eq!(guest.flush_persistence(&dir).expect("flush"), 1);
        assert_eq!(
            std::fs::read(dir.join("save0.sav")).unwrap(),
            b"progress-v1"
        );

        let mut restored = Guest::new(
            GuestConfig::default(),
            vec![("myapp/save0.sav".to_string(), b"old".to_vec())],
        );
        assert_eq!(restored.load_persistence(&dir).expect("load"), 1);
        let data = restored
            .files
            .iter()
            .find(|(name, _)| name == "save0.sav")
            .map(|(_, data)| data.clone())
            .expect("restored save0.sav");
        assert_eq!(data, b"progress-v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistence_roundtrips_nested_directories() {
        let dir = temp_persistence_dir("nested");
        let mut guest = Guest::new(
            GuestConfig::default(),
            vec![("gymls/save.dat".to_string(), b"seed".to_vec())],
        );
        // The guest writes through the nested path it created at runtime.
        write_guest_file(&mut guest, "gymls/save.dat", b"nested-progress");
        assert_eq!(guest.flush_persistence(&dir).expect("flush"), 1);
        assert!(dir.join("gymls").join("save.dat").is_file());

        let mut restored = Guest::new(
            GuestConfig::default(),
            vec![("gymls/save.dat".to_string(), b"seed".to_vec())],
        );
        assert_eq!(restored.load_persistence(&dir).expect("load"), 1);
        assert!(
            restored.directories.contains("gymls"),
            "load registers the directory segment"
        );
        let data = restored
            .files
            .iter()
            .find(|(name, _)| name == "gymls/save.dat")
            .map(|(_, data)| data.clone())
            .expect("restored nested file under its original guest path");
        assert_eq!(data, b"nested-progress");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistence_load_updates_backslash_named_twin() {
        let dir = temp_persistence_dir("twin");
        // Flush a forward-slash name, then restore into a guest whose file
        // table carries the backslash twin a game may have created before a
        // path normalization fix. The load must update the twin in place
        // instead of adding a second entry.
        let mut guest = Guest::new(
            GuestConfig::default(),
            vec![("app/data.cfg".to_string(), b"stale".to_vec())],
        );
        write_guest_file(&mut guest, "app/data.cfg", b"cfg-v2");
        assert_eq!(guest.flush_persistence(&dir).expect("flush"), 1);

        let mut restored = Guest::new(
            GuestConfig::default(),
            vec![("app\\data.cfg".to_string(), b"stale".to_vec())],
        );
        assert_eq!(restored.load_persistence(&dir).expect("load"), 1);
        let updated = restored
            .files
            .iter()
            .find(|(name, _)| name == "app\\data.cfg")
            .map(|(_, data)| data.clone())
            .expect("backslash twin updated in place");
        assert_eq!(updated, b"cfg-v2");
        assert!(
            !restored
                .files
                .iter()
                .any(|(name, _)| name == "app/data.cfg"),
            "no duplicate forward-slash entry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistence_load_of_missing_dir_is_empty() {
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        let missing = std::env::temp_dir().join("mrpemu-persist-does-not-exist");
        assert_eq!(guest.load_persistence(&missing).expect("load"), 0);
        assert!(guest.files.is_empty());
    }
}
