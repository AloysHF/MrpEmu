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

use std::time::{Duration, Instant};

use armv4t_emu::{reg, Cpu, Memory};

/// General-purpose register numbers (the crate only names the specials).
const R0: reg::Reg = 0;
const R1: reg::Reg = 1;
const R2: reg::Reg = 2;
const R3: reg::Reg = 3;

use crate::error::MrpError;
use crate::fmt::{self, Arg};

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
/// The guest-visible screen buffer, RGB565 — above the heap (the reference
/// runtime allocates it from the SDK's own pool; a dedicated region keeps
/// guest allocations from colliding with presented frames).
pub const SCREENBUF: u32 = HEAP_BASE + HEAP_LEN;
pub const SCREENBUF_SIZE: u32 = 240 * 320 * 2;
/// The guest stack grows down from here (the real firmware's stack region
/// 0x180000-0x280000, sitting between the ext code area and the tables).
pub const STACK_TOP: u32 = 0x0028_0000;
pub const MEM_SIZE: u32 = SCREENBUF + 512 * 1024;

/// Largest single allocation accepted from the heap.
const HEAP_MAX_ALLOC: u32 = HEAP_LEN - 16;
/// First-fit bookkeeping: allocations are 8-aligned and never split below
/// this size, mirroring the reference allocator.
const HEAP_MIN_PIECE: u32 = 8;
/// Cap on guest instructions per host call, so runaway ext code surfaces as
/// an error instead of a hang.
pub const DEFAULT_INSTRUCTION_LIMIT: u64 = 20_000_000;

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
    /// Shared scratch region backing the mr_bitmap/tile/map/sound/sprite
    /// bookkeeping arrays (not modeled yet, but addressable).
    BitmapArrays,
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
        Slot::Data(BitmapArrays),       // 95 mr_bitmap array (placeholder region)
        Slot::Data(BitmapArrays),       // 96 mr_tile array
        Slot::Data(BitmapArrays),       // 97 mr_map
        Slot::Data(BitmapArrays),       // 98 mr_sound
        Slot::Data(BitmapArrays),       // 99 mr_sprite
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

// The placeholder slots (mr_bitmap/tile/map/sound/sprite arrays) share one
// scratch region via `DataSlot::BitmapArrays`.

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
    started_at: Instant,
    rand_state: u32,
    /// Heap free list: offset of the first free block relative to
    /// `HEAP_BASE`, then `next`/`size` words inside each free block (the
    /// reference allocator's layout).
    heap_free_head: u32,
    heap_left: u32,
    heap_min: u32,
    heap_top: u32,
    /// Guest address of the ext's registered event handler.
    pub c_function: u32,
    /// Guest address of the ext's `mr_c_function_st`.
    pub c_function_p: u32,
    /// Guest addresses registered through `mr_testCom` (codes 2..6).
    pub event_function: u32,
    pub timer_function: u32,
    stop_function: u32,
    pause_app_function: u32,
    resume_app_function: u32,
    /// Base flags word maintained by `mr_testCom` handshake codes.
    pub bi: u32,
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
    pub exit_requested: bool,
    /// Diagnostics: how often each table slot was called.
    pub slot_calls: [u64; TABLE_SLOTS],
    /// Heap address of the single glyph export buffer (lazily allocated).
    glyph_buf: u32,
    dirty_files: std::collections::HashSet<String>,
    /// Loaded bitmaps: id → RGB565 pixel data.
    pub bitmaps: std::collections::HashMap<u32, Bitmap>,
    /// Monotonically increasing handle for GUI controls (menu/dialog/text/edit/win).
    pub next_gui_handle: u32,
    /// The active platform menu (runtime-rendered, key-driven).
    pub menu: Option<MenuState>,
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

/// A loaded bitmap with dimensions and RGB565 pixel data.
#[derive(Debug, Clone)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u16>,
}

#[derive(Debug, Clone)]
pub struct OpenFile {
    pub name: String,
    pub data: Vec<u8>,
    pub pos: usize,
    /// `true` once the handle is closed (defensive reuse guard).
    pub closed: bool,
}

impl Guest {
    /// Creates the guest memory image and fills every table slot. `files`
    /// backs `mr_readFile`/`mr_open` (package members and VFS content).
    pub fn new(config: GuestConfig, files: Vec<(String, Vec<u8>)>) -> Self {
        let mut guest = Self {
            mem: vec![0; MEM_SIZE as usize],
            cpu: Cpu::new(),
            config,
            started_at: Instant::now(),
            rand_state: 0x1234_5678,
            heap_free_head: 0,
            heap_left: HEAP_LEN,
            heap_min: HEAP_LEN,
            heap_top: 0,
            c_function: 0,
            c_function_p: 0,
            event_function: 0,
            timer_function: 0,
            stop_function: 0,
            pause_app_function: 0,
            resume_app_function: 0,
            bi: 0,
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
            exit_requested: false,
            slot_calls: [0; TABLE_SLOTS],
            glyph_buf: 0,
            dirty_files: std::collections::HashSet::new(),
            bitmaps: std::collections::HashMap::new(),
            next_gui_handle: 0,
            menu: None,
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
        // The internal state table (slot 23): an array of 7 pointers to
        // live state cells. The helper consults mr_state (index 2) — it
        // must be MR_STATE_RUN(1) or refresh/timer/event paths early-return.
        let table = self.data_slot_address(DataSlot::CInternalTable);
        self.set_data(DataSlot::CInternalTable, table);
        // m0_files, vm_state, mr_state, bi, timer_p, timer_state,
        // timer_run_without_pause
        for (index, value) in [0u32, 0, 1, 0, 0, 0, 0].iter().enumerate() {
            let cell = table + 64 + index as u32 * 4;
            self.w32(table + index as u32 * 4, cell);
            self.w32(cell, *value);
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
            BitmapArrays => 19,
            SmsCfgBuf => 20,
            CInternalTable => 21,
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
        self.set_data(DataSlot::LgMemFree, cell);
        self.w32(cell, self.heap_free_head);
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

    pub fn heap_malloc(&mut self, requested: u32) -> u32 {
        if requested == 0 || requested > HEAP_MAX_ALLOC {
            return 0;
        }
        let len = requested.max(HEAP_MIN_PIECE).div_ceil(8) * 8;
        if len > self.heap_left {
            return 0;
        }
        // Walk the free list: head sentinel lives at HEAP_BASE in the sense
        // that `heap_free_head` is the offset of the first free block, and
        // each free block stores {next: u32, size: u32} at its start.
        // Guest use-after-free or interior frees can corrupt the list into
        // a cycle, so the walk is bounded and charged against the survey
        // budget; a corrupt list fails the allocation.
        let end = HEAP_LEN;
        let mut prev_next: Option<u32> = None;
        let mut cursor = self.heap_free_head;
        let mut steps = 0u32;
        while cursor < end {
            steps += 1;
            if steps > HEAP_LEN / HEAP_MIN_PIECE || charge_instruction() {
                return 0;
            }
            let next = self.r32(HEAP_BASE + cursor);
            let size = self.r32(HEAP_BASE + cursor + 4);
            if size >= len {
                let remaining = size - len;
                let address = HEAP_BASE + cursor;
                if remaining > HEAP_MIN_PIECE {
                    // Split: the tail stays on the free list.
                    let cut = cursor + len;
                    self.w32(HEAP_BASE + cut, next);
                    self.w32(HEAP_BASE + cut + 4, remaining);
                    match prev_next {
                        Some(previous) => {
                            self.w32(HEAP_BASE + previous, cut);
                        }
                        None => self.heap_free_head = cut,
                    }
                } else {
                    match prev_next {
                        Some(previous) => {
                            self.w32(HEAP_BASE + previous, next);
                        }
                        None => self.heap_free_head = next,
                    }
                }
                self.heap_left = self.heap_left.saturating_sub(len);
                self.heap_min = self.heap_min.min(self.heap_left);
                self.heap_top = self.heap_top.max(self.heap_left);
                self.sync_lg_slots();
                if std::env::var("MRP_TRACE").is_ok() {
                    eprintln!(
                        "[malloc] {len} -> {address:#010x} (left {:#x})",
                        self.heap_left
                    );
                }
                return address;
            }
            prev_next = Some(cursor);
            cursor = next;
        }
        0
    }

    fn heap_free(&mut self, address: u32, requested: u32) {
        if requested == 0 || !(HEAP_BASE..HEAP_BASE + HEAP_LEN).contains(&address) {
            return;
        }
        let len = requested.max(HEAP_MIN_PIECE).div_ceil(8) * 8;
        if address + len > HEAP_BASE + HEAP_LEN {
            return;
        }
        let cursor = address - HEAP_BASE;
        // Insert in address order and coalesce with both neighbours — the
        // reference allocator's layout, which probing guests depend on when
        // they walk the exported pool state.
        let end = HEAP_LEN;
        let mut prev: Option<u32> = None;
        let mut node = self.heap_free_head;
        let mut steps = 0u32;
        while node < end && node < cursor {
            steps += 1;
            if steps > HEAP_LEN / HEAP_MIN_PIECE || charge_instruction() {
                return; // corrupt list: refuse the free
            }
            prev = Some(node);
            node = self.r32(HEAP_BASE + node);
        }
        // Merge with the next node when adjacent, then with the previous —
        // both merges compose into one node (the reference allocator's
        // my_free does the same two-step coalesce).
        let mut merged_len = len;
        let mut next_node = node;
        if node < end && cursor + len == node {
            merged_len += self.r32(HEAP_BASE + node + 4);
            next_node = self.r32(HEAP_BASE + node);
        }
        let mut prev_merged = false;
        if let Some(p) = prev {
            let prev_size = self.r32(HEAP_BASE + p + 4);
            if p + prev_size == cursor {
                self.w32(HEAP_BASE + p, next_node);
                self.w32(HEAP_BASE + p + 4, prev_size + merged_len);
                prev_merged = true;
            }
        }
        if !prev_merged {
            match prev {
                Some(p) => self.w32(HEAP_BASE + p, cursor),
                None => self.heap_free_head = cursor,
            }
            self.w32(HEAP_BASE + cursor, next_node);
            self.w32(HEAP_BASE + cursor + 4, merged_len);
        }
        self.heap_left = self.heap_left.saturating_add(len);
        self.sync_lg_slots();
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
        let mut ring: Vec<(u32, u32, u32)> = Vec::new();
        let mut prev_pc = self.cpu.reg_get(self.cpu.mode(), reg::PC);
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
            if pc == SEND_APP_EVENT_ADDR {
                // The chunk's sendAppEvent callback: (ext, chunk, code,
                // timer, param1-on-stack) — code 0 starts the platform timer
                // with the stack argument as the interval, code 1 stops it.
                let code = self.cpu.reg_get(self.cpu.mode(), R2) as i32;
                let interval = self.stack_arg(0) as i32;
                if std::env::var("MRP_TRACE").is_ok() {
                    eprintln!("[sendAppEvent] code={code} interval={interval}");
                }
                match code {
                    0 => {
                        let interval = interval.max(1) as u64;
                        self.timers.retain(|timer| timer.id != 0);
                        self.timers.push(GuestTimer {
                            id: 0,
                            interval: Duration::from_millis(interval),
                            next_fire: Instant::now() + Duration::from_millis(interval),
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
            if (BRIDGE_BASE..bridge_end).contains(&pc) {
                let slot = ((pc - BRIDGE_BASE) / 4) as usize;
                if slot >= TABLE_SLOTS || SLOTS[slot] != Slot::Func {
                    return Err(MrpError::Unsupported(format!(
                        "guest jumped into non-function table slot {slot}"
                    )));
                }
                self.slot_calls[slot] += 1;
                if std::env::var("MRP_TRACE").is_ok() {
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
            if pc >= MEM_SIZE || if thumb { (pc & 1) != 0 } else { (pc & 3) != 0 } {
                return Err(MrpError::Unsupported(format!(
                    "guest PC out of range: {pc:#010x}"
                )));
            }
            if std::env::var("MRP_TRACE").is_ok() && executed < 80000 {
                let inst = self.r32(pc);
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
            let inst = self.r32(pc);
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
            // emulator in debug builds. Encoding:
            // cond 000 P U I W L Rn Rd imm4H 1101 imm4L.
            let inst = self.r32(pc);
            if !thumb && (inst & 0x0E00_00F0) == 0x0000_00D0 {
                let cond_ok = self.arm_cond_passed(inst >> 28);
                if cond_ok {
                    let pre_index = inst & 0x0200_0000 != 0; // P
                    let up = inst & 0x0080_0000 != 0; // U
                    let writeback = inst & 0x0020_0000 != 0; // W
                    let load = inst & 0x0010_0000 != 0; // L
                    let rn = (inst >> 16) & 0xF;
                    let rd = (inst >> 12) & 0xF;
                    let imm = (((inst >> 8) & 0xF) << 4) | (inst & 0xF);
                    let offset = (imm * 4) as i32;
                    let base = if rn == 15 {
                        pc.wrapping_add(8)
                    } else {
                        self.cpu.reg_get(self.cpu.mode(), rn as reg::Reg)
                    };
                    let target = if up {
                        base.wrapping_add(offset as u32)
                    } else {
                        base.wrapping_sub(offset as u32)
                    };
                    let addr = if pre_index { target } else { base };
                    let aligned = addr & !3;
                    let rd_pair = rd & !1;
                    if load {
                        let low = self.r32(aligned);
                        let high = self.r32(aligned + 4);
                        self.cpu.reg_set(self.cpu.mode(), rd_pair as reg::Reg, low);
                        self.cpu
                            .reg_set(self.cpu.mode(), (rd_pair + 1) as reg::Reg, high);
                    } else {
                        let low = self.cpu.reg_get(self.cpu.mode(), rd_pair as reg::Reg);
                        let high = self.cpu.reg_get(self.cpu.mode(), (rd_pair + 1) as reg::Reg);
                        self.w32(aligned, low);
                        self.w32(aligned + 4, high);
                    }
                    if (!pre_index || writeback) && rn != 15 {
                        self.cpu.reg_set(self.cpu.mode(), rn as reg::Reg, target);
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
            let inst = self.r32(pc);
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
            let inst = self.r32(pc);
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
        Err(MrpError::Unsupported(format!(
            "guest instruction limit exceeded after {limit}; hot slots [{}]",
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
        // The handler's trailing arguments (`uint8** output`,
        // `int32* output_len`) live on the stack at SP+0/SP+4 as seen at
        // handler entry. run_guest resets SP to STACK_TOP, so lower the
        // entry SP to make room for the handler's own frame, park the two
        // slot words above the handler's frame region, and point them at
        // heap scratch buffers that survive any stack depth.
        let scratch1 = self.heap_malloc(8);
        let scratch2 = self.heap_malloc(8);
        if scratch1 == 0 || scratch2 == 0 {
            return Err(MrpError::Unsupported(
                "call_c_function: heap exhausted for output scratch".to_string(),
            ));
        }
        self.w32(scratch1, 0);
        self.w32(scratch2, 0);
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
        self.w32(HANDLER_SP, scratch1);
        self.w32(HANDLER_SP + 4, scratch2);
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
                // realloc: fresh block, copy, release the old one.
                let old = self.cpu.reg_get(self.cpu.mode(), R0);
                let len = self.cpu.reg_get(self.cpu.mode(), R1);
                let new = self.heap_malloc(len);
                if new != 0 && old != 0 {
                    let count = len.min(HEAP_MAX_ALLOC) as usize;
                    let src = old as usize;
                    let dst = new as usize;
                    let bytes: Vec<u8> = self.mem[src..src + count].to_vec();
                    self.mem[dst..dst + count].copy_from_slice(&bytes);
                    self.heap_free(old, len);
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
                self.w32(self.load_base() + 4, block);
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
                // mr_drawBitmap(bmp, x, y, w, h): official table slot 29.
                // The helper's mrc_refreshScreen routes through this slot
                // with the screen buffer as the source — present it too.
                let src = self.cpu.reg_get(self.cpu.mode(), R0);
                let x = self.cpu.reg_get(self.cpu.mode(), R1);
                let y = self.cpu.reg_get(self.cpu.mode(), R2);
                let w = self.cpu.reg_get(self.cpu.mode(), R3);
                let h = self.stack_arg(0);
                self.blit_from_address(src, x, y, w, h);
                if src == SCREENBUF {
                    self.refresh_screen(screen);
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
                    next_fire: Instant::now() + Duration::from_millis(u64::from(interval)),
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
            33 => self.started_at.elapsed().as_millis() as u32 as i32,
            34 => {
                // mr_getDatetime(struct): year, month, day, hour, minute, second.
                let buffer = self.cpu.reg_get(self.cpu.mode(), R0);
                let now = local_datetime();
                self.w32(buffer, now.0);
                self.w32(buffer + 4, now.1);
                self.w32(buffer + 8, now.2);
                self.w32(buffer + 12, now.3);
                self.w32(buffer + 16, now.4);
                self.w32(buffer + 20, now.5);
                MR_SUCCESS
            }
            35 => {
                // mr_getUserInfo: IMEI, IMSI, vendor, device strings.
                let buffer = self.cpu.reg_get(self.cpu.mode(), R0);
                let imei = self.config.imei.clone();
                let imsi = self.config.imsi.clone();
                let vendor = self.config.vendor.clone();
                let device = self.config.device.clone();
                for (offset, text) in [(0, imei), (16, imsi), (32, vendor), (48, device)] {
                    let bytes = text.as_bytes();
                    let len = bytes.len().min(15);
                    let base = (buffer + offset) as usize;
                    self.mem[base..base + len].copy_from_slice(&bytes[..len]);
                    self.mem[base + len] = 0;
                }
                MR_SUCCESS
            }
            36 => MR_SUCCESS, // mr_sleep: instantaneous in the emulator
            37 => MR_IGNORE,  // mr_plat
            38 => MR_IGNORE,  // mr_platEx
            39 => 0,          // mr_ferrno
            40 => self.swi_open(),
            41 => self.swi_close(),
            42 => self.swi_info(),
            43 => self.swi_write(),
            44 => self.swi_read(),
            45 => self.swi_seek(),
            46 => self.swi_get_len(),
            47 => MR_FAILED, // mr_remove
            48 => MR_FAILED, // mr_rename
            49 => MR_FAILED, // mr_mkDir
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
            // mr_playSound(type, data, len, loop): decode the guest's WAV
            // bytes and hand the PCM clip to the audio sink. Type 1 (MIDI)
            // is logged and skipped until a synth exists.
            57 => {
                let sound_type = self.cpu.reg_get(self.cpu.mode(), R0);
                let data = self.cpu.reg_get(self.cpu.mode(), R1);
                let len = self.cpu.reg_get(self.cpu.mode(), R2);
                let looped = self.cpu.reg_get(self.cpu.mode(), R3) != 0;
                let bytes = self.guest_bytes(data, len.min(4 * 1024 * 1024) as usize);
                match sound_type {
                    0 => match bytes.and_then(|bytes| crate::audio::decode_wav(&bytes).ok()) {
                        Some(clip) => {
                            self.audio_sink.play_bgm(clip, looped);
                        }
                        None => {
                            warn_once("audio", "mr_playSound: WAV decode failed".to_string());
                        }
                    },
                    other => {
                        warn_once("audio", format!("mr_playSound type {other} unsupported"));
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
            // and GetText return success. This lets ext code that builds
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
            75 => {
                self.next_gui_handle += 1;
                self.next_gui_handle as i32
            }
            76 => MR_SUCCESS, // editRelease
            77 => 0,          // editGetText → empty string (nil terminated at r0)
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
            81..=90 => {
                warn_once(
                    "net",
                    format!("mr_table slot {slot} (network) is offline; failing"),
                );
                MR_FAILED
            }
            113..=117 => 0, // md5 / sms cfg: tolerated stubs
            119 => {
                // mr_drawPoint(x, y, color): draw into the guest buffer.
                let x = self.cpu.reg_get(self.cpu.mode(), R0);
                let y = self.cpu.reg_get(self.cpu.mode(), R1);
                let color = rgb_to_565(self.cpu.reg_get(self.cpu.mode(), R2));
                let width = u32::from(self.config.screen_width);
                if x < width {
                    self.w16(SCREENBUF + (y * width + x) * 2, color);
                }
                MR_SUCCESS
            }
            122 => {
                // mr_drawRect(x1, y1, x2, y2, r, g, b)
                let x1 = self.cpu.reg_get(self.cpu.mode(), R0);
                let y1 = self.cpu.reg_get(self.cpu.mode(), R1);
                let x2 = self.cpu.reg_get(self.cpu.mode(), R2);
                let y2 = self.cpu.reg_get(self.cpu.mode(), R3);
                let r = self.stack_arg(0) & 0xFF;
                let g = self.stack_arg(1) & 0xFF;
                let b = self.stack_arg(2) & 0xFF;
                let color = rgb888_to_565(r, g, b);
                let width = u32::from(self.config.screen_width);
                let height = u32::from(self.config.screen_height);
                for y in y1.min(height)..y2.min(height) {
                    for x in x1.min(width)..x2.min(width) {
                        self.w16(SCREENBUF + (y * width + x) * 2, color);
                    }
                }
                MR_SUCCESS
            }
            123 | 128 => {
                // mr_drawText(text, x, y, r, g, b, ...) — GBK text rendering.
                let text_addr = self.cpu.reg_get(self.cpu.mode(), R0);
                let x = self.cpu.reg_get(self.cpu.mode(), R1) as i32;
                let y = self.cpu.reg_get(self.cpu.mode(), R2) as i32;
                let text = self.guest_cstring(text_addr, 512);
                let (r, g, b) = (
                    self.stack_arg(0) & 0xFF,
                    self.stack_arg(1) & 0xFF,
                    self.stack_arg(2) & 0xFF,
                );
                let color = rgb888_to_565(r, g, b);
                self.draw_text(&text, x, y, color);
                MR_SUCCESS
            }
            145 => {
                // mr_platDrawChar(ch, x, y, r, g, b)
                let ch = self.cpu.reg_get(self.cpu.mode(), R0) as u8 as char;
                let x = self.cpu.reg_get(self.cpu.mode(), R1) as i32;
                let y = self.cpu.reg_get(self.cpu.mode(), R2) as i32;
                let (r, g, b) = (
                    self.stack_arg(0) & 0xFF,
                    self.stack_arg(1) & 0xFF,
                    self.stack_arg(2) & 0xFF,
                );
                let mut buf = [0u8; 3];
                ch.encode_utf8(&mut buf);
                let color = rgb888_to_565(r, g, b);
                self.draw_text(&buf[..ch.len_utf8()], x, y, color);
                MR_SUCCESS
            }
            120 => {
                // mr_drawBitmap(p, x, y, w, h, rop, transcolor, sx, sy, mw):
                // the ten-argument sprite blit (rop modes, rotate/flip).
                let p = self.cpu.reg_get(self.cpu.mode(), R0);
                let x = self.cpu.reg_get(self.cpu.mode(), R1) as i32;
                let y = self.cpu.reg_get(self.cpu.mode(), R2) as i32;
                let w = self.cpu.reg_get(self.cpu.mode(), R3);
                let h = self.stack_arg(0);
                let rop = self.stack_arg(1);
                let transcolor = self.stack_arg(2) as u16;
                let sx = self.stack_arg(3) as i32;
                let sy = self.stack_arg(4) as i32;
                let mw = self.stack_arg(5);
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
                self.refresh_screen(screen);
                MR_SUCCESS
            }
            124 => 0, // mr_bitmapCheck
            125 => self.swi_read_file(),
            126 => 0, // mr_wstrlen
            127 => 0, // mr_registerAPP
            129 => 0, // mr_effSetCon
            130 => {
                // mr_testCom(state, input0, input1)
                let input0 = self.cpu.reg_get(self.cpu.mode(), R1);
                let input1 = self.cpu.reg_get(self.cpu.mode(), R2);
                self.dispatch_test_com(input0, input1)
            }
            131 => MR_IGNORE, // mr_testCom1
            132 => 0,         // mr_c2u
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
            141 => {
                // mr_unzip(inputBuf, inputLen, &outputBuf, &outputLen):
                // gunzip into a fresh malloc'd buffer; the ext frees it.
                let input_buf = self.cpu.reg_get(self.cpu.mode(), R0);
                let input_len = self.cpu.reg_get(self.cpu.mode(), R1) as usize;
                let out_buf_ptr = self.cpu.reg_get(self.cpu.mode(), R2);
                let out_len_ptr = self.cpu.reg_get(self.cpu.mode(), R3);
                let Some(input) = self.guest_bytes(input_buf, input_len) else {
                    return MR_FAILED;
                };
                let Some(output) = gunzip_any(&input) else {
                    warn_once("unzip", "mr_unzip: payload is not gzip/zlib".to_string());
                    return MR_FAILED;
                };
                let block = self.heap_malloc(output.len() as u32);
                if block == 0 {
                    return MR_FAILED;
                }
                let base = block as usize;
                self.mem[base..base + output.len()].copy_from_slice(&output);
                self.w32(out_buf_ptr, block);
                self.w32(out_len_ptr, output.len() as u32);
                MR_SUCCESS
            }
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
        self.data_slot_address(DataSlot::CInternalTable) + 64 + index * 4
    }

    fn load_base(&self) -> u32 {
        EXT_BASE
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
            3 => {
                // memcpy(dst, src, len)
                if let (Some(dst), Some(src)) = ((r0 as usize..).next(), (r1 as usize..).next()) {
                    let count = r2 as usize;
                    if src + count <= self.mem.len() && dst + count <= self.mem.len() {
                        let bytes = self.mem[src..src + count].to_vec();
                        self.mem[dst..dst + count].copy_from_slice(&bytes);
                    }
                }
                r0 as i32
            }
            4 => {
                // memmove: copy through a temporary.
                let count = r2 as usize;
                if r1 as usize + count <= self.mem.len() && r0 as usize + count <= self.mem.len() {
                    let bytes = self.mem[r1 as usize..r1 as usize + count].to_vec();
                    self.mem[r0 as usize..r0 as usize + count].copy_from_slice(&bytes);
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
                let a = self.guest_cstring_or_fixed(r0, r2, slot == 9);
                let b = self.guest_cstring_or_fixed(r1, r2, slot == 9);
                let limit = match slot {
                    11 => r2 as usize,
                    9 => a.len(),
                    _ => usize::MAX,
                };
                let result = a
                    .iter()
                    .zip(b.iter())
                    .take(limit)
                    .find_map(|(x, y)| match x.cmp(y) {
                        std::cmp::Ordering::Equal => None,
                        other => Some(other),
                    })
                    .unwrap_or(std::cmp::Ordering::Equal);
                match result {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Greater => 1,
                    std::cmp::Ordering::Equal => 0,
                }
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

    fn guest_cstring_or_fixed(&self, addr: u32, len: u32, fixed: bool) -> Vec<u8> {
        if fixed {
            let start = addr as usize;
            let end = self.mem.len().min(start + len as usize);
            return self.mem[start..end].to_vec();
        }
        self.guest_cstring(addr, 4096)
    }

    pub fn refresh_screen(&mut self, screen: &mut crate::display::Screen) {
        let width = u32::from(self.config.screen_width);
        let height = u32::from(self.config.screen_height);
        for y in 0..height {
            for x in 0..width {
                let pixel = self.r16(SCREENBUF + (y * width + x) * 2);
                screen.set_pixel(x as u16, y as u16, pixel);
            }
        }
    }

    fn swi_open(&mut self) -> i32 {
        let path = self.guest_cstring(self.cpu.reg_get(self.cpu.mode(), R0), 512);
        let name = String::from_utf8_lossy(&path).to_string();
        let mode = self.cpu.reg_get(self.cpu.mode(), R1);
        log::info!("mr_open({name:?}, mode={mode:#x})");
        // Missing files fail with handle 0 unless MR_FILE_CREATE (8) asks
        // for creation — mirrors the on-device contract (`if (f)`).
        let data = match self.file_lookup(&name) {
            Some(data) => data,
            None if mode & 8 != 0 => Vec::new(),
            None => return 0,
        };
        self.open_files.push(OpenFile {
            name,
            data,
            pos: 0,
            closed: false,
        });
        (self.open_files.len() as u32) as i32
    }

    fn swi_close(&mut self) -> i32 {
        let handle = self.cpu.reg_get(self.cpu.mode(), R0) as usize;
        if handle == 0 || handle > self.open_files.len() {
            return -1;
        }
        self.open_files[handle - 1].closed = true;
        // Sync the (possibly written) data back to the file table so
        // subsequent mr_open calls see the updated content.
        let file = &self.open_files[handle - 1];
        if let Some(entry) = self.files.iter_mut().find(|(name, _)| *name == file.name) {
            entry.1 = file.data.clone();
        } else {
            self.dirty_files.insert(file.name.clone());
        }
        0
    }

    fn swi_info(&mut self) -> i32 {
        // mr_info(path) returns the file size, or MR_FAILED (-1).
        let path = self.guest_cstring(self.cpu.reg_get(self.cpu.mode(), R0), 512);
        let name = String::from_utf8_lossy(&path).to_string();
        let result = match self.file_lookup(&name) {
            Some(data) => data.len() as i32,
            None => -1,
        };
        log::info!("mr_info({name:?}) -> {result}");
        result
    }

    fn swi_write(&mut self) -> i32 {
        // Writes land in the handle's buffer; persistence lands with the
        // guest file system work.
        let handle = self.cpu.reg_get(self.cpu.mode(), R0) as usize;
        let len = self.cpu.reg_get(self.cpu.mode(), R2) as usize;
        if handle == 0 || handle > self.open_files.len() {
            return -1;
        }
        len as i32
    }

    fn swi_read(&mut self) -> i32 {
        let handle = self.cpu.reg_get(self.cpu.mode(), R0) as usize;
        let buffer = self.cpu.reg_get(self.cpu.mode(), R1);
        let len = self.cpu.reg_get(self.cpu.mode(), R2) as usize;
        if handle == 0 || handle > self.open_files.len() || self.open_files[handle - 1].closed {
            return -1;
        }
        let file = &mut self.open_files[handle - 1];
        let remaining = file.data.len().saturating_sub(file.pos);
        let count = len.min(remaining);
        log::info!(
            "mr_read(handle={handle} {:?} pos={:#x} len={count})",
            file.name,
            file.pos
        );
        let bytes = file.data[file.pos..file.pos + count].to_vec();
        file.pos += count;
        let base = buffer as usize;
        if base + count <= self.mem.len() {
            self.mem[base..base + count].copy_from_slice(&bytes);
        }
        count as i32
    }

    fn swi_seek(&mut self) -> i32 {
        let handle = self.cpu.reg_get(self.cpu.mode(), R0) as usize;
        let offset = self.cpu.reg_get(self.cpu.mode(), R1) as i32;
        let whence = self.cpu.reg_get(self.cpu.mode(), R2) as i32;
        if handle == 0 || handle > self.open_files.len() {
            return -1;
        }
        let file = &mut self.open_files[handle - 1];
        let len = file.data.len() as i64;
        let base = match whence {
            0 => 0,
            1 => file.pos as i64,
            _ => len,
        };
        let target = (base + i64::from(offset)).clamp(0, len);
        log::info!("mr_seek(handle={handle} offset={offset} whence={whence} -> {target:#x})",);
        file.pos = target as usize;
        file.pos as i32
    }

    fn swi_get_len(&mut self) -> i32 {
        let handle = self.cpu.reg_get(self.cpu.mode(), R0) as usize;
        match self.open_files.get(handle.wrapping_sub(1)) {
            Some(file) => file.data.len() as i32,
            None => -1,
        }
    }

    /// `mr_readFile(name, buf, *len, lookfor)` — reads from the running
    /// package first (gzip-aware), then the virtual file system.
    fn swi_read_file(&mut self) -> i32 {
        // mr_readFile(name, &filelen, lookfor) → malloc'd buffer pointer
        // (or 0). lookfor 1 asks for the ram file only, which this emulator
        // does not host; everything else resolves from the package.
        let name_addr = self.cpu.reg_get(self.cpu.mode(), R0);
        let len_ptr = self.cpu.reg_get(self.cpu.mode(), R1);
        let lookfor = self.cpu.reg_get(self.cpu.mode(), R2);
        let name = String::from_utf8_lossy(&self.guest_cstring(name_addr, 512)).to_string();
        let content = if lookfor == 1 {
            None
        } else {
            self.file_lookup(&name)
        };
        log::info!(
            "mr_readFile({name:?}, lookfor={lookfor}) -> {} bytes",
            content.as_ref().map(|data| data.len()).unwrap_or(0)
        );
        let Some(content) = content else {
            self.w32(len_ptr, 0);
            return 0;
        };
        let block = self.heap_malloc(content.len() as u32);
        if block == 0 {
            return 0;
        }
        let base = block as usize;
        self.mem[base..base + content.len()].copy_from_slice(&content);
        self.w32(len_ptr, content.len() as u32);
        block as i32
    }

    /// Looks up `name` in the file table seeded at boot (package members
    /// and VFS content). Falls back to a basename match so guests opening
    /// `mythroad/x.png` find the packaged `x.png`.
    pub fn file_lookup(&self, name: &str) -> Option<Vec<u8>> {
        let name = name.trim_start_matches('/');
        if let Some((_, data)) = self.files.iter().find(|(entry, _)| entry == name) {
            return Some(data.clone());
        }
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        self.files
            .iter()
            .find(|(entry, _)| entry.ends_with(base))
            .map(|(_, data)| data.clone())
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
                self.bi |= 0x4;
                0
            }
            0x12f => {
                self.bi &= !0x4;
                0
            }
            0x130 => {
                self.bi |= 0x8;
                0
            }
            0x131 => {
                self.bi &= !0x8;
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
                self.bi |= 0x1;
                0
            }
            0xf51 if input1 == 0x18030 => {
                self.bi |= 0x2;
                0
            }
            0xcb3 if input1 == 0x9E67A => {
                self.bi &= !0x2;
                0
            }
            // Secondary ext load (the loader-chain flow): the ext hands the
            // platform a buffer holding a full ext image; the runtime writes
            // the function-table pointer at its base and executes
            // buffer+8 with r0 = code. This mirrors the platform's
            // `_mr_TestCom` case 800.
            800 => {
                // The C-level `_mr_TestCom(?, 800, buf)` reads its code via
                // `mr_L_optint(L, 3, 0)` — no Lua frame on this path, so the
                // platform default 0 applies.
                let buffer = input1;
                let code = 0u32; // mr_L_optint(L, 3, 0) platform default
                let _ = code;
                if buffer == 0 {
                    return -1; // MR_FAILED
                }
                self.w32(buffer, TABLE_BASE);
                let entry = buffer + 8;
                let mut screen = crate::display::Screen::new(
                    self.config.screen_width,
                    self.config.screen_height,
                );
                let result = self
                    .run_guest(entry, DEFAULT_INSTRUCTION_LIMIT, &mut screen)
                    .map(|value| value as i32)
                    .unwrap_or(-1); // MR_FAILED
                self.refresh_screen(&mut screen);
                result
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
        self.mem[base..base + ext.len()].copy_from_slice(ext);
        self.w32(addr, TABLE_BASE);
        self.cpu.reg_set(self.cpu.mode(), R0, code as u32);
        let result = self
            .run_guest(addr + 8, DEFAULT_INSTRUCTION_LIMIT, screen)
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
        self.w32(chunk + 0x14, 0); // var_buf
        self.w32(chunk + 0x18, 0); // var_len
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
        self.mem[base..base + ext.len()].copy_from_slice(ext);
        self.w32(EXT_BASE, TABLE_BASE);

        // mr_testCom(0, 0xe2d, 0xb61): the loader's flag handshake.
        self.bi |= 0x1;

        // mr_c_function_load(code): the ext compares r0 against 1 and only
        // takes the mr_c_function_new registration path when the argument
        // is 1 (observed in the reference ext's disassembly); other values
        // jump to an alternate entry.
        self.cpu.reg_set(self.cpu.mode(), R0, 1);
        self.run_guest(EXT_BASE + 8, DEFAULT_INSTRUCTION_LIMIT, screen)?;
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
        self.call_c_function(6, EXT_BASE, 0x7cd, DEFAULT_INSTRUCTION_LIMIT, screen)?;
        // code 8: application info block (id, version, sidName, reserved).
        let app_info = self.heap_malloc(16);
        self.w32(app_info, app_id);
        self.w32(app_info + 4, app_version);
        self.call_c_function(8, app_info, 16, DEFAULT_INSTRUCTION_LIMIT, screen)?;
        // code 0: mrc_init.
        self.call_c_function(0, EXT_BASE, 0x7cd, DEFAULT_INSTRUCTION_LIMIT, screen)?;
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
            // mr_com_event_t { code, param0, param1 } allocated on the heap.
            let block = self.heap_malloc(12);
            self.w32(block, event_type as u32);
            self.w32(block + 4, param0 as u32);
            self.w32(block + 8, param1 as u32);
            return self.call_c_function(1, block, 12, DEFAULT_INSTRUCTION_LIMIT, screen);
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
        if self.c_function != 0 {
            return self.call_c_function(2, 0, 0, DEFAULT_INSTRUCTION_LIMIT, screen);
        }
        if self.timer_function != 0 {
            return self
                .run_guest(self.timer_function, DEFAULT_INSTRUCTION_LIMIT, screen)
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
        if let Some(timer) = self.timers.iter_mut().find(|timer| timer.id == id) {
            timer.interval = interval;
            timer.next_fire = std::time::Instant::now() + interval;
        } else {
            self.timers.push(GuestTimer {
                id,
                interval,
                next_fire: std::time::Instant::now() + interval,
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
        let width = u32::from(self.config.screen_width);
        if x < width {
            self.w16(SCREENBUF + (y * width + x) * 2, rgb_to_565(color));
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_rect(&mut self, x1: u32, y1: u32, x2: u32, y2: u32, r: u32, g: u32, b: u32) {
        let color = rgb888_to_565(r, g, b);
        let width = u32::from(self.config.screen_width);
        let height = u32::from(self.config.screen_height);
        for y in y1.min(height)..y2.min(height) {
            for x in x1.min(width)..x2.min(width) {
                self.w16(SCREENBUF + (y * width + x) * 2, color);
            }
        }
    }

    pub fn fill_screenbuf(&mut self, color: u16) {
        let width = u32::from(self.config.screen_width);
        let height = u32::from(self.config.screen_height);
        for y in 0..height {
            for x in 0..width {
                self.w16(SCREENBUF + (y * width + x) * 2, color);
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
        let width = u32::from(self.config.screen_width);
        let height = u32::from(self.config.screen_height);
        for y in 0..height {
            for x in 0..width {
                let color = if y < 20 { 0x001F } else { 0xFFFF };
                self.w16(SCREENBUF + (y * width + x) * 2, color);
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
                        self.w16(SCREENBUF + py * 2, 0x001F);
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
        let mut cursor_x = x;
        for ch in decoded.chars() {
            let Some(glyph) = unifont::get_glyph(ch) else {
                cursor_x += 8;
                continue;
            };
            let width = glyph.get_width() as i32;
            let s_width = u32::from(self.config.screen_width);
            let s_height = u32::from(self.config.screen_height);
            for row in 0..16u32 {
                for col in 0..width {
                    if glyph.get_pixel(col as usize, row as usize) {
                        let px = cursor_x + col;
                        let py = y + row as i32;
                        if px >= 0 && py >= 0 && (px as u32) < s_width && (py as u32) < s_height {
                            self.w16(SCREENBUF + (py as u32 * s_width + px as u32) * 2, color);
                        }
                    }
                }
            }
            cursor_x += width;
        }
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
                self.w16(SCREENBUF + (py * s_width + px) * 2, *pixel);
            }
        }
    }

    /// Blits a rectangle of raw RGB565 pixels from guest memory at `src`.
    pub fn blit_from_address(&mut self, src: u32, x: u32, y: u32, w: u32, h: u32) {
        let s_width = u32::from(self.config.screen_width);
        let s_height = u32::from(self.config.screen_height);
        for row in 0..h {
            for col in 0..w {
                let px = x + col;
                let py = y + row;
                if px < s_width && py < s_height {
                    let pixel = self.r16(src + (row * w + col) * 2);
                    self.w16(SCREENBUF + (py * s_width + px) * 2, pixel);
                }
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
        let data = self.file_lookup(name)?;
        self.open_files.push(OpenFile {
            name: name.to_string(),
            data,
            pos: 0,
            closed: false,
        });
        Some(self.open_files.len())
    }

    pub fn file_read(&mut self, handle: usize, len: usize) -> Option<Vec<u8>> {
        let file = self.open_files.get_mut(handle.wrapping_sub(1))?;
        if file.closed {
            return None;
        }
        let remaining = file.data.len().saturating_sub(file.pos);
        let count = len.min(remaining);
        let bytes = file.data[file.pos..file.pos + count].to_vec();
        file.pos += count;
        Some(bytes)
    }

    pub fn file_close(&mut self, handle: usize) {
        if let Some(file) = self.open_files.get_mut(handle.wrapping_sub(1)) {
            file.closed = true;
        }
    }

    pub fn file_len(&self, handle: usize) -> Option<usize> {
        Some(self.open_files.get(handle.wrapping_sub(1))?.data.len())
    }

    /// Writes bytes at the current position; returns whether it landed.
    pub fn file_write(&mut self, handle: usize, data: &[u8]) -> bool {
        let Some(file) = self.open_files.get_mut(handle.wrapping_sub(1)) else {
            return false;
        };
        if file.closed {
            return false;
        }
        let end = file.pos + data.len();
        if end > file.data.len() {
            file.data.resize(end, 0);
        }
        file.data[file.pos..end].copy_from_slice(data);
        file.pos = end;
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
        let len = file.data.len() as i64;
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
        self.started_at.elapsed().as_millis() as u32
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
    /// same-named entries). Returns the number of files loaded.
    pub fn load_persistence(&mut self, dir: &std::path::Path) -> std::io::Result<usize> {
        let mut count = 0usize;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(0);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let Ok(data) = std::fs::read(&path) else {
                continue;
            };
            if let Some(existing) = self.files.iter_mut().find(|(entry, _)| *entry == name) {
                existing.1 = data;
            } else {
                self.files.push((name, data));
            }
            count += 1;
        }
        Ok(count)
    }

    /// Screen-buffer plot with clipping.
    fn screen_put(&mut self, x: i32, y: i32, color: u16) {
        let width = u32::from(self.config.screen_width);
        let height = u32::from(self.config.screen_height);
        if x >= 0 && y >= 0 && (x as u32) < width && (y as u32) < height {
            let (x, y) = (x as u32, y as u32);
            self.w16(SCREENBUF + (y * width + x) * 2, color);
        }
    }

    /// Reads a guest byte range (bounds-safe).
    pub fn guest_bytes(&self, addr: u32, len: usize) -> Option<Vec<u8>> {
        let base = addr as usize;
        if base + len <= self.mem.len() {
            Some(self.mem[base..base + len].to_vec())
        } else {
            None
        }
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
        let _ = (sx, sy, mw); // source-window form only used by tile draws
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
        let screen_stride = u32::from(self.config.screen_width);
        let dst = |x: u32, y: u32| SCREENBUF + (y * screen_stride + x) * 2;
        for dy in min_y..max_y {
            for dx in min_x..max_x {
                let lx = dx - x;
                let ly = dy - y;
                let src = if sprite_path && rotate != 0 {
                    let (row, col) = match (rotate, flip) {
                        (1, false) => (lx, w - 1 - ly),
                        (1, true) => (h - 1 - lx, w - 1 - ly),
                        (2, false) => (h - 1 - ly, w - 1 - lx),
                        (2, true) => (ly, lx),
                        (3, false) => (w - 1 - lx, ly),
                        _ => (lx, ly),
                    };
                    p + ((row.max(0) * w + col.max(0)) as u32) * 2
                } else {
                    p + ((ly.max(0) * w + lx.max(0)) as u32) * 2
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
        let width = u32::from(self.config.screen_width);
        let height = u32::from(self.config.screen_height);
        let plot = |guest: &mut Self, x: i32, y: i32| {
            if x >= 0 && y >= 0 && (x as u32) < width && (y as u32) < height {
                let (x, y) = (x as u32, y as u32);
                guest.w16(SCREENBUF + (y * width + x) * 2, color);
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

/// Gunzip (or raw-zlib) decompression for `mr_unzip`; accepts both
/// containers since the platform calls sites pass gzip members.
fn gunzip_any(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    if data.len() >= 2 && data[0] == 0x1F && data[1] == 0x8B {
        let mut decoder = flate2::read::GzDecoder::new(data);
        let mut out = Vec::new();
        return decoder.read_to_end(&mut out).ok().map(|_| out);
    }
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok().map(|_| out)
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
            "../../tmp/scratch/Maze.mrp",
            "../../tmp/vmrp/mrc/asm/asm.mrp",
            "../../tmp/vmrp/wasm/dist/fs/mythroad/ydqtwo.mrp",
        ];
        let _ = std::fs::create_dir_all("../../tmp/scratch/survey");
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
            write_screen_bmp(&machine, &format!("../../tmp/scratch/survey/{stem}.bmp"));
        }
    }

    /// Diagnostic: boot Maze.mrp, run 60 frames, dump all slot calls with
    /// return values, screen color histogram, and missing natives.
    #[test]
    #[ignore = "requires the local reference corpus under tmp/"]
    fn maze_slot_trace() {
        let _ = env_logger::try_init();
        let mut machine = crate::emulator::MrpMachine::from_path("../../tmp/scratch/Maze.mrp")
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
        let _ = std::fs::write("../../tmp/scratch/maze_screen.bmp", &bmp);
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
            "../../tmp/scratch/Maze.mrp",
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
}
