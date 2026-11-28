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
use crate::vm::{Native, Value, Vm};

/// Sentinel native slot backing unknown globals: scripts probing optional
/// API surface get a callable that returns nothing instead of crashing.
pub struct ScriptBridge {
    pub vm: Vm,
    missing_globals: RefCell<HashSet<String>>,
    /// File table captured by `load_package` before `create_guest` runs.
    pending_files: Option<Vec<(String, Vec<u8>)>>,
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
            pending_files: None,
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
        let mut files: Vec<(String, Vec<u8>)> = package
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.name.clone(),
                    package.read_entry(&entry.name).unwrap_or_default(),
                )
            })
            .collect();
        // The raw package under `pack_filename`: the SDK runtime's internal
        // resource loader opens the pack itself and extracts members
        // (sky_cfg.bin, res_lang0.rc, …), so the guest must be able to
        // `mr_open` it.
        files.push(("app.mrp".to_string(), package.raw_data().to_vec()));
        if let Some(guest) = self.vm.guest.as_mut() {
            guest.files = files;
        } else {
            // The guest is created after load (create_guest); remember the
            // file table so the new guest still sees the package members.
            self.pending_files = Some(files);
        }
        // The boot entry the runtime passes in (`_dsm` by default) plus the
        // empty start parameter — scripts branch on both.
        self.vm
            .globals
            .borrow_mut()
            .set(vh::str_value(b"_mr_entry"), vh::str_value(b"_dsm"));
        self.vm
            .globals
            .borrow_mut()
            .set(vh::str_value(b"_mr_param"), vh::str_value(b""));
        // Glyph metrics the shared-library scripts reference before the
        // game's own _textWidth calibration runs.
        self.vm
            .globals
            .borrow_mut()
            .set(vh::str_value(b"ChWordW"), vh::num_value(16.0));
        self.vm
            .globals
            .borrow_mut()
            .set(vh::str_value(b"ChWordH"), vh::num_value(16.0));
        self.vm
            .globals
            .borrow_mut()
            .set(vh::str_value(b"EngWordW"), vh::num_value(8.0));
        self.vm
            .globals
            .borrow_mut()
            .set(vh::str_value(b"EngWordH"), vh::num_value(16.0));
        // Platform-visible package identity: the header app id and pack
        // file name back the `o_APPID` / `contentMrp` globals.
        let app_id = package.header().app_id;
        let pack_name = package.header().internal_name.clone();
        self.vm.package = Some(package);
        self.vm
            .globals
            .borrow_mut()
            .set(vh::str_value(b"o_APPID"), vh::num_value(app_id as f32));
        self.vm.globals.borrow_mut().set(
            vh::str_value(b"contentMrp"),
            vh::str_value(pack_name.as_bytes()),
        );
    }

    /// Creates the ext guest with the platform configuration (the machine's
    /// resolution flows in so the SDK runtime sees the real screen size).
    pub fn create_guest_with(&mut self, config: crate::cfunction::GuestConfig) {
        if self.vm.guest.is_none() {
            self.vm.screen = crate::display::Screen::new(config.screen_width, config.screen_height);
            let files = self.pending_files.take().unwrap_or_default();
            self.vm.guest = Some(crate::cfunction::Guest::new(config, files));
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
        // A handler registered through `_com(2, fn)` takes priority over the
        // conventional `dealevent` global.
        let registered = self.vm.globals.borrow().get(&vh::str_value(b"__event_cb"));
        if let Value::Closure(closure) = registered {
            return self
                .vm
                .call_closure(
                    closure,
                    &[
                        vh::num_value(event_type as f32),
                        vh::num_value(p0 as f32),
                        vh::num_value(p1 as f32),
                    ],
                )
                .map(|_| ());
        }
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

    /// Fires a due script timer: `_com(3, fn)` registrations first, then the
    /// conventional `dealtimer` global.
    pub fn fire_script_timer(&mut self) -> Result<(), String> {
        let registered = self.vm.globals.borrow().get(&vh::str_value(b"__timer_cb"));
        if let Value::Closure(closure) = registered {
            return self.vm.call_closure(closure, &[]).map(|_| ());
        }
        let handler = self.vm.globals.borrow().get(&vh::str_value(b"dealtimer"));
        if matches!(handler, Value::Nil) {
            return Ok(());
        }
        self.vm.call_global("dealtimer", &[]).map(|_| ())
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
                .add_native("__stub", Rc::new(|_vm, _args| Ok(vec![Value::Nil]))),
        );

        self.vm.register(
            "Exit",
            Rc::new(|vm, _args| {
                if let Some(guest) = vm.guest.as_mut() {
                    guest.exit_requested = true;
                }
                Ok(vec![])
            }),
        );
        self.vm.register("_com", Rc::new(com_native));
        // `TestCom` is the same dispatcher as `_com` (the SDK exposes both
        // names); `_strCom` is the string-input variant used to read package
        // files and to drive the ext loader.
        self.vm.register("TestCom", Rc::new(com_native));
        self.vm.register("_strCom", Rc::new(str_com_native));
        self.vm.register(
            "TestCom1",
            Rc::new(|_vm, _args| Ok(vec![vh::num_value(1.0)])),
        );
        self.vm.register(
            "GetRand",
            Rc::new(|vm, args| {
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
            Rc::new(|_vm, args| {
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
            Rc::new(|vm, _args| Ok(vec![sysinfo_value(vm)])),
        );
        self.vm.register(
            "GetDatetime",
            Rc::new(|_vm, _args| Ok(vec![datetime_value()])),
        );
        self.vm.register(
            "TimerStart",
            Rc::new(|vm, args| {
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
            Rc::new(|vm, args| {
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
            Rc::new(|vm, args| {
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
            Rc::new(|vm, args| {
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
            Rc::new(|vm, args| {
                let numbers: Vec<f32> = args.iter().filter_map(vh::as_num).collect();
                if let (Some(guest), true) = (vm.guest.as_mut(), numbers.len() >= 3) {
                    guest.draw_point(numbers[0] as u32, numbers[1] as u32, numbers[2] as u32);
                }
                Ok(vec![])
            }),
        );
        self.vm.register(
            "RefreshScreen",
            Rc::new(|vm, _args| {
                present_screen(vm);
                Ok(vec![])
            }),
        );
        // Script-side drawing primitives routed through the guest engine.
        self.vm.register(
            "DrawText",
            Rc::new(|vm, args| {
                let text = args.first().map(vh::to_string_bytes).unwrap_or_default();
                let x = args.get(1).and_then(vh::as_num).unwrap_or(0.0) as i32;
                let y = args.get(2).and_then(vh::as_num).unwrap_or(0.0) as i32;
                let (r, g, b) = color_args(&args[3.min(args.len())..]);
                let color = crate::cfunction::rgb888_to_565(r, g, b);
                if let Some(guest) = vm.guest.as_mut() {
                    guest.draw_text(&text, x, y, color);
                }
                present_screen(vm);
                Ok(vec![])
            }),
        );
        // `TextWidth` returns (width, height) — the platform convention the
        // SDK scripts rely on when seeding glyph metrics.
        self.vm.register(
            "TextWidth",
            Rc::new(|_vm, args| {
                let text = args.first().map(vh::to_string_bytes).unwrap_or_default();
                let (decoded, _, _) = encoding_rs::GBK.decode(&text);
                let mut width = 0i32;
                for ch in decoded.chars() {
                    width += unifont::get_glyph(ch)
                        .map(|glyph| glyph.get_width() as i32)
                        .unwrap_or(8);
                }
                Ok(vec![vh::num_value(width as f32), vh::num_value(16.0)])
            }),
        );
        self.vm.register(
            "DrawLine",
            Rc::new(|vm, args| {
                let nums: Vec<i32> = args
                    .iter()
                    .filter_map(vh::as_num)
                    .map(|v| v as i32)
                    .collect();
                let (r, g, b) = color_args(&args[4.min(args.len())..]);
                let color = crate::cfunction::rgb888_to_565(r, g, b);
                if nums.len() >= 4 {
                    if let Some(guest) = vm.guest.as_mut() {
                        guest.draw_line(nums[0], nums[1], nums[2], nums[3], color);
                    }
                }
                present_screen(vm);
                Ok(vec![])
            }),
        );
        self.vm.register(
            "DrawPoint",
            Rc::new(|vm, args| {
                let nums: Vec<i32> = args
                    .iter()
                    .filter_map(vh::as_num)
                    .map(|v| v as i32)
                    .collect();
                let (r, g, b) = color_args(&args[2.min(args.len())..]);
                let color = crate::cfunction::rgb888_to_565(r, g, b);
                if nums.len() >= 2 {
                    if let Some(guest) = vm.guest.as_mut() {
                        guest.draw_line(nums[0], nums[1], nums[0], nums[1], color);
                    }
                }
                present_screen(vm);
                Ok(vec![])
            }),
        );
        for name in ["DispUpEx", "DispUp"] {
            self.vm.register(
                name,
                Rc::new(|vm, _args| {
                    present_screen(vm);
                    Ok(vec![])
                }),
            );
        }
        // Audio: no output in the core, but success lets games skip
        // audio init and proceed to the game loop.
        for name in [
            "BgMusicSet",
            "BgMusicStart",
            "BgMusicStop",
            "SoundSet",
            "SoundPlay",
            "SoundStop",
        ] {
            self.vm
                .register(name, Rc::new(|_vm, _args| Ok(vec![vh::num_value(0.0)])));
        }
        register_stub_group(
            &mut self.vm,
            &[
                "DrawTextEx",
                "EffSetCon",
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
                "PlatDrawChar",
                "GetCharBitmap",
            ],
        );

        // Bitmap management: load from package, blit to screen.
        self.vm.register(
            "BitmapLoad",
            Rc::new(|vm, args| {
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
            Rc::new(|vm, args| {
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
            Rc::new(|vm, args| {
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
            Rc::new(|vm, args| {
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
            Rc::new(|_vm, _args| Ok(vec![vh::num_value(crate::cfunction::SCREENBUF as f32)])),
        );

        // Standard-library subset.
        self.vm.register(
            "type",
            Rc::new(|_vm, args| {
                let name = args
                    .first()
                    .map(|value| value.type_name().to_string())
                    .unwrap_or_else(|| "nil".to_string());
                Ok(vec![vh::str_value(name.as_bytes())])
            }),
        );
        self.vm.register(
            "tonumber",
            Rc::new(|_vm, args| {
                Ok(vec![args
                    .first()
                    .and_then(vh::as_num)
                    .map(vh::num_value)
                    .unwrap_or(Value::Nil)])
            }),
        );
        self.vm.register(
            "tostring",
            Rc::new(|_vm, args| {
                let text = args
                    .first()
                    .map(vh::to_string_bytes)
                    .unwrap_or_else(|| b"nil".to_vec());
                Ok(vec![vh::str_value(&text)])
            }),
        );
        self.vm.register(
            "error",
            Rc::new(|_vm, args| {
                let text = args
                    .first()
                    .map(vh::to_string_bytes)
                    .unwrap_or_else(|| b"error".to_vec());
                Err(String::from_utf8_lossy(&text).to_string())
            }),
        );
        self.vm.register(
            "assert",
            Rc::new(|_vm, args| {
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
            Rc::new(|_vm, args| {
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
            Rc::new(|_vm, _args| {
                Ok(vec![Value::Nil]) // replaced by the ipairs_iter pair below
            }),
        );
        // `pcall`/`xpcall`: call a function value with error capture. The
        // native re-enters the interpreter for closures.
        self.vm.register(
            "pcall",
            Rc::new(|vm, args| {
                let outcome = match args.first() {
                    Some(Value::Closure(closure)) => vm.call_closure(closure.clone(), &args[1..]),
                    Some(Value::Native(index)) => vm.call_native(*index, &args[1..]),
                    Some(other) => Err(format!("attempt to call a {} value", other.type_name())),
                    None => Err("bad argument #1 to 'pcall'".to_string()),
                };
                match outcome {
                    Ok(mut results) => {
                        let mut out = vec![vh::num_value(1.0)];
                        out.append(&mut results);
                        Ok(out)
                    }
                    Err(error) => Ok(vec![vh::num_value(0.0), vh::str_value(error.as_bytes())]),
                }
            }),
        );
        self.vm.register(
            "xpcall",
            Rc::new(|vm, args| {
                let handler = args.get(1).cloned();
                let outcome = match args.first() {
                    Some(Value::Closure(closure)) => vm.call_closure(closure.clone(), &args[2..]),
                    Some(Value::Native(index)) => vm.call_native(*index, &args[2..]),
                    Some(other) => Err(format!("attempt to call a {} value", other.type_name())),
                    None => Err("bad argument #1 to 'xpcall'".to_string()),
                };
                match outcome {
                    Ok(mut results) => {
                        let mut out = vec![vh::num_value(1.0)];
                        out.append(&mut results);
                        Ok(out)
                    }
                    Err(error) => {
                        let message = vh::str_value(error.as_bytes());
                        let mut handled = match handler {
                            Some(Value::Closure(closure)) => {
                                vm.call_closure(closure, &[message]).unwrap_or_default()
                            }
                            Some(Value::Native(index)) => {
                                vm.call_native(index, &[message]).unwrap_or_default()
                            }
                            _ => Vec::new(),
                        };
                        let mut out = vec![vh::num_value(0.0)];
                        out.append(&mut handled);
                        Ok(out)
                    }
                }
            }),
        );
        self.vm.register("rawequal", Rc::new(raw_equal_native));
        self.vm.register("rawget", Rc::new(raw_get_native));
        self.vm.register("unpack", Rc::new(unpack_native));
        // Chunk loaders: packaged `.mr` members are already-compiled MiniJ
        // chunks, so `loadfile`/`dofile`/`loadstring` parse them with the
        // same deserializer the boot script used — no source compiler.
        self.vm.register(
            "loadfile",
            Rc::new(|vm, args| {
                let name = String::from_utf8_lossy(
                    &args.first().map(vh::to_string_bytes).unwrap_or_default(),
                )
                .to_string();
                match load_package_chunk(vm, &name) {
                    Ok(closure) => Ok(vec![closure]),
                    Err(error) => Ok(vec![Value::Nil, vh::str_value(error.as_bytes())]),
                }
            }),
        );
        self.vm.register(
            "dofile",
            Rc::new(|vm, args| {
                let name = String::from_utf8_lossy(
                    &args.first().map(vh::to_string_bytes).unwrap_or_default(),
                )
                .to_string();
                let closure = match load_package_chunk(vm, &name) {
                    Ok(Value::Closure(closure)) => closure,
                    Ok(Value::Nil) => return Ok(vec![Value::Nil]),
                    Ok(other) => {
                        return Err(format!("attempt to call a {} value", other.type_name()))
                    }
                    Err(error) => {
                        log::warn!("dofile({name:?}) failed: {error}");
                        return Ok(vec![Value::Nil]);
                    }
                };
                vm.call_closure(closure, &[])
            }),
        );
        self.vm.register(
            "loadstring",
            Rc::new(|_vm, args| {
                let data = args.first().map(vh::to_string_bytes).unwrap_or_default();
                match crate::undump::Chunk::parse(&data) {
                    Ok(chunk) => Ok(vec![Value::Closure(Rc::new(crate::vm::LuaClosure {
                        proto: Rc::new(chunk.proto),
                        upvals: Vec::new(),
                    }))]),
                    Err(error) => Ok(vec![
                        Value::Nil,
                        vh::str_value(error.to_string().as_bytes()),
                    ]),
                }
            }),
        );
        // Remaining compiler/runtime entry points that no reference sample
        // has needed yet; log-once stubs keep scripts alive.
        for (name, note) in [
            ("_runFile", "run packaged mrp"),
            ("RunFile", "run packaged mrp"),
            ("_loadPack", "switch package"),
            ("LoadPack", "switch package"),
            ("SaveTable", "persist table"),
            ("LoadTable", "restore table"),
            ("_getTab", "metatable support"),
            ("_setTab", "metatable support"),
        ] {
            self.vm.register(
                name,
                Rc::new(move |_vm, _args| {
                    log::warn!("native `{name}` is not implemented yet ({note})");
                    Ok(vec![Value::Nil])
                }),
            );
        }
        // The platform's `_lowerCamel` aliases over the standard library.
        self.vm.alias("error", "_error");
        self.vm.alias("assert", "_assert");
        self.vm.alias("next", "_next");
        self.vm.alias("ipairs", "_iPairs");
        self.vm.alias("tonumber", "_num");
        self.vm.alias("tostring", "_str");
        self.vm.alias("pcall", "_pCall");
        self.vm.alias("xpcall", "_pCallEx");
        self.vm.alias("rawequal", "_rawEq");
        self.vm.alias("loadfile", "_loadFile");
        self.vm.alias("dofile", "_execFile");
        self.vm.alias("loadstring", "_loads");
        self.vm.alias("loadstring", "_loadBuf");
        // Drawing/API aliases: scripts reach the same natives through the
        // `_lowerCamel` names the SDK headers document.
        self.vm.alias("DrawText", "_drawText");
        self.vm.alias("DrawRect", "_drawRect");
        self.vm.alias("DrawLine", "_drawLine");
        self.vm.alias("DrawPoint", "_drawPoint");
        self.vm.alias("ClearScreen", "_clearScr");
        self.vm.alias("TextWidth", "_textWidth");
        self.vm.alias("BitmapLoad", "_bmpLoad");
        self.vm.alias("BitmapShow", "_bmpShow");
        self.vm.alias("BitmapNew", "_bmpNew");
        self.vm.alias("BitmapDraw", "_bmpDraw");
        self.vm.alias("DispUpEx", "_dispUpEx");
        self.vm.alias("RefreshScreen", "_refreshScr");
        // Standard-library namespaces the scripts index directly. The
        // `table` surface follows the platform's tablib (getArrSize/setn
        // aliases, forEach pairs, rawGet/rawSet, items = unpack).
        self.vm
            .register_in("table", "insert", Rc::new(table_insert_native));
        self.vm
            .register_in("table", "remove", Rc::new(table_remove_native));
        let getn = |_vm: &mut Vm, args: &[Value]| -> Result<Vec<Value>, String> {
            let len = match args.first() {
                Some(Value::Table(table)) => table.borrow().array.len(),
                _ => 0,
            };
            Ok(vec![vh::num_value(len as f32)])
        };
        self.vm.register_in("table", "getn", Rc::new(getn));
        self.vm.register_in("table", "getArrSize", Rc::new(getn));
        self.vm.register_in(
            "table",
            "setArrSize",
            Rc::new(|_vm, args| {
                let Some(Value::Table(table)) = args.first() else {
                    return Err("bad argument #1 to 'setArrSize' (table expected)".to_string());
                };
                let size = args.get(1).and_then(vh::as_num).unwrap_or(0.0).max(0.0) as usize;
                table.borrow_mut().array.resize(size, Value::Nil);
                Ok(vec![])
            }),
        );
        self.vm.register_in(
            "table",
            "concat",
            Rc::new(|_vm, args| {
                let Some(Value::Table(table)) = args.first() else {
                    return Err("bad argument #1 to 'concat' (table expected)".to_string());
                };
                let separator = args.get(1).map(vh::to_string_bytes).unwrap_or_default();
                let mut out = Vec::new();
                for (index, value) in table.borrow().array.iter().enumerate() {
                    if index > 0 {
                        out.extend_from_slice(&separator);
                    }
                    out.extend_from_slice(&vh::to_string_bytes(value));
                }
                Ok(vec![vh::str_value(&out)])
            }),
        );
        self.vm
            .register_in("table", "items", Rc::new(unpack_native));
        self.vm
            .register_in("table", "rawGet", Rc::new(raw_get_native));
        self.vm.register_in(
            "table",
            "rawSet",
            Rc::new(|_vm, args| {
                let Some(Value::Table(table)) = args.first() else {
                    return Err("bad argument #1 to 'rawSet' (table expected)".to_string());
                };
                let key = args.get(1).cloned().unwrap_or(Value::Nil);
                let value = args.get(2).cloned().unwrap_or(Value::Nil);
                table.borrow_mut().set(key, value);
                Ok(vec![])
            }),
        );
        self.vm.register_in(
            "table",
            "next",
            Rc::new(|vm, args| (next_native_ref())(vm, args)),
        );
        self.vm.register_in(
            "table",
            "iPairs",
            Rc::new(|vm, args| {
                let Some(Value::Table(table)) = args.first() else {
                    return Err("bad argument #1 to 'iPairs' (table expected)".to_string());
                };
                let iter = vm
                    .native_index(IPAIRS_ITER_NAME)
                    .ok_or("ipairs_iter missing")?;
                Ok(vec![
                    Value::Native(iter),
                    Value::Table(table.clone()),
                    vh::num_value(0.0),
                ])
            }),
        );
        self.vm.register_in(
            "table",
            "pairs",
            Rc::new(|vm, args| {
                let Some(Value::Table(table)) = args.first() else {
                    return Err("bad argument #1 to 'pairs' (table expected)".to_string());
                };
                let iter = vm.native_index("next").ok_or("next missing")?;
                Ok(vec![
                    Value::Native(iter),
                    Value::Table(table.clone()),
                    Value::Nil,
                ])
            }),
        );
        self.vm.register(
            IPAIRS_ITER_NAME,
            Rc::new(|_vm, args| {
                let Some(Value::Table(table)) = args.first() else {
                    return Ok(vec![Value::Nil]);
                };
                let index = args.get(1).and_then(vh::as_num).unwrap_or(0.0) as usize;
                let table = table.borrow();
                match table.array.get(index) {
                    Some(value) => Ok(vec![vh::num_value(index as f32 + 1.0), value.clone()]),
                    None => Ok(vec![Value::Nil]),
                }
            }),
        );
        self.vm.register_in(
            "table",
            "sort",
            Rc::new(|vm, args| {
                let Some(Value::Table(table)) = args.first() else {
                    return Err("bad argument #1 to 'sort' (table expected)".to_string());
                };
                let comparator = args.get(1).cloned();
                let mut items = table.borrow().array.clone();
                // Insertion sort; ordering through the comparator value when
                // given, otherwise numeric/string natural order.
                for sorted in 1..items.len() {
                    let mut cursor = sorted;
                    while cursor > 0 {
                        let (before, after) = (&items[cursor - 1], &items[cursor]);
                        let less = match &comparator {
                            Some(Value::Closure(closure)) => vm
                                .call_closure(closure.clone(), &[after.clone(), before.clone()])?
                                .first()
                                .map(|value| !value.is_false_y())
                                .unwrap_or(false),
                            Some(Value::Native(index)) => vm
                                .call_native(*index, &[after.clone(), before.clone()])?
                                .first()
                                .map(|value| !value.is_false_y())
                                .unwrap_or(false),
                            _ => match (before.as_num(), after.as_num()) {
                                (Some(before), Some(after)) => before > after,
                                _ => {
                                    let (before, after) =
                                        (vh::to_string_bytes(before), vh::to_string_bytes(after));
                                    before > after
                                }
                            },
                        };
                        if !less {
                            break;
                        }
                        items.swap(cursor - 1, cursor);
                        cursor -= 1;
                    }
                }
                table.borrow_mut().array = items;
                Ok(vec![])
            }),
        );
        let foreach = |indexed: bool| -> Native {
            Rc::new(
                move |vm: &mut Vm, args: &[Value]| -> Result<Vec<Value>, String> {
                    let Some(Value::Table(table)) = args.first() else {
                        return Err("bad argument #1 (table expected)".to_string());
                    };
                    let callback = args.get(1).cloned().unwrap_or(Value::Nil);
                    let entries: Vec<(Value, Value)> = {
                        let table = table.borrow();
                        if indexed {
                            table
                                .array
                                .iter()
                                .enumerate()
                                .map(|(index, value)| {
                                    (vh::num_value(index as f32 + 1.0), value.clone())
                                })
                                .collect()
                        } else {
                            table
                                .snapshot_map()
                                .iter()
                                .map(|(key, value)| (vh::key_to_value(key), value.clone()))
                                .collect()
                        }
                    };
                    for (key, value) in entries {
                        match &callback {
                            Value::Closure(closure) => {
                                vm.call_closure(closure.clone(), &[key, value])?;
                            }
                            Value::Native(index) => {
                                vm.call_native(*index, &[key, value])?;
                            }
                            _ => return Err("attempt to call a non-function".to_string()),
                        }
                    }
                    Ok(vec![])
                },
            )
        };
        self.vm.register_in("table", "forEach", foreach(false));
        self.vm.register_in("table", "foreach", foreach(false));
        self.vm.register_in("table", "iforEach", foreach(true));
        self.vm.register_in("table", "foreachi", foreach(true));
        for name in ["save", "load"] {
            self.vm.register_in(
                "table",
                name,
                Rc::new(move |_vm, _args| {
                    log::warn!("table.{name} persistence is not implemented yet");
                    Ok(vec![Value::Nil])
                }),
            );
        }
        // Platform extension entry points: no operator services in this
        // emulator. `_platEx(code, input)` returns (output, ret); code 1204
        // is the drive-change request the runtime echoes back.
        self.vm.register(
            "_platEx",
            Rc::new(|_vm, args| {
                let code = args.first().and_then(vh::as_num).unwrap_or(0.0) as i32;
                let input = args.get(1).map(vh::to_string_bytes).unwrap_or_default();
                if code == 1204 {
                    return Ok(vec![vh::str_value(&input), vh::num_value(0.0)]);
                }
                match code {
                    // MD5 digest (16 raw bytes).
                    500 => Ok(vec![
                        vh::str_value(&crate::md5::md5(&input)),
                        vh::num_value(0.0),
                    ]),
                    // The mythroad base64 variant: encode / decode.
                    501 => match mr_encode(&input) {
                        Some(encoded) => Ok(vec![
                            vh::str_value(&encoded),
                            vh::num_value(encoded.len() as f32),
                        ]),
                        None => Ok(vec![vh::str_value(b""), vh::num_value(-1.0)]),
                    },
                    502 => match mr_decode(&input) {
                        Some(decoded) => Ok(vec![
                            vh::str_value(&decoded),
                            vh::num_value(decoded.len() as f32),
                        ]),
                        None => Ok(vec![vh::str_value(b""), vh::num_value(-1.0)]),
                    },
                    _ => Ok(vec![vh::str_value(b""), vh::num_value(-1.0)]),
                }
            }),
        );
        for name in ["_plat", "_closeNet"] {
            self.vm
                .register(name, Rc::new(|_vm, _args| Ok(vec![vh::num_value(-1.0)])));
        }
        // `print`: Lua's debug output, routed to the log.
        self.vm.register(
            "print",
            Rc::new(|_vm, args| {
                let mut line: Vec<u8> = Vec::new();
                for (index, piece) in args.iter().map(vh::to_string_bytes).enumerate() {
                    if index > 0 {
                        line.push(b'\t');
                    }
                    line.extend_from_slice(&piece);
                }
                log::info!("print: {}", String::from_utf8_lossy(&line));
                Ok(vec![])
            }),
        );
        self.vm
            .register("_gc", Rc::new(|_vm, _args| Ok(vec![vh::num_value(0.0)])));
        // The `sys` namespace: platform services the scripts reach through
        // `sys.getInfo()`, `sys.datetime()`, and file management.
        self.vm.register_in(
            "sys",
            "getInfo",
            Rc::new(|vm, _args| Ok(vec![sysinfo_value(vm)])),
        );
        self.vm.register_in(
            "sys",
            "getuptime",
            Rc::new(|vm, _args| {
                Ok(vec![vh::num_value(
                    vm.guest
                        .as_ref()
                        .map(|guest| guest.uptime_ms())
                        .unwrap_or(0) as f32,
                )])
            }),
        );
        self.vm.register_in(
            "sys",
            "datetime",
            Rc::new(|_vm, _args| Ok(vec![datetime_value()])),
        );
        self.vm.register_in(
            "sys",
            "getFileLen",
            Rc::new(|vm, args| {
                let handle = args.first().and_then(vh::as_num).unwrap_or(0.0) as usize;
                let len = vm
                    .guest
                    .as_ref()
                    .and_then(|guest| guest.file_len(handle))
                    .unwrap_or(0);
                Ok(vec![vh::num_value(len as f32)])
            }),
        );
        for op in ["rm", "rename", "mkDir", "mkdir", "rmDir", "rmdir"] {
            self.vm.register_in(
                "sys",
                op,
                Rc::new(move |_vm, _args| {
                    log::warn!("sys.{op} is not implemented yet; reporting success");
                    Ok(vec![vh::num_value(0.0)])
                }),
            );
        }
        for op in [
            "findStart",
            "findstart",
            "findNext",
            "findnext",
            "findStop",
            "findstop",
        ] {
            self.vm.register_in(
                "sys",
                op,
                Rc::new(move |_vm, _args| {
                    log::warn!("sys.{op} is not implemented yet; returning nothing");
                    Ok(vec![Value::Nil])
                }),
            );
        }
        // `sys.getFileInfo(name)` mirrors `mr_info`: the file size in bytes
        // or MR_FAILED for missing files.
        self.vm.register_in(
            "sys",
            "getFileInfo",
            Rc::new(|vm, args| {
                let name = String::from_utf8_lossy(
                    &args.first().map(vh::to_string_bytes).unwrap_or_default(),
                )
                .to_string();
                let size = vm
                    .package
                    .as_ref()
                    .and_then(|package| package.read_entry(&name).ok())
                    .map(|data| data.len() as i32)
                    .unwrap_or(-1);
                Ok(vec![vh::num_value(size as f32)])
            }),
        );
        self.vm.register_in(
            "string",
            "sub",
            Rc::new(|_vm, args| {
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
            Rc::new(|_vm, args| {
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
            Rc::new(|_vm, args| {
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
        // C/wide-string helpers over raw byte strings (the platform's
        // pointer-style string handling).
        let c_string_len = |bytes: &[u8]| {
            bytes
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(bytes.len())
        };
        let wide_string_len = |bytes: &[u8]| {
            let mut length = 0usize;
            while length + 2 <= bytes.len() {
                if bytes[length] == 0 && bytes[length + 1] == 0 {
                    return length;
                }
                length += 2;
            }
            bytes.len()
        };
        self.vm.register_in(
            "string",
            "cstr",
            Rc::new(move |_vm, args| {
                let bytes = args.first().map(vh::to_string_bytes).unwrap_or_default();
                Ok(vec![vh::str_value(&bytes[..c_string_len(&bytes)])])
            }),
        );
        self.vm.register_in(
            "string",
            "wstr",
            Rc::new(move |_vm, args| {
                let bytes = args.first().map(vh::to_string_bytes).unwrap_or_default();
                Ok(vec![vh::str_value(&bytes[..wide_string_len(&bytes)])])
            }),
        );
        self.vm.register_in(
            "string",
            "clen",
            Rc::new(move |_vm, args| {
                let bytes = args.first().map(vh::to_string_bytes).unwrap_or_default();
                Ok(vec![vh::num_value(c_string_len(&bytes) as f32)])
            }),
        );
        self.vm.register_in(
            "string",
            "wlen",
            Rc::new(move |_vm, args| {
                let bytes = args.first().map(vh::to_string_bytes).unwrap_or_default();
                Ok(vec![vh::num_value(wide_string_len(&bytes) as f32)])
            }),
        );
        // GBK ↔ UTF-16LE conversion (the platform's c2u/u2c pair).
        self.vm.register_in(
            "string",
            "c2u",
            Rc::new(|_vm, args| {
                let bytes = args.first().map(vh::to_string_bytes).unwrap_or_default();
                let (decoded, _, _) = encoding_rs::GBK.decode(&bytes);
                let mut units = Vec::with_capacity(decoded.len() * 2);
                for unit in decoded.encode_utf16() {
                    units.extend_from_slice(&unit.to_le_bytes());
                }
                Ok(vec![vh::str_value(&units)])
            }),
        );
        self.vm.register_in(
            "string",
            "u2c",
            Rc::new(|_vm, args| {
                let bytes = args.first().map(vh::to_string_bytes).unwrap_or_default();
                let mut units: Vec<u16> = Vec::with_capacity(bytes.len() / 2);
                for pair in bytes.windows(2).step_by(2) {
                    let unit = u16::from_le_bytes([pair[0], pair[1]]);
                    if unit == 0 {
                        break;
                    }
                    units.push(unit);
                }
                let decoded = String::from_utf16_lossy(&units);
                let (encoded, _, _) = encoding_rs::GBK.encode(&decoded);
                Ok(vec![vh::str_value(&encoded)])
            }),
        );
        // `string.pack`/`unpack`: the platform's fixed-width binary format
        // (i/I = 4 bytes, h/H = 2, b/B = 1, little-endian) that the SDK
        // loader scripts use to marshal ext headers and struct data.
        let pack_fmt = |format: &[u8], args: &[Value]| -> Result<Vec<u8>, String> {
            let mut out = Vec::new();
            for (offset, &conversion) in (1usize..).zip(format.iter()) {
                let arg = args.get(offset).cloned().unwrap_or(Value::Nil);
                let number = vh::as_num(&arg).unwrap_or(0.0) as i64;
                match conversion {
                    b'i' | b'I' => out.extend_from_slice(&(number as u32).to_le_bytes()),
                    b'h' | b'H' => out.extend_from_slice(&(number as u16).to_le_bytes()),
                    b'b' | b'B' => out.push(number as u8),
                    other => return Err(format!("string.pack: unsupported format `{other}`")),
                }
            }
            Ok(out)
        };
        let unpack_fmt = |format: &[u8],
                          data: &[u8],
                          init: usize|
         -> Result<(Vec<Value>, usize), String> {
            let mut values = Vec::new();
            let mut cursor = init;
            for &conversion in format {
                let need = match conversion {
                    b'i' | b'I' => 4,
                    b'h' | b'H' => 2,
                    b'b' | b'B' => 1,
                    other => return Err(format!("string.unpack: unsupported format `{other}`")),
                };
                if cursor + need > data.len() {
                    return Err("string.unpack: data too short".to_string());
                }
                let slice = &data[cursor..cursor + need];
                let mut bytes = [0u8; 8];
                bytes[..need].copy_from_slice(slice);
                let value = match conversion {
                    b'i' => i32::from_le_bytes(bytes[..4].try_into().unwrap()) as f32,
                    b'I' => u32::from_le_bytes(bytes[..4].try_into().unwrap()) as f32,
                    b'h' => i16::from_le_bytes(bytes[..2].try_into().unwrap()) as f32,
                    b'H' => u16::from_le_bytes(bytes[..2].try_into().unwrap()) as f32,
                    _ => bytes[0] as i8 as f32,
                };
                values.push(vh::num_value(value));
                cursor += need;
            }
            Ok((values, cursor + 1))
        };
        self.vm.register_in(
            "string",
            "pack",
            Rc::new(move |_vm, args| {
                let format = args.first().map(vh::to_string_bytes).unwrap_or_default();
                Ok(vec![vh::str_value(&pack_fmt(&format, args)?)])
            }),
        );
        self.vm.register_in(
            "string",
            "unpack",
            Rc::new(move |_vm, args| {
                let format = args.first().map(vh::to_string_bytes).unwrap_or_default();
                let data = args.get(1).map(vh::to_string_bytes).unwrap_or_default();
                let init = args
                    .get(2)
                    .and_then(vh::as_num)
                    .map(|value| value.max(1.0) as usize - 1)
                    .unwrap_or(0);
                let (values, next) = unpack_fmt(&format, &data, init)?;
                let mut out = values;
                out.push(vh::num_value(next as f32));
                Ok(out)
            }),
        );
        self.vm.register_in(
            "string",
            "packLen",
            Rc::new(move |_vm, args| {
                let format = args.first().map(vh::to_string_bytes).unwrap_or_default();
                Ok(vec![vh::num_value(pack_fmt(&format, args)?.len() as f32)])
            }),
        );
        // The SDK's raw-buffer string FFI. `subV` escapes a string as
        // `(pointer, length)`; `new`/`set`/`update`/`pupdate` patch byte
        // buffers held in guest memory. Buffer-backed strings surface as
        // `{pointer, size}` tables — the same shape `_strCom` already reads
        // back as payloads.
        self.vm.register_in(
            "string",
            "subV",
            Rc::new(|vm, args| {
                let bytes = args.first().map(vh::to_string_bytes).unwrap_or_default();
                let length = bytes.len() as f32;
                let Some(guest) = vm.guest.as_mut() else {
                    return Ok(vec![vh::num_value(0.0), vh::num_value(length)]);
                };
                let block = guest.heap_malloc(bytes.len() as u32 + 1);
                if block == 0 {
                    return Ok(vec![vh::num_value(0.0), vh::num_value(length)]);
                }
                let base = block as usize;
                guest.mem[base..base + bytes.len()].copy_from_slice(&bytes);
                guest.mem[base + bytes.len()] = 0;
                Ok(vec![vh::num_value(block as f32), vh::num_value(length)])
            }),
        );
        self.vm.register_in(
            "string",
            "new",
            Rc::new(|vm, args| {
                let len = args.first().and_then(vh::as_num).unwrap_or(0.0).max(0.0) as u32;
                let Some(guest) = vm.guest.as_mut() else {
                    return Ok(vec![Value::Nil]);
                };
                let block = guest.heap_malloc(len.max(1));
                if block == 0 {
                    return Ok(vec![Value::Nil]);
                }
                let base = block as usize;
                guest.mem[base..base + len as usize].fill(0);
                Ok(vec![buffer_value(block, len)])
            }),
        );
        self.vm.register_in(
            "string",
            "set",
            Rc::new(|vm, args| {
                let offset = args.get(1).and_then(vh::as_num).unwrap_or(1.0) as i32;
                let value = args.get(2).and_then(vh::as_num).unwrap_or(0.0) as u8;
                match args.first() {
                    Some(Value::Table(pair)) => {
                        let (ptr, size) = buffer_cells(&pair.borrow());
                        if let Some(index) = buffer_index(ptr, size, offset) {
                            if let Some(guest) = vm.guest.as_mut() {
                                guest.mem[index] = value;
                            }
                        }
                    }
                    Some(Value::Str(_)) => {
                        // Plain strings are shared values here; the SDK's
                        // in-place patch only applies to `string.new`
                        // buffers, so this is a no-op.
                    }
                    _ => {}
                }
                Ok(vec![])
            }),
        );
        self.vm.register_in(
            "string",
            "update",
            Rc::new(|vm, args| {
                let source = args.get(1).map(vh::to_string_bytes).unwrap_or_default();
                let offset = args.get(2).and_then(vh::as_num).unwrap_or(1.0) as i32;
                let start = args.get(3).and_then(vh::as_num).unwrap_or(1.0) as i32;
                let end = args
                    .get(4)
                    .and_then(vh::as_num)
                    .map(|value| value as i32)
                    .unwrap_or(source.len() as i32);
                let slice = slice_range(&source, start, end);
                match args.first() {
                    Some(Value::Table(pair)) => {
                        let (ptr, size) = buffer_cells(&pair.borrow());
                        if let Some(guest) = vm.guest.as_mut() {
                            guest_buffer_update(guest, ptr, size, offset, &source[slice]);
                        }
                    }
                    Some(Value::Str(_)) => {
                        // Plain strings are shared values; see `set`.
                    }
                    _ => {}
                }
                Ok(vec![])
            }),
        );
        self.vm.register_in(
            "string",
            "pupdate",
            Rc::new(|vm, args| {
                // pupdate(dst_ptr, dst_len, src_ptr, src_len, [offset, start, end])
                let (Some(dst), Some(dst_len), Some(src), Some(src_len)) = (
                    args.first().and_then(vh::as_num),
                    args.get(1).and_then(vh::as_num),
                    args.get(2).and_then(vh::as_num),
                    args.get(3).and_then(vh::as_num),
                ) else {
                    return Ok(vec![]);
                };
                let (dst, dst_len) = (dst as u32, dst_len as u32);
                let (src, src_len) = (src as u32, src_len as u32);
                let offset = args.get(4).and_then(vh::as_num).unwrap_or(1.0) as i32;
                let start = args.get(5).and_then(vh::as_num).unwrap_or(1.0) as i32;
                let end = args
                    .get(6)
                    .and_then(vh::as_num)
                    .map(|value| value as i32)
                    .unwrap_or(src_len as i32);
                if let Some(guest) = vm.guest.as_mut() {
                    let source: Vec<u8> = (0..src_len).map(|index| guest.r8(src + index)).collect();
                    let slice = slice_range(&source, start, end);
                    guest_buffer_update(guest, dst, dst_len, offset, &source[slice]);
                }
                Ok(vec![])
            }),
        );
        self.vm.register_in(
            "string",
            "find",
            Rc::new(|_vm, args| {
                let text = args.first().map(vh::to_string_bytes).unwrap_or_default();
                let pattern = args.get(1).map(vh::to_string_bytes).unwrap_or_default();
                let init = args
                    .get(2)
                    .and_then(vh::as_num)
                    .map(|value| value as i32)
                    .unwrap_or(1);
                let init = if init > 0 { (init - 1) as usize } else { 0 };
                let plain = args.get(3).map(Value::is_false_y) == Some(false);
                match crate::lua_pattern::find(&text, &pattern, init, plain) {
                    Some((start, end)) => Ok(vec![
                        vh::num_value(start as f32 + 1.0),
                        vh::num_value(end as f32),
                    ]),
                    None => Ok(vec![Value::Nil]),
                }
            }),
        );
        self.vm.register_in(
            "string",
            "gsub",
            Rc::new(|_vm, args| {
                let text = args.first().map(vh::to_string_bytes).unwrap_or_default();
                let pattern = args.get(1).map(vh::to_string_bytes).unwrap_or_default();
                let replacement = args.get(2).map(vh::to_string_bytes).unwrap_or_default();
                let limit = args.get(3).and_then(vh::as_num).map(|value| value as usize);
                Ok(vec![vh::str_value(&crate::lua_pattern::gsub(
                    &text,
                    &pattern,
                    &replacement,
                    limit,
                ))])
            }),
        );
        // The SDK registers the same substitution under `subEx`/`subex`.
        for alias in ["subEx", "subex"] {
            self.vm.register_in(
                "string",
                alias,
                Rc::new(|_vm, args| {
                    let text = args.first().map(vh::to_string_bytes).unwrap_or_default();
                    let pattern = args.get(1).map(vh::to_string_bytes).unwrap_or_default();
                    let replacement = args.get(2).map(vh::to_string_bytes).unwrap_or_default();
                    let limit = args.get(3).and_then(vh::as_num).map(|value| value as usize);
                    Ok(vec![vh::str_value(&crate::lua_pattern::gsub(
                        &text,
                        &pattern,
                        &replacement,
                        limit,
                    ))])
                }),
            );
        }
        self.vm.register_in(
            "string",
            "rep",
            Rc::new(|_vm, args| {
                let text = args.first().map(vh::to_string_bytes).unwrap_or_default();
                let count = args.get(1).and_then(vh::as_num).unwrap_or(0.0).max(0.0) as usize;
                let mut out = Vec::with_capacity(text.len() * count);
                for _ in 0..count {
                    out.extend_from_slice(&text);
                }
                Ok(vec![vh::str_value(&out)])
            }),
        );
        self.vm.register_in(
            "string",
            "byte",
            Rc::new(|_vm, args| {
                let text = args.first().map(vh::to_string_bytes).unwrap_or_default();
                let start = args
                    .get(1)
                    .and_then(vh::as_num)
                    .map(|value| value.max(1.0) as usize - 1)
                    .unwrap_or(0);
                Ok(text
                    .get(start..)
                    .map(|slice| {
                        slice
                            .iter()
                            .take(args.get(2).and_then(vh::as_num).unwrap_or(1.0) as usize)
                            .map(|&byte| vh::num_value(byte as f32))
                            .collect()
                    })
                    .unwrap_or_else(|| vec![Value::Nil]))
            }),
        );
        self.vm.register_in(
            "string",
            "char",
            Rc::new(|_vm, args| {
                let bytes: Vec<u8> = args
                    .iter()
                    .filter_map(vh::as_num)
                    .map(|value| value as u8)
                    .collect();
                Ok(vec![vh::str_value(&bytes)])
            }),
        );
        for (name, mapper) in [
            (
                "reverse",
                Rc::new(|bytes: &[u8]| bytes.iter().rev().copied().collect::<Vec<u8>>())
                    as std::rc::Rc<dyn Fn(&[u8]) -> Vec<u8>>,
            ),
            (
                "upper",
                Rc::new(|bytes: &[u8]| bytes.iter().map(|byte| byte.to_ascii_uppercase()).collect())
                    as std::rc::Rc<dyn Fn(&[u8]) -> Vec<u8>>,
            ),
            (
                "lower",
                Rc::new(|bytes: &[u8]| bytes.iter().map(|byte| byte.to_ascii_lowercase()).collect())
                    as std::rc::Rc<dyn Fn(&[u8]) -> Vec<u8>>,
            ),
        ] {
            self.vm.register_in(
                "string",
                name,
                Rc::new(move |_vm, args| {
                    let text = args.first().map(vh::to_string_bytes).unwrap_or_default();
                    Ok(vec![vh::str_value(&mapper(&text))])
                }),
            );
        }
        self.vm.register_in(
            "math",
            "floor",
            Rc::new(|_vm, args| {
                Ok(vec![vh::num_value(
                    args.first().and_then(vh::as_num).unwrap_or(0.0).floor(),
                )])
            }),
        );
        self.vm.register_in(
            "math",
            "abs",
            Rc::new(|_vm, args| {
                Ok(vec![vh::num_value(
                    args.first().and_then(vh::as_num).unwrap_or(0.0).abs(),
                )])
            }),
        );
        self.vm.register_in(
            "math",
            "random",
            Rc::new(|_vm, args| {
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
            .register_in("math", "randomseed", Rc::new(|_vm, _args| Ok(vec![])));

        // The `file` library: package-backed reads plus guest handles.
        self.vm.register_in(
            "file",
            "open",
            Rc::new(|vm, args| {
                let path = args.first().map(vh::to_string_bytes).unwrap_or_default();
                let name = String::from_utf8_lossy(&path).to_string();
                let Some(guest) = vm.guest.as_mut() else {
                    return Ok(vec![Value::Nil]);
                };
                match guest.file_open(&name) {
                    Some(handle) => {
                        // The SDK file.open returns an object with methods
                        // (f:read(len), f:write(s), f:seek(...), f:close()).
                        let object = match vh::table_value(|table| {
                            table.set(vh::str_value(b"handle"), vh::num_value(handle as f32));
                        }) {
                            Value::Table(table) => table,
                            _ => unreachable!("table_value builds a table"),
                        };
                        for (method, tag) in [
                            ("read", "__file_read"),
                            ("write", "__file_write"),
                            ("seek", "__file_seek"),
                            ("close", "__file_close"),
                            ("state", "__file_state"),
                        ] {
                            let index = vm
                                .native_index(tag)
                                .ok_or_else(|| format!("{tag} missing"))?;
                            object
                                .borrow_mut()
                                .set(vh::str_value(method.as_bytes()), Value::Native(index));
                        }
                        Ok(vec![Value::Table(object)])
                    }
                    None => Ok(vec![Value::Nil]),
                }
            }),
        );
        self.vm.register(
            "__file_read",
            Rc::new(|vm, args| {
                let handle = file_handle(args)?;
                let len = args.get(1).and_then(vh::as_num).unwrap_or(0.0) as usize;
                let Some(guest) = vm.guest.as_mut() else {
                    return Ok(vec![Value::Nil]);
                };
                match guest.file_read(handle, len) {
                    Some(bytes) => Ok(vec![vh::str_value(&bytes)]),
                    None => Ok(vec![Value::Nil]),
                }
            }),
        );
        self.vm.register(
            "__file_write",
            Rc::new(|vm, args| {
                let handle = file_handle(args)?;
                let data = args.get(1).map(vh::to_string_bytes).unwrap_or_default();
                let Some(guest) = vm.guest.as_mut() else {
                    return Ok(vec![vh::num_value(0.0)]);
                };
                Ok(vec![vh::num_value(if guest.file_write(handle, &data) {
                    1.0
                } else {
                    0.0
                })])
            }),
        );
        self.vm.register(
            "__file_seek",
            Rc::new(|vm, args| {
                let handle = file_handle(args)?;
                let offset = args.get(1).and_then(vh::as_num).unwrap_or(0.0) as i32;
                let whence = args.get(2).and_then(vh::as_num).unwrap_or(0.0) as i32;
                let Some(guest) = vm.guest.as_mut() else {
                    return Ok(vec![vh::num_value(0.0)]);
                };
                Ok(vec![vh::num_value(
                    guest.file_seek(handle, offset, whence) as f32
                )])
            }),
        );
        self.vm.register(
            "__file_close",
            Rc::new(|vm, args| {
                let handle = file_handle(args)?;
                if let Some(guest) = vm.guest.as_mut() {
                    guest.file_close(handle);
                }
                Ok(vec![])
            }),
        );
        self.vm.register(
            "__file_state",
            Rc::new(|_vm, _args| Ok(vec![vh::num_value(1.0)])),
        );
        self.vm.register_in(
            "file",
            "read",
            Rc::new(|vm, args| {
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
            Rc::new(|vm, args| {
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
            Rc::new(|vm, args| {
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
            guest.present_frame(&mut self.vm.screen);
        }
    }
}

/// Builds the platform system-info table (`GetSysInfo` / `sys.getInfo`).
fn sysinfo_value(vm: &Vm) -> Value {
    let (imei, imsi, vendor, device) = vm
        .guest
        .as_ref()
        .map(|guest| guest.sysinfo_strings())
        .unwrap_or_default();
    let scrw = vm.screen.width();
    let scrh = vm.screen.height();
    // Glyph metrics the scripts read for text layout: the first CJK sample
    // character and a Latin sample digit.
    let dims = |ch| {
        unifont::get_glyph(ch)
            .map(|glyph| (glyph.get_width() as i32, 16))
            .unwrap_or((8, 16))
    };
    let (chw, chh) = dims('\u{70b9}');
    let (ascw, asch) = dims('2');
    vh::table_value(|table| {
        // Platform field names (short forms plus the legacy aliases older
        // applications expect).
        table.set(vh::str_value(b"vmver"), vh::num_value(1968.0));
        table.set(vh::str_value(b"scrw"), vh::num_value(scrw as f32));
        table.set(vh::str_value(b"scrh"), vh::num_value(scrh as f32));
        table.set(vh::str_value(b"ScreenW"), vh::num_value(scrw as f32));
        table.set(vh::str_value(b"ScreenH"), vh::num_value(scrh as f32));
        table.set(vh::str_value(b"chw"), vh::num_value(chw as f32));
        table.set(vh::str_value(b"chh"), vh::num_value(chh as f32));
        table.set(vh::str_value(b"ChineseWidth"), vh::num_value(chw as f32));
        table.set(vh::str_value(b"ChineseHigh"), vh::num_value(chh as f32));
        table.set(vh::str_value(b"ascw"), vh::num_value(ascw as f32));
        table.set(vh::str_value(b"asch"), vh::num_value(asch as f32));
        table.set(vh::str_value(b"EnglishWidth"), vh::num_value(ascw as f32));
        table.set(vh::str_value(b"EnglishHigh"), vh::num_value(asch as f32));
        table.set(vh::str_value(b"packname"), vh::str_value(b""));
        table.set(vh::str_value(b"hsman"), vh::str_value(&vendor));
        table.set(vh::str_value(b"hstype"), vh::str_value(&device));
        table.set(vh::str_value(b"hsver"), vh::num_value(0.0));
        table.set(vh::str_value(b"IMEI"), vh::str_value(&imei));
        table.set(vh::str_value(b"IMSI"), vh::str_value(&imsi));
        table.set(vh::str_value(b"manufactory"), vh::str_value(&vendor));
        table.set(vh::str_value(b"type"), vh::str_value(&device));
    })
}

/// Builds the platform datetime table (`GetDatetime` / `sys.datetime`).
fn datetime_value() -> Value {
    let now = crate::cfunction::local_datetime();
    vh::table_value(|table| {
        table.set(vh::str_value(b"year"), vh::num_value(now.0 as f32));
        table.set(vh::str_value(b"mon"), vh::num_value(now.1 as f32));
        table.set(vh::str_value(b"day"), vh::num_value(now.2 as f32));
        table.set(vh::str_value(b"hour"), vh::num_value(now.3 as f32));
        table.set(vh::str_value(b"min"), vh::num_value(now.4 as f32));
        table.set(vh::str_value(b"sec"), vh::num_value(now.5 as f32));
    })
}

/// The generic-for iterator registered for `iPairs`.
const IPAIRS_ITER_NAME: &str = "ipairs_iter";

/// Extracts the handle from a file-object method call (`args[0].handle`).
fn file_handle(args: &[Value]) -> Result<usize, String> {
    let Some(Value::Table(object)) = args.first() else {
        return Err("file method called on a non-object".to_string());
    };
    let handle = object.borrow().get(&vh::str_value(b"handle"));
    let handle = handle.as_num().unwrap_or(0.0) as usize;
    if handle == 0 {
        return Err("file handle is closed".to_string());
    }
    Ok(handle)
}

/// The mythroad base64 alphabet mapping (encode direction).
fn mr_encode_table(value: u8) -> Option<u8> {
    match value {
        7 => Some(b'D'),
        14 => Some(b'h'),
        59 => Some(b'/'),
        62 => Some(b'+'),
        63 => Some(b'x'),
        0..=10 => Some(value + b'a'),
        11..=36 => Some(value - 11 + b'A'),
        37..=46 => Some(value - 37 + b'0'),
        47..=61 => Some(value - 47 + b'l'),
        _ => None,
    }
}

fn mr_decode_table(value: u8) -> Option<u8> {
    match value {
        b'D' => Some(7),
        b'h' => Some(14),
        b'x' => Some(63),
        b'+' => Some(62),
        b'/' => Some(59),
        b'=' => Some(64),
        b'A'..=b'Z' => Some(value - b'A' + 11),
        b'a'..=b'k' => Some(value - b'a'),
        b'l'..=b'z' => Some(value - b'l' + 47),
        b'0'..=b'9' => Some(value - b'0' + 37),
        _ => None,
    }
}

/// `_mr_encode`: the platform's base64 variant.
pub fn mr_encode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4 + 1);
    let chunks = input.len() / 3;
    for chunk in 0..chunks {
        let base = chunk * 3;
        let (b0, b1, b2) = (input[base], input[base + 1], input[base + 2]);
        out.push(mr_encode_table(b0 >> 2)?);
        out.push(mr_encode_table((b0 & 0x03) << 4 | b1 >> 4)?);
        out.push(mr_encode_table((b1 & 0x0F) << 2 | b2 >> 6)?);
        out.push(mr_encode_table(b2 & 0x3F)?);
    }
    let rest = input.len() % 3;
    if rest != 0 {
        let base = chunks * 3;
        let mut buf = [0u8; 3];
        buf[..rest].copy_from_slice(&input[base..base + rest]);
        out.push(mr_encode_table(buf[0] >> 2)?);
        out.push(mr_encode_table((buf[0] & 0x03) << 4 | buf[1] >> 4)?);
        out.push(mr_encode_table((buf[1] & 0x0F) << 2 | buf[2] >> 6)?);
        out.push(mr_encode_table(buf[2] & 0x3F)?);
        let len = out.len();
        for pad in 0..3 - rest {
            out[len - pad - 1] = b'=';
        }
    }
    out.push(0);
    Some(out)
}

/// `_mr_decode`: the matching decoder; returns the payload bytes.
pub fn mr_decode(input: &[u8]) -> Option<Vec<u8>> {
    if input.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let blocks = (input.len() - 4) / 4;
    let mut cursor = 0usize;
    for _ in 0..blocks {
        let mut group = [0u8; 4];
        for slot in &mut group {
            *slot = mr_decode_table(input[cursor])?;
            cursor += 1;
        }
        out.push(group[0] << 2 | (group[1] & 0x30) >> 4);
        out.push((group[1] & 0x0F) << 4 | (group[2] & 0x3C) >> 2);
        out.push((group[2] & 0x03) << 6 | (group[3] & 0x3F));
    }
    let mut group = [0u8; 4];
    for slot in &mut group {
        *slot = mr_decode_table(input[cursor])?;
        cursor += 1;
    }
    let padding = if input.len() >= 2 && input[input.len() - 2] == b'=' {
        2
    } else if input.last() == Some(&b'=') {
        1
    } else {
        0
    };
    for slot in group.iter_mut().skip(4 - padding) {
        *slot = 0;
    }
    out.push(group[0] << 2 | (group[1] & 0x30) >> 4);
    out.push((group[1] & 0x0F) << 4 | (group[2] & 0x3C) >> 2);
    out.push((group[2] & 0x03) << 6 | (group[3] & 0x3F));
    out.truncate(out.len() - padding);
    Some(out)
}

/// The `next` native shared by the global and the `table.next` member.
fn next_native_ref() -> Native {
    Rc::new(|_vm, args| {
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
    })
}

/// `rawequal(a, b)`: identity-free value comparison.
fn raw_equal_native(_vm: &mut Vm, args: &[Value]) -> Result<Vec<Value>, String> {
    let equal = match (args.first(), args.get(1)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    Ok(vec![vh::num_value(if equal { 1.0 } else { 0.0 })])
}

/// `rawget(table, key)`: plain table lookup (no metatables in the VM).
fn raw_get_native(_vm: &mut Vm, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(Value::Table(table)) = args.first() else {
        return Err("attempt to index a non-table value".to_string());
    };
    let key = args.get(1).cloned().unwrap_or(Value::Nil);
    Ok(vec![table.borrow().get(&key)])
}

/// `unpack(table)`: the array part as multiple results.
fn unpack_native(_vm: &mut Vm, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(Value::Table(table)) = args.first() else {
        return Err("bad argument #1 to 'unpack' (table expected)".to_string());
    };
    Ok(table.borrow().array.clone())
}

/// `table.insert(t, [pos,] value)`: appends or inserts into the array part.
fn table_insert_native(_vm: &mut Vm, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(Value::Table(table)) = args.first() else {
        return Err("bad argument #1 to 'insert' (table expected)".to_string());
    };
    let mut table = table.borrow_mut();
    match (args.get(1), args.get(2)) {
        (Some(value), None) => table.array.push(value.clone()),
        (Some(Value::Num(pos)), Some(value)) => {
            let pos = (*pos as usize).saturating_sub(1).min(table.array.len());
            table.array.insert(pos, value.clone());
        }
        _ => return Err("wrong number of arguments to 'insert'".to_string()),
    }
    Ok(vec![])
}

/// `table.remove(t [, pos])`: removes and returns the array-part element.
fn table_remove_native(_vm: &mut Vm, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(Value::Table(table)) = args.first() else {
        return Err("bad argument #1 to 'remove' (table expected)".to_string());
    };
    let mut table = table.borrow_mut();
    let pos = match args.get(1).and_then(vh::as_num) {
        Some(value) => (value as usize).saturating_sub(1),
        None => table.array.len().saturating_sub(1),
    };
    if pos < table.array.len() {
        Ok(vec![table.array.remove(pos)])
    } else {
        Ok(vec![Value::Nil])
    }
}

/// Loads a packaged `.mr` member as a callable MiniJ closure. The loader
/// alias `_loadFile` shares this path.
fn load_package_chunk(vm: &mut Vm, name: &str) -> Result<Value, String> {
    let data = vm
        .package
        .as_ref()
        .and_then(|package| package.read_entry(name).ok())
        .ok_or_else(|| format!("cannot open {name:?}"))?;
    let chunk = crate::undump::Chunk::parse(&data)
        .map_err(|error| format!("cannot load {name:?}: {error}"))?;
    Ok(Value::Closure(Rc::new(crate::vm::LuaClosure {
        proto: Rc::new(chunk.proto),
        upvals: Vec::new(),
    })))
}

/// `_com(input0, input1, [len], [code])`: the script → runtime bridge.
/// 800 loads an ext image (input1 = the ext bytes as a string), 801 calls
/// the registered handler with (code, input bytes).
fn com_native(vm: &mut Vm, args: &[Value]) -> Result<Vec<Value>, String> {
    let input0 = args.first().and_then(vh::as_num).unwrap_or(0.0) as i32;
    log::trace!("_com input0={input0:#x} nargs={}", args.len());
    match input0 {
        // The startup handshake: `_com(3629, 2913)` marks the platform
        // "BI" (boot-initialized) flag once the wrapper script is running.
        3629 => {
            let input1 = args.get(1).and_then(vh::as_num).unwrap_or(0.0) as i32;
            if input1 == 2913 {
                if let Some(guest) = vm.guest.as_mut() {
                    guest.update_flags(0x1, 0);
                }
            }
            Ok(vec![vh::num_value(0.0)])
        }
        // Handler registrations and platform queries (the vmrp TestCom
        // contract): 1 = uptime, 2-6 = event/timer/stop/pause/resume.
        1 => Ok(vec![vh::num_value(
            vm.guest
                .as_ref()
                .map(|guest| guest.uptime_ms())
                .unwrap_or(0) as f32,
        )]),
        2..=4 => {
            let global = match input0 {
                2 => "__event_cb",
                3 => "__timer_cb",
                _ => "__stop_cb",
            };
            let value = args.get(1).cloned().unwrap_or(Value::Nil);
            vm.globals
                .borrow_mut()
                .set(vh::str_value(global.as_bytes()), value);
            Ok(vec![vh::num_value(0.0)])
        }
        5 | 6 => Ok(vec![vh::num_value(0.0)]),
        100 => Ok(vec![vh::num_value(4.0)]),
        101 | 102 => Ok(vec![vh::num_value(1024.0 * 1024.0)]),
        300 => Ok(vec![vh::num_value(1.0)]),
        301 => Ok(vec![vh::num_value(1.0)]),
        400 => Ok(vec![vh::num_value(0.0)]),
        401 => Ok(vec![vh::num_value(240.0)]),
        // setgcthreshold: the interpreter has no incremental GC to tune.
        403 => Ok(vec![vh::num_value(0.0)]),
        800 | 802 => {
            let code = args
                .get(2)
                .and_then(vh::as_num)
                .map(|value| value as i32)
                .unwrap_or(0);
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
            let number;
            let bytes = match args.get(1) {
                Some(Value::Str(text)) => text.as_slice(),
                Some(Value::Num(value)) => {
                    number = value.to_le_bytes();
                    &number
                }
                _ => &[],
            };
            match call_native_buffer(vm, code, NativeInput::Bytes(bytes)) {
                Ok(output) => Ok(output),
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

/// `_strCom(input0, payload, [code])`: the string-input dispatcher. The
/// payload is either a Lua string or a `{ptr, len}` pair addressing guest
/// memory. 601 reads a package file, 800 loads an ext image, 801 calls the
/// registered handler with (code, payload bytes).
fn str_com_native(vm: &mut Vm, args: &[Value]) -> Result<Vec<Value>, String> {
    let input0 = args.first().and_then(vh::as_num).unwrap_or(0.0) as i32;
    let code = args.get(2).and_then(vh::as_num).unwrap_or(0.0) as i32;
    if input0 == 801 {
        let input = match args.get(1) {
            Some(Value::Str(text)) => NativeInput::Bytes(text),
            Some(Value::Table(pair)) => {
                let (pointer, length) = buffer_cells(&pair.borrow());
                NativeInput::Guest(pointer, length)
            }
            _ => NativeInput::Bytes(&[]),
        };
        return match call_native_buffer(vm, code, input) {
            Ok(output) => Ok(output),
            Err(error) => {
                log::error!("_strCom(801) failed: {error}");
                Ok(vec![vh::str_value(b""), vh::num_value(-1.0)])
            }
        };
    }
    let payload: Vec<u8> = match args.get(1) {
        Some(Value::Str(text)) => text.as_ref().clone(),
        Some(Value::Table(pair)) => {
            let (pointer, length) = buffer_cells(&pair.borrow());
            vm.guest
                .as_ref()
                .and_then(|guest| guest.guest_bytes(pointer, length as usize))
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };
    match input0 {
        // Read a file from the running package (gzip-transparent).
        601 => {
            let name = String::from_utf8_lossy(&payload).to_string();
            match vm.package.as_ref() {
                Some(package) => match package.read_entry(&name) {
                    Ok(bytes) => Ok(vec![Value::str_from(&bytes)]),
                    Err(error) => {
                        log::error!("_strCom(601) read {name:?} failed: {error}");
                        Ok(vec![Value::Nil])
                    }
                },
                None => Ok(vec![Value::Nil]),
            }
        }
        800 | 802 => {
            if payload.is_empty() {
                return Ok(vec![vh::num_value(-1.0)]);
            }
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
            let result = guest.load_ext_at(&payload, crate::cfunction::EXT_BASE, code, &mut screen);
            vm.screen = screen;
            match result {
                Ok(value) => Ok(vec![vh::num_value(value as f32)]),
                Err(error) => {
                    log::error!("_strCom(800) failed: {error}");
                    Ok(vec![vh::num_value(-1.0)])
                }
            }
        }
        other => {
            log::warn!("_strCom got unknown input0 {other:#x}");
            Ok(vec![vh::num_value(-1.0)])
        }
    }
}

enum NativeInput<'a> {
    Bytes(&'a [u8]),
    Guest(u32, u32),
}

fn call_native_buffer(
    vm: &mut Vm,
    code: i32,
    input: NativeInput<'_>,
) -> Result<Vec<Value>, String> {
    let guest = vm.guest.as_mut().ok_or("no guest")?;
    let (pointer, length, allocation) = match input {
        NativeInput::Bytes(bytes) => {
            let length = i32::try_from(bytes.len()).map_err(|error| error.to_string())?;
            let allocation = length as u32 + 1;
            let pointer = guest.heap_malloc(allocation);
            if pointer == 0 {
                return Err("native input allocation failed".to_string());
            }
            let start = pointer as usize;
            guest.mem[start..start + bytes.len()].copy_from_slice(bytes);
            guest.w8(pointer + length as u32, 0);
            (pointer, length, allocation)
        }
        NativeInput::Guest(pointer, length) => {
            if pointer as usize > guest.mem.len()
                || length as usize > guest.mem.len() - pointer as usize
            {
                return Err("native input points outside guest memory".to_string());
            }
            (pointer, length as i32, 0)
        }
    };
    let instruction_limit = match code {
        0 | 6 | 8 => crate::cfunction::BOOT_INSTRUCTION_LIMIT,
        _ => crate::cfunction::DEFAULT_INSTRUCTION_LIMIT,
    };
    let result = guest
        .call_c_function_full(code, pointer, length, instruction_limit, &mut vm.screen)
        .map(|(status, (output, length))| {
            let bytes = if output == 0 {
                Vec::new()
            } else {
                guest
                    .guest_bytes(output, length as usize)
                    .unwrap_or_default()
            };
            vec![vh::str_value(&bytes), vh::num_value(status as f32)]
        });
    if allocation != 0 {
        guest.heap_free(pointer, allocation);
    }
    result.map_err(|error| error.to_string())
}

/// Builds the `{pointer, size}` table value that represents a guest-side
/// raw buffer string (the SDK `string.new` shape).
fn buffer_value(ptr: u32, size: u32) -> Value {
    let mut table = crate::vm::Table::default();
    table.array.push(vh::num_value(ptr as f32));
    table.array.push(vh::num_value(size as f32));
    Value::Table(Rc::new(std::cell::RefCell::new(table)))
}

/// Reads the `(pointer, size)` cells out of a buffer-table string.
fn buffer_cells(table: &crate::vm::Table) -> (u32, u32) {
    let read = |index: usize| -> u32 {
        table
            .array
            .get(index)
            .and_then(vh::as_num)
            .unwrap_or(0.0)
            .max(0.0) as u32
    };
    (read(0), read(1))
}

/// Resolves a 1-based (negative = from-end) byte offset into a 0-based
/// index; `None` when out of bounds. Mirrors the SDK's offset arithmetic.
fn buffer_index(base: u32, size: u32, offset: i32) -> Option<usize> {
    let index = if offset < 0 {
        size as i64 + offset as i64
    } else {
        offset as i64 - 1
    };
    if index < 0 || index >= size as i64 {
        return None;
    }
    Some((base as usize) + index as usize)
}

/// Clips a 1-based inclusive `start..=end` range against a byte slice
/// (negative values count from the end), per the SDK's update semantics.
fn slice_range(data: &[u8], start: i32, end: i32) -> std::ops::Range<usize> {
    let len = data.len() as i32;
    let begin = (if start < 0 { len + start } else { start - 1 }).clamp(0, len);
    let stop = (if end < 0 { len + end } else { end }).clamp(begin, len);
    begin as usize..stop as usize
}

/// Copies `source` into a guest buffer at the 1-based offset, clamping to
/// the buffer's remaining space (`string.update` / `string.pupdate`).
fn guest_buffer_update(
    guest: &mut crate::cfunction::Guest,
    base: u32,
    size: u32,
    offset: i32,
    source: &[u8],
) {
    let Some(index) = buffer_index(base, size, offset) else {
        return;
    };
    let remaining = (base + size) as usize - index;
    let count = source.len().min(remaining);
    guest.mem[index..index + count].copy_from_slice(&source[..count]);
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
            Rc::new(move |_vm, _args| {
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

// ---------------------------------------------------------------------------
// Snapshot serialization. The VM travels through its own pooled impl; the
// native registry rebuilds from the platform template after decode (the
// machine drives that via `rebuild_snapshot_state`).
// ---------------------------------------------------------------------------

impl ScriptBridge {
    /// Builds the platform native registry for snapshot restoration.
    pub(crate) fn platform_native_registry() -> (Vec<Native>, Vec<&'static str>) {
        let mut bridge = Self::new();
        bridge.vm.take_native_registry()
    }
}

impl serde::Serialize for ScriptBridge {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Data<'a> {
            vm: &'a Vm,
            missing_globals: Vec<String>,
            pending_files: &'a Option<Vec<(String, Vec<u8>)>>,
        }
        Data {
            vm: &self.vm,
            missing_globals: self.missing_globals.borrow().iter().cloned().collect(),
            pending_files: &self.pending_files,
        }
        .serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ScriptBridge {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Data {
            vm: Vm,
            missing_globals: Vec<String>,
            pending_files: Option<Vec<(String, Vec<u8>)>>,
        }
        let data = Data::deserialize(deserializer)?;
        Ok(Self {
            vm: data.vm,
            missing_globals: RefCell::new(data.missing_globals.into_iter().collect()),
            pending_files: data.pending_files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cfunction::{Guest, GuestConfig, EXT_BASE};

    #[test]
    fn native_script_calls_pass_the_whole_payload() {
        let mut vm = Vm::new();
        let mut guest = Guest::new(GuestConfig::default(), Vec::new());
        // Return the last byte of the native input buffer.
        guest.w32(EXT_BASE, 0xe2433001); // sub r3, r3, #1
        guest.w32(EXT_BASE + 4, 0xe7d20003); // ldrb r0, [r2, r3]
        guest.w32(EXT_BASE + 8, 0xe12fff1e); // bx lr
        guest.c_function = EXT_BASE;
        let mut data = vec![0; 6000];
        data[5999] = 0x5a;
        let pointer = guest.heap_malloc(data.len() as u32);
        guest.mem[pointer as usize..pointer as usize + data.len()].copy_from_slice(&data);
        vm.guest = Some(guest);

        let command = vh::num_value(801.0);
        let result = com_native(&mut vm, &[command.clone(), vh::str_value(&data)]).unwrap();
        assert_eq!(result.get(1).and_then(vh::as_num), Some(90.0));
        let result = str_com_native(&mut vm, &[command, buffer_value(pointer, 6000)]).unwrap();
        assert_eq!(result.get(1).and_then(vh::as_num), Some(90.0));
    }
}
