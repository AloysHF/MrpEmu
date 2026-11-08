//! The script bridge: runs `start.mr` on the MiniJ VM and exposes the
//! platform API surface as natives.
//!
//! The SDK's start.mr wrapper drives the whole application lifecycle from
//! the script side: it reads the ext images from the package, hands them to
//! the runtime through `_com(800, data, len, mode)` (which loads and
//! registers the ext), forwards lifecycle events with `_com(801, ...)`, and
//! defines globals such as `dealevent` that the runtime calls back into.
//! The natives here mirror the registrations of the reference runtime.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use crate::package::MrpPackage;
use crate::value_helpers as vh;
use crate::vm::{Value, Vm};

/// Sentinel native slot backing unknown globals: scripts probing optional
/// API surface get a callable that returns nothing instead of crashing.
pub struct ScriptBridge {
    pub vm: Vm,
    missing_globals: RefCell<HashSet<String>>,
}

impl Default for ScriptBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptBridge {
    /// Creates the bridge, registering the platform natives.
    pub fn new() -> Self {
        let mut bridge = Self {
            vm: Vm::new(),
            missing_globals: RefCell::new(HashSet::new()),
        };
        bridge.register_natives();
        bridge
    }

    /// Creates a bridge that wraps an already-booted guest (the direct
    /// ext path); the script never ran, so only guest-level pumping works.
    pub fn with_guest(guest: crate::cfunction::Guest, screen: crate::display::Screen) -> Self {
        let mut bridge = Self::new();
        bridge.vm.guest = Some(guest);
        bridge.vm.screen = screen;
        bridge
    }

    /// The reason `run_start_script` failed, if it was attempted and failed.
    pub fn run_start_script_failed_reason(&mut self) -> Option<String> {
        self.run_start_script().err()
    }

    /// Loads the package into the bridge: guest files, header data, and the
    /// script-visible package handle.
    pub fn load_package(&mut self, package: MrpPackage) {
        let files: Vec<(String, Vec<u8>)> = package
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.name.clone(),
                    package.read_entry(&entry.name).unwrap_or_default(),
                )
            })
            .collect();
        if let Some(guest) = self.vm.guest.as_mut() {
            guest.files = files;
        }
        self.vm.package = Some(package);
    }

    /// Creates the ext guest with the default platform configuration.
    pub fn create_guest(&mut self) {
        if self.vm.guest.is_none() {
            self.vm.guest = Some(crate::cfunction::Guest::new(
                crate::cfunction::GuestConfig::default(),
                Vec::new(),
            ));
        }
    }

    /// Undumps and runs `start.mr`.
    pub fn run_start_script(&mut self) -> Result<(), String> {
        let Some(package) = self.vm.package.as_ref() else {
            return Err("no package loaded".to_string());
        };
        let script = package
            .read_entry("start.mr")
            .map_err(|error| format!("start.mr unreadable: {error}"))?;
        let chunk = crate::undump::Chunk::parse(&script)
            .map_err(|error| format!("start.mr is not a valid chunk: {error}"))?;
        self.vm
            .run_chunk(Rc::new(chunk.proto))
            .map_err(|error| format!("start.mr failed: {}", error.0))
    }

    /// Calls the script's `dealevent(type, param0, param1)` global.
    pub fn send_event(&mut self, event_type: i32, p0: i32, p1: i32) -> Result<(), String> {
        let handler = self.vm.globals.borrow().get(&vh::str_value(b"dealevent"));
        if matches!(handler, Value::Nil) {
            return Ok(());
        }
        self.vm
            .call_global(
                "dealevent",
                &[
                    vh::num_value(event_type as f32),
                    vh::num_value(p0 as f32),
                    vh::num_value(p1 as f32),
                ],
            )
            .map(|_| ())
    }

    /// Globals the script referenced but the bridge does not implement.
    pub fn missing_globals(&self) -> Vec<String> {
        self.missing_globals.borrow().iter().cloned().collect()
    }

    // -- Native registration -------------------------------------------------

    fn register_natives(&mut self) {
        // The stub sentinel used for unknown globals.
        self.vm.stub_native = Some(
            self.vm
                .add_native("__stub", Box::new(|_vm, _args| Ok(vec![Value::Nil]))),
        );

        self.vm.register(
            "Exit",
            Box::new(|vm, _args| {
                if let Some(guest) = vm.guest.as_mut() {
                    guest.exit_requested = true;
                }
                Ok(vec![])
            }),
        );
        self.vm.register("_com", Box::new(com_native));
        self.vm.register(
            "TestCom1",
            Box::new(|_vm, _args| Ok(vec![vh::num_value(1.0)])),
        );
        self.vm.register(
            "GetRand",
            Box::new(|vm, args| {
                let limit = args.first().and_then(vh::as_num).unwrap_or(32767.0);
                let value = vm
                    .guest
                    .as_mut()
                    .map(|guest| guest.rand_next())
                    .unwrap_or(0);
                Ok(vec![vh::num_value((value % limit.max(1.0) as u32) as f32)])
            }),
        );
        self.vm.register(
            "mod",
            Box::new(|_vm, args| {
                let a = args.first().and_then(vh::as_num).unwrap_or(0.0);
                let b = args.get(1).and_then(vh::as_num).unwrap_or(1.0);
                if b == 0.0 {
                    Ok(vec![vh::num_value(0.0)])
                } else {
                    Ok(vec![vh::num_value(a % b)])
                }
            }),
        );
        self.vm.register(
            "GetSysInfo",
            Box::new(|vm, _args| {
                let (imei, imsi, vendor, device) = vm
                    .guest
                    .as_ref()
                    .map(|guest| guest.sysinfo_strings())
                    .unwrap_or_default();
                Ok(vec![vh::table_value(|table| {
                    table.set(vh::str_value(b"IMEI"), vh::str_value(&imei));
                    table.set(vh::str_value(b"IMSI"), vh::str_value(&imsi));
                    table.set(vh::str_value(b"manufactory"), vh::str_value(&vendor));
                    table.set(vh::str_value(b"type"), vh::str_value(&device));
                })])
            }),
        );
        self.vm.register(
            "GetDatetime",
            Box::new(|_vm, _args| {
                let now = crate::cfunction::local_datetime();
                Ok(vec![vh::table_value(|table| {
                    table.set(vh::str_value(b"year"), vh::num_value(now.0 as f32));
                    table.set(vh::str_value(b"mon"), vh::num_value(now.1 as f32));
                    table.set(vh::str_value(b"day"), vh::num_value(now.2 as f32));
                    table.set(vh::str_value(b"hour"), vh::num_value(now.3 as f32));
                    table.set(vh::str_value(b"minute"), vh::num_value(now.4 as f32));
                    table.set(vh::str_value(b"sec"), vh::num_value(now.5 as f32));
                })])
            }),
        );
        self.vm.register(
            "TimerStart",
            Box::new(|vm, args| {
                let id = args.first().and_then(vh::as_num).unwrap_or(0.0) as i32;
                let interval = args.get(1).and_then(vh::as_num).unwrap_or(100.0);
                if let Some(guest) = vm.guest.as_mut() {
                    guest.timer_start(id, interval as u32);
                }
                Ok(vec![])
            }),
        );
        self.vm.register(
            "TimerStop",
            Box::new(|vm, args| {
                let id = args.first().and_then(vh::as_num).unwrap_or(0.0) as i32;
                if let Some(guest) = vm.guest.as_mut() {
                    guest.timer_stop(id);
                }
                Ok(vec![])
            }),
        );

        // Drawing natives write into the guest screen buffer.
        self.vm.register(
            "ClearScreen",
            Box::new(|vm, args| {
                let (r, g, b) = color_args(args);
                if let Some(guest) = vm.guest.as_mut() {
                    guest.fill_screenbuf(crate::cfunction::rgb888_to_565(r, g, b));
                }
                let color = crate::cfunction::rgb888_to_565(r, g, b);
                vm.screen.fill(color);
                Ok(vec![])
            }),
        );
        self.vm.register(
            "DrawRect",
            Box::new(|vm, args| {
                let numbers: Vec<f32> = args.iter().filter_map(vh::as_num).collect();
                if let (Some(guest), true) = (vm.guest.as_mut(), numbers.len() >= 7) {
                    guest.draw_rect(
                        numbers[0] as u32,
                        numbers[1] as u32,
                        numbers[2] as u32,
                        numbers[3] as u32,
                        numbers[4] as u32,
                        numbers[5] as u32,
                        numbers[6] as u32,
                    );
                }
                Ok(vec![])
            }),
        );
        self.vm.register(
            "DrawPoint",
            Box::new(|vm, args| {
                let numbers: Vec<f32> = args.iter().filter_map(vh::as_num).collect();
                if let (Some(guest), true) = (vm.guest.as_mut(), numbers.len() >= 3) {
                    guest.draw_point(numbers[0] as u32, numbers[1] as u32, numbers[2] as u32);
                }
                Ok(vec![])
            }),
        );
        self.vm.register(
            "RefreshScreen",
            Box::new(|vm, _args| {
                present_screen(vm);
                Ok(vec![])
            }),
        );
        register_stub_group(
            &mut self.vm,
            &[
                "DrawText",
                "DrawTextEx",
                "DrawLine",
                "TextWidth",
                "EffSetCon",
                "DispUpEx",
                "DispUp",
                "SpriteSet",
                "SpriteDraw",
                "SpriteDrawEx",
                "SpriteCheck",
                "TileSet",
                "TileSetRect",
                "TileDraw",
                "GetTile",
                "SetTile",
                "TileShift",
                "TileLoad",
                "SaveTable",
                "LoadTable",
                "Call",
                "SendSms",
                "GetNetworkID",
                "ConnectWAP",
                "RunFile",
                "c2u",
                "BgMusicSet",
                "BgMusicStart",
                "BgMusicStop",
                "SoundSet",
                "SoundPlay",
                "SoundStop",
                "PlatDrawChar",
                "GetCharBitmap",
            ],
        );

        // Bitmap management: load from package, blit to screen.
        self.vm.register(
            "BitmapLoad",
            Box::new(|vm, args| {
                let id = args.first().and_then(vh::as_num).unwrap_or(0.0) as u32;
                let path = args.get(1).map(vh::to_string_bytes).unwrap_or_default();
                let name = String::from_utf8_lossy(&path).to_string();
                let width = args.get(3).and_then(vh::as_num).unwrap_or(0.0) as u32;
                let height = args.get(4).and_then(vh::as_num).unwrap_or(0.0) as u32;
                let Some(guest) = vm.guest.as_mut() else {
                    return Ok(vec![vh::num_value(-1.0)]);
                };
                match guest.file_lookup(&name) {
                    Some(data) => {
                        let pixel_count = (width * height) as usize;
                        let mut pixels = Vec::with_capacity(pixel_count);
                        for i in 0..pixel_count {
                            let offset = i * 2;
                            if offset + 2 <= data.len() {
                                pixels.push(u16::from_le_bytes([data[offset], data[offset + 1]]));
                            }
                        }
                        guest.bitmaps.insert(
                            id,
                            crate::cfunction::Bitmap {
                                width,
                                height,
                                pixels,
                            },
                        );
                        Ok(vec![vh::num_value(0.0)])
                    }
                    None => Ok(vec![vh::num_value(-1.0)]),
                }
            }),
        );
        self.vm.register(
            "BitmapShow",
            Box::new(|vm, args| {
                let id = args.first().and_then(vh::as_num).unwrap_or(0.0) as u32;
                let x = args.get(1).and_then(vh::as_num).unwrap_or(0.0) as u32;
                let y = args.get(2).and_then(vh::as_num).unwrap_or(0.0) as u32;
                if let Some(guest) = vm.guest.as_mut() {
                    if let Some(bitmap) = guest.bitmaps.get(&id) {
                        let pixels = bitmap.pixels.clone();
                        let width = bitmap.width;
                        guest.blit_bitmap(&pixels, width, x, y, None);
                    }
                }
                Ok(vec![])
            }),
        );
        self.vm.register(
            "BitmapNew",
            Box::new(|vm, args| {
                let id = args.first().and_then(vh::as_num).unwrap_or(0.0) as u32;
                let width = args.get(1).and_then(vh::as_num).unwrap_or(0.0) as u32;
                let height = args.get(2).and_then(vh::as_num).unwrap_or(0.0) as u32;
                if let Some(guest) = vm.guest.as_mut() {
                    guest.bitmaps.insert(
                        id,
                        crate::cfunction::Bitmap {
                            width,
                            height,
                            pixels: vec![0; (width * height) as usize],
                        },
                    );
                }
                Ok(vec![])
            }),
        );
        self.vm.register(
            "BitmapDraw",
            Box::new(|vm, args| {
                let src_id = args.first().and_then(vh::as_num).unwrap_or(0.0) as u32;
                let x = args.get(3).and_then(vh::as_num).unwrap_or(0.0) as u32;
                let y = args.get(4).and_then(vh::as_num).unwrap_or(0.0) as u32;
                if let Some(guest) = vm.guest.as_mut() {
                    if let Some(bitmap) = guest.bitmaps.get(&src_id) {
                        let pixels = bitmap.pixels.clone();
                        let width = bitmap.width;
                        guest.blit_bitmap(&pixels, width, x, y, None);
                    }
                }
                Ok(vec![])
            }),
        );
        self.vm.register(
            "BmGetScr",
            Box::new(|_vm, _args| Ok(vec![vh::num_value(crate::cfunction::SCREENBUF as f32)])),
        );

        // Standard-library subset.
        self.vm.register(
            "type",
            Box::new(|_vm, args| {
                let name = args
                    .first()
                    .map(|value| value.type_name().to_string())
                    .unwrap_or_else(|| "nil".to_string());
                Ok(vec![vh::str_value(name.as_bytes())])
            }),
        );
        self.vm.register(
            "tonumber",
            Box::new(|_vm, args| {
                Ok(vec![args
                    .first()
                    .and_then(vh::as_num)
                    .map(vh::num_value)
                    .unwrap_or(Value::Nil)])
            }),
        );
        self.vm.register(
            "tostring",
            Box::new(|_vm, args| {
                let text = args
                    .first()
                    .map(vh::to_string_bytes)
                    .unwrap_or_else(|| b"nil".to_vec());
                Ok(vec![vh::str_value(&text)])
            }),
        );
        self.vm.register(
            "error",
            Box::new(|_vm, args| {
                let text = args
                    .first()
                    .map(vh::to_string_bytes)
                    .unwrap_or_else(|| b"error".to_vec());
                Err(String::from_utf8_lossy(&text).to_string())
            }),
        );
        self.vm.register(
            "assert",
            Box::new(|_vm, args| {
                let truthy = !args.first().map(Value::is_false_y).unwrap_or(true);
                if truthy {
                    Ok(args.to_vec())
                } else {
                    Err("assertion failed".to_string())
                }
            }),
        );
        self.vm.register(
            "next",
            Box::new(|_vm, args| {
                // Table iteration for generic-for: returns the next key/value.
                let Some(Value::Table(table)) = args.first() else {
                    return Ok(vec![Value::Nil]);
                };
                let current = args.get(1).cloned().unwrap_or(Value::Nil);
                let table = table.borrow();
                let entries: Vec<Value> = table
                    .array
                    .iter()
                    .enumerate()
                    .flat_map(|(index, value)| [vh::num_value(index as f32 + 1.0), value.clone()])
                    .chain(
                        table
                            .snapshot_map()
                            .iter()
                            .flat_map(|(key, value)| [vh::key_to_value(key), value.clone()]),
                    )
                    .collect();
                let position = if matches!(current, Value::Nil) {
                    0
                } else {
                    entries
                        .iter()
                        .step_by(2)
                        .position(|key| vh::values_equal(key, &current))
                        .map(|pair| pair * 2 + 2)
                        .unwrap_or(entries.len())
                };
                match entries.get(position..position + 2) {
                    Some([key, value]) => Ok(vec![key.clone(), value.clone()]),
                    _ => Ok(vec![Value::Nil]),
                }
            }),
        );
        self.vm.register(
            "ipairs",
            Box::new(|_vm, _args| {
                Ok(vec![Value::Nil]) // replaced by the ipairs_iter pair below
            }),
        );
        self.vm.register_in(
            "string",
            "sub",
            Box::new(|_vm, args| {
                let Some(Value::Str(text)) = args.first() else {
                    return Ok(vec![Value::Nil]);
                };
                let len = text.len() as i32;
                let normalize = |value: i32| -> i32 {
                    if value < 0 {
                        len + value + 1
                    } else {
                        value
                    }
                };
                let start = args.get(1).and_then(vh::as_num).map_or(1, |n| n as i32);
                let end = args
                    .get(2)
                    .and_then(vh::as_num)
                    .map(|n| n as i32)
                    .unwrap_or(len);
                let start = normalize(start).clamp(1, len.max(1));
                let end = normalize(end).clamp(1, len.max(1));
                if start > end || len == 0 {
                    return Ok(vec![vh::str_value(b"")]);
                }
                Ok(vec![vh::str_value(
                    &text[(start - 1) as usize..end as usize],
                )])
            }),
        );
        self.vm.register_in(
            "string",
            "len",
            Box::new(|_vm, args| {
                let len = match args.first() {
                    Some(Value::Str(text)) => text.len() as f32,
                    _ => 0.0,
                };
                Ok(vec![vh::num_value(len)])
            }),
        );
        self.vm.register_in(
            "string",
            "format",
            Box::new(|_vm, args| {
                // Formatting over the MiniJ values; reuse the kernel by mapping
                // numbers/strings positionally after the format string.
                let Some(Value::Str(format)) = args.first() else {
                    return Ok(vec![vh::str_value(b"")]);
                };
                let mut out: Vec<u8> = Vec::new();
                let mut arg_index = 1usize;
                let bytes = format.as_ref();
                let mut index = 0usize;
                while index < bytes.len() {
                    if bytes[index] != b'%' {
                        out.push(bytes[index]);
                        index += 1;
                        continue;
                    }
                    // Find the conversion letter.
                    let mut cursor = index + 1;
                    while cursor < bytes.len()
                        && !matches!(
                            bytes[cursor],
                            b'd' | b'i' | b'u' | b'x' | b'X' | b's' | b'c' | b'%'
                        )
                    {
                        cursor += 1;
                    }
                    let Some(conversion) = bytes.get(cursor).copied() else {
                        out.push(b'%');
                        index += 1;
                        continue;
                    };
                    let spec_bytes = &bytes[index..=cursor];
                    let arg = args.get(arg_index).cloned().unwrap_or(Value::Nil);
                    arg_index += 1;
                    match conversion {
                        b'%' => out.push(b'%'),
                        b's' => out.extend_from_slice(&vh::to_string_bytes(&arg)),
                        _ => {
                            let number = vh::as_num(&arg).unwrap_or(0.0) as i32;
                            let rendered =
                                crate::fmt::format(spec_bytes, |slot, _conv| match slot {
                                    0 => crate::fmt::Arg::Num(number as u32),
                                    _ => crate::fmt::Arg::Skip,
                                });
                            out.extend_from_slice(&rendered);
                        }
                    }
                    index = cursor + 1;
                }
                Ok(vec![vh::str_value(&out)])
            }),
        );
        self.vm.register_in(
            "math",
            "floor",
            Box::new(|_vm, args| {
                Ok(vec![vh::num_value(
                    args.first().and_then(vh::as_num).unwrap_or(0.0).floor(),
                )])
            }),
        );
        self.vm.register_in(
            "math",
            "abs",
            Box::new(|_vm, args| {
                Ok(vec![vh::num_value(
                    args.first().and_then(vh::as_num).unwrap_or(0.0).abs(),
                )])
            }),
        );
        self.vm.register_in(
            "math",
            "random",
            Box::new(|_vm, args| {
                let value = {
                    let mut state = 0x1234_5678u32;
                    state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    (state >> 16) & 0x7FFF
                };
                match args.len() {
                    0 => Ok(vec![vh::num_value(value as f32 / 32768.0)]),
                    1 => Ok(vec![vh::num_value(
                        1.0 + (value % vh::as_num(&args[0]).unwrap_or(1.0) as u32) as f32,
                    )]),
                    _ => {
                        let low = vh::as_num(&args[0]).unwrap_or(0.0);
                        let high = vh::as_num(&args[1]).unwrap_or(1.0);
                        Ok(vec![vh::num_value(
                            low + (value as f32 % (high - low + 1.0)),
                        )])
                    }
                }
            }),
        );
        self.vm
            .register_in("math", "randomseed", Box::new(|_vm, _args| Ok(vec![])));

        // The `file` library: package-backed reads plus guest handles.
        self.vm.register_in(
            "file",
            "open",
            Box::new(|vm, args| {
                let path = args.first().map(vh::to_string_bytes).unwrap_or_default();
                let name = String::from_utf8_lossy(&path).to_string();
                let Some(guest) = vm.guest.as_mut() else {
                    return Ok(vec![vh::num_value(0.0)]);
                };
                match guest.file_open(&name) {
                    Some(handle) => Ok(vec![vh::num_value(handle as f32)]),
                    None => Ok(vec![vh::num_value(0.0)]),
                }
            }),
        );
        self.vm.register_in(
            "file",
            "read",
            Box::new(|vm, args| {
                let handle = args.first().and_then(vh::as_num).unwrap_or(0.0) as usize;
                let len = args.get(1).and_then(vh::as_num).unwrap_or(0.0) as usize;
                let Some(guest) = vm.guest.as_mut() else {
                    return Ok(vec![vh::str_value(b"")]);
                };
                match guest.file_read(handle, len) {
                    Some(bytes) => Ok(vec![vh::str_value(&bytes)]),
                    None => Ok(vec![Value::Nil]),
                }
            }),
        );
        self.vm.register_in(
            "file",
            "close",
            Box::new(|vm, args| {
                let handle = args.first().and_then(vh::as_num).unwrap_or(0.0) as usize;
                if let Some(guest) = vm.guest.as_mut() {
                    guest.file_close(handle);
                }
                Ok(vec![])
            }),
        );
        self.vm.register_in(
            "file",
            "getLen",
            Box::new(|vm, args| {
                let handle = args.first().and_then(vh::as_num).unwrap_or(0.0) as usize;
                let len = vm
                    .guest
                    .as_ref()
                    .and_then(|guest| guest.file_len(handle))
                    .unwrap_or(0);
                Ok(vec![vh::num_value(len as f32)])
            }),
        );
    }

    /// Presents the guest framebuffer into the bridge screen.
    pub fn present(&mut self) {
        if let Some(guest) = self.vm.guest.as_mut() {
            let width = guest.screen_width();
            let height = guest.screen_height();
            if self.vm.screen.width() != width || self.vm.screen.height() != height {
                self.vm.screen = crate::display::Screen::new(width, height);
            }
            guest.refresh_screen(&mut self.vm.screen);
        }
    }
}

/// `_com(input0, input1, [len], [code])`: the script → runtime bridge.
/// 800 loads an ext image (input1 = the ext bytes as a string), 801 calls
/// the registered handler with (code, input bytes).
fn com_native(vm: &mut Vm, args: &[Value]) -> Result<Vec<Value>, String> {
    let input0 = args.first().and_then(vh::as_num).unwrap_or(0.0) as i32;
    eprintln!("[_com] input0={input0:#x} nargs={}", args.len());
    match input0 {
        800 | 802 => {
            let code = args
                .get(3)
                .and_then(vh::as_num)
                .map(|value| value as i32)
                .unwrap_or(1);
            let Some(Value::Str(ext)) = args.get(1) else {
                return Ok(vec![vh::num_value(-1.0)]);
            };
            let Some(guest) = vm.guest.as_mut() else {
                return Ok(vec![vh::num_value(-1.0)]);
            };
            let mut screen = std::mem::replace(
                &mut vm.screen,
                crate::display::Screen::new(
                    crate::display::DEFAULT_WIDTH,
                    crate::display::DEFAULT_HEIGHT,
                ),
            );
            let result = guest.load_ext_at(ext, crate::cfunction::EXT_BASE, code, &mut screen);
            vm.screen = screen;
            match result {
                Ok(value) => Ok(vec![vh::num_value(value as f32)]),
                Err(error) => {
                    log::error!("_com(800) failed: {error}");
                    Ok(vec![vh::num_value(-1.0)])
                }
            }
        }
        801 => {
            let code = args
                .get(2)
                .and_then(vh::as_num)
                .map(|value| value as i32)
                .unwrap_or(0);
            let payload = match args.get(1) {
                Some(Value::Str(text)) => text.as_ref().clone(),
                Some(Value::Num(number)) => number.to_le_bytes().to_vec(),
                _ => Vec::new(),
            };
            let Some(guest) = vm.guest.as_mut() else {
                return Ok(vec![vh::num_value(-1.0)]);
            };
            let scratch = guest.scratch_buffer(&payload);
            let mut screen = std::mem::replace(
                &mut vm.screen,
                crate::display::Screen::new(
                    crate::display::DEFAULT_WIDTH,
                    crate::display::DEFAULT_HEIGHT,
                ),
            );
            let result = guest.call_c_function(
                code,
                scratch,
                payload.len() as i32,
                crate::cfunction::DEFAULT_INSTRUCTION_LIMIT,
                &mut screen,
            );
            vm.screen = screen;
            match result {
                Ok(value) => Ok(vec![vh::num_value(value as f32)]),
                Err(error) => {
                    log::error!("_com(801) failed: {error}");
                    Ok(vec![vh::num_value(-1.0)])
                }
            }
        }
        other => {
            log::warn!("_com got unknown input0 {other:#x}");
            Ok(vec![vh::num_value(1.0)])
        }
    }
}

fn color_args(args: &[Value]) -> (u32, u32, u32) {
    let numbers: Vec<f32> = args.iter().filter_map(vh::as_num).collect();
    (
        numbers.first().copied().unwrap_or(0.0) as u32,
        numbers.get(1).copied().unwrap_or(0.0) as u32,
        numbers.get(2).copied().unwrap_or(0.0) as u32,
    )
}

fn present_screen(vm: &mut Vm) {
    if let Some(guest) = vm.guest.as_mut() {
        let width = guest.screen_width();
        let height = guest.screen_height();
        if vm.screen.width() != width || vm.screen.height() != height {
            vm.screen = crate::display::Screen::new(width, height);
        }
        guest.refresh_screen(&mut vm.screen);
    }
}

fn register_stub_group(vm: &mut Vm, names: &[&'static str]) {
    for name in names {
        let key = *name;
        vm.register(
            key,
            Box::new(move |_vm, _args| {
                {
                    use std::collections::HashSet;
                    use std::sync::Mutex;
                    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
                    let mut guard = SEEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    if guard
                        .get_or_insert_with(HashSet::new)
                        .insert(key.to_string())
                    {
                        eprintln!("[stub] native `{key}` called");
                    }
                }
                Ok(vec![vh::num_value(0.0)])
            }),
        );
    }
}
