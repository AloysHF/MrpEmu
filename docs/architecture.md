# Architecture

MrpEmu emulates applications for the mythroad (MRP) platform. This page
describes how that platform is put together, how the emulator is layered, and
what is implemented today.

## The mythroad/MRP platform

MRP is the application platform that Sky-mobi operated on MTK-based Chinese
feature phones. On the handset the platform is a runtime layer (mythroad)
written in ARM code: it loads `.mrp` application packages, exposes the C API
applications are written against — the `mrc_`-prefixed functions — and owns
the phone's screen, keypad, filesystem and network services.

An `.mrp` package is an archive of the application's files. The application
logic ships as MiniJ bytecode, the instruction set of the platform's virtual
machine. A package may additionally carry `ext` entries: native ARM libraries
that extend the VM with device-specific or performance-critical code.

Applications target the phones' LCD — typically 240×320 with an RGB565
framebuffer — and store text in GBK.

Running an application therefore involves three layers the emulator must
provide:

1. **MiniJ VM** — executes the bytecode inside the package.
2. **`mrc_` API bridge** — reimplements the platform services the bytecode
   calls.
3. **ext execution** — runs the optional native ARM libraries, which talk to
   the phone hardware directly.

## Workspace layout

MrpEmu is a Rust workspace split into three crates:

```
crates/
├── mrpemu-core/       # Platform-independent emulator engine (library)
├── mrpemu/            # Standalone desktop frontend, minifb (binary: mrpemu)
└── mrpemu-libretro/   # Libretro core (cdylib: mrpemu_libretro)
```

- **`mrpemu-core`** — everything platform-independent: `.mrp` package
  parsing, the RGB565 framebuffer, the feature-phone keypad input model, and
  the in-memory guest filesystem. The MiniJ interpreter and the `mrc_` service
  bridge will live here as well. The core depends on no windowing system and
  no frontend; frontends drive it through a framebuffer plus a keypad
  interface.
- **`mrpemu`** — the standalone desktop frontend built on minifb. It loads an
  `.mrp` path from the command line, presents the core's framebuffer in a
  window, and feeds host keyboard events into the keypad model.
- **`mrpemu-libretro`** — the libretro core. It loads `.mrp` content and
  reports the 240×320 RGB565 surface as libretro video; input, audio and save
  states will follow as the core matures.

## Emulation strategy

MrpEmu follows a pure-Rust, high-level emulation (HLE) route: the platform
layer is reimplemented from its observable behavior and public documentation,
not by emulating a specific chipset or copying proprietary runtime code.

### MiniJ interpreter

MRP bytecode is executed by a self-developed MiniJ interpreter inside
`mrpemu-core`. Interpreting the bytecode in Rust keeps the core portable,
debuggable and independent of any proprietary runtime. **Not yet implemented**
— this is the central open work item.

### `mrc_` service bridge

Applications reach the phone through the `mrc_`-prefixed C API: display and
drawing, GBK text, input, files, timers, and the rest of the platform surface.
The bridge implements these services natively in Rust against the core's
framebuffer, input model and VFS. **Not yet implemented.**

### ext (native ARM) libraries — open decision point

Some packages ship native ARM code in `ext` entries. Executing that code
requires an ARM CPU subsystem (an interpreter or a recompiler) and a policy
for how such code may reach the platform services. How and in what form
MrpEmu implements this is a deliberate follow-up decision. **Nothing is
implemented today**; packages that require ext code cannot run until this
exists.

## Current implementation status

Implemented (0.1.0 scaffold):

- 200-byte `.mrp` header parsing with GBK text decoding: magic, data offset,
  package size, file-list offset, internal name, display name, IMEI, version,
  app ID, author and description
- RGB565 framebuffer at 240×320
- Feature-phone keypad input model: direction keys, OK, left/right soft keys,
  number keys
- In-memory VFS skeleton
- Desktop frontend (`mrpemu`, minifb): loads an `.mrp` file and opens a window
- Libretro core (`mrpemu_libretro`): loads content and reports 240×320 RGB565
  video

Not yet implemented:

- MiniJ VM execution — applications do not run
- `mrc_` API bridge
- ext ARM code execution
- Audio
- Save states
- Networking

## Boot flow (today)

1. The frontend receives an `.mrp` path (CLI argument or libretro content).
2. `mrpemu-core` validates the package header and decodes the GBK metadata
   fields (see [MRP Format](Mrp-Format.md)).
3. A 240×320 RGB565 framebuffer is created and presented — a desktop window in
   the standalone frontend, a `video_refresh` callback in the libretro core.
4. Host input is translated into the keypad model.
5. Execution ends here for now: bytecode interpretation and `mrc_` service
   calls are the next milestones.
