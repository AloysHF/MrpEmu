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
  parsing, the MiniJ and ARM/Thumb interpreters, the service bridge, the
  RGB565 framebuffer, phone input, and guest storage. The input interface
  accepts keypad state and a single touch in guest screen coordinates.
- **`mrpemu`** — the standalone desktop frontend built on minifb. It loads an
  `.mrp` path from the command line, presents the core's framebuffer in a
  window, and maps the keyboard to the keypad and the left mouse button
  to touch press, drag, and release.
- **`mrpemu-libretro`** — loads content, reports RGB565 game frames and
  screen geometry, and maps RetroPad input to the phone keypad. It converts
  the frontend's normalized pointer into the same guest touch coordinates
  used by the desktop frontend.

## Emulation strategy

MrpEmu follows a pure-Rust, high-level emulation (HLE) route: the platform
layer is reimplemented from its observable behavior and public documentation,
not by emulating a specific chipset or copying proprietary runtime code.

### MiniJ interpreter

The MiniJ interpreter loads and executes `start.mr`. Platform natives expose
package files, input, drawing operations, and ext loading to guest scripts.

### `mrc_` service bridge

Applications reach the phone through the `mrc_`-prefixed C API: display and
drawing, GBK text, input, files, timers, and the rest of the platform surface.
The bridge exposes these services through guest function tables. Open file
handles share the same virtual files, so an application can generate a
resource cache and read it later in the same session. Native code can
register a RAM package and use the file bridge to look up, borrow or
decompress its members. It can also select a package in guest storage.
The frontends may seed guest storage with optional device files at load
time. The core supplies fallback bitmap fonts and the GB12 companion file
for applications that read them directly. Native edit dialogs accept text
from the frontend and return UCS-2 text through the guest's edit handle.
The framebuffer supports 240×320, 240×400, and 320×480 displays.
Native drawing follows the current guest framebuffer pointer, and an
explicit video submission retains its pixels for the frontend. Optional
platform extended RAM is allocated separately from the SDK's main heap.

### Native extensions

The core executes native extensions in a 32-bit ARM/Thumb guest. Library
entry points register an application handler. The service bridge dispatches
platform calls and can invoke a secondary ext entry point while preserving
the suspended ARM caller's registers. MiniJ can send binary strings and
guest memory buffers to the handler without truncating the payload.

### Audio

The native `mr_playSound` bridge forwards Standard MIDI Files (type 0,
formats 0 and 1 with ticks-per-quarter-note timing) and decodes PCM WAV
(type 1, mono or stereo with 8-bit or 16-bit samples), MP3 (type 2), and
AMR-NB files (type 5, 8 kHz mono). The shared audio player handles play,
loop, and stop for one active sound. It synthesizes
MIDI with RustySynth-ext and the embedded FluidR3Mono GM 2.312 sound bank,
so MIDI playback requires no device files or system MIDI synthesizer.
The player resamples decoded clips and renders normalized stereo PCM on request.

The desktop frontend uses CPAL to pull PCM at the device rate. Libretro and
EmulatorJS submit 1,600 signed 16-bit stereo frames per guest update at 48 kHz
and 30 updates per second. Headless and screenshot runs use the silent sink
and skip MIDI synthesis. Work on additional audio formats is ongoing.

## Current implementation status

The core runs MiniJ and native ARM/Thumb code and exposes display, keypad,
file, and timer services. Platform service coverage, save states, and
networking are ongoing work.

## Boot flow (today)

1. The frontend receives an `.mrp` path (CLI argument or libretro content).
2. `mrpemu-core` validates the package header and decodes the GBK metadata
   fields (see [MRP Format](Mrp-Format.md)).
3. The core initializes its framebuffer at the detected size. It runs
   the MiniJ startup script when it initializes a native wrapper, with
   direct native boot available for packages that use that path.
4. Both frontends drive frame updates at 30 Hz. The core uses the supplied
   guest time to deliver key edges and due timers during each update.
5. The standalone frontend converts RGB565 pixels for its window. The
   libretro core sends RGB565 pixels to the frontend's `video_refresh` callback.
