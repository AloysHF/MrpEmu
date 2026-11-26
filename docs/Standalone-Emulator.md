# Standalone Emulator

This guide covers building and running the standalone `mrpemu` desktop
frontend, its command-line options, and the key mappings.

> **Status: experimental.** The frontend runs MRP applications and displays
> their RGB565 frames. See [Application Compatibility](Game-Compatibility.md)
> for the verification criteria.

## Supported Platforms

The frontend is built on minifb and targets the desktop platforms:

| Platform | Architecture |
|----------|-------------|
| Windows | x86_64 |
| macOS | x86_64, aarch64 |
| Linux | x86_64, aarch64 |

## Installation

Download a binary from the
[Releases](https://github.com/AloysHF/MrpEmu/releases) page when available, or
build from source:

```bash
cargo build -p mrpemu --release
```

The binary is produced at `target/release/mrpemu` (`.exe` on Windows).

## Synopsis

```text
mrpemu [OPTIONS] <MRP_PATH>
```

## Options

| Option | Value | Description |
|---|---|---|
| `<MRP_PATH>` | path | *required* — path to the `.mrp` application package. |
| `--scale <SCALE>` | integer | Integer window scale factor applied to the detected framebuffer size. Defaults to 2. |
| `--system-dir <DIR>` | path | Root directory for optional Mythroad device files. Defaults to the package's parent directory. |
| `--headless` | flag | Run without opening a window, then exit. |
| `--frames <COUNT>` | integer | Number of frames to run in headless mode. Defaults to 60. |
| `--instruction-limit <COUNT>` | integer | Total guest instruction budget for boot and headless frames; `0` is unlimited. Defaults to 512000000. Ignored in windowed mode. |
| `-S, --screenshot <PATH>` | path | Save a PNG of the presented frame and exit. Implies headless. |
| `--screenshot-frames <COUNT>` | integer | Number of frames to run before capturing. Defaults to 200. |
| `--press <FRAME:KEY>` | text | Scripted key press at an exact frame; repeatable. Headless and screenshot runs only. |
| `-v, --verbose` | flag | Enable debug logging. |

The screen defaults to 240×320. A file or parent folder labeled `240x400`
or `320x480` selects that size. Labels can also use the multiplication sign
`×`.

## Loading Applications

```bash
# Load an application at its original screen size
mrpemu path/to/app.mrp --scale 1

# Open a 2x window (480×640)
mrpemu path/to/app.mrp --scale 2

# Or straight from a source checkout
cargo run --release -p mrpemu -- path/to/app.mrp
```

### Optional device files

Some packages expect a platform plugin, such as the Flash player. Place
your device files under `--system-dir` with this layout. Missing files
are skipped and the emulator supplies fallback bitmap fonts.

```text
plugins/flaengine.mrp
plugins/netpay.mrp
system/gb12.uc2
system/gb12_uc2.adl
system/gb16.uc2
```

## Headless capture

A headless run uses a fixed 30 Hz guest clock, so a given package and press
schedule produce the same frame sequence on every machine. Each `--press`
fires one key-down edge and one key-up edge on consecutive frames:

```bash
# Run 120 frames without a window
mrpemu path/to/app.mrp --headless --frames 120

# Capture the frame after 200 scripted frames and two key presses
mrpemu path/to/app.mrp -S shot.png --screenshot-frames 200 \
  --press 30:lsk --press 65:ok
```

Accepted key names are `up`, `down`, `left`, `right`, `ok` (also `enter`,
`confirm`), `lsk` (also `leftsoft`), `rsk` (also `rightsoft`), `0`-`9`,
`star`, and `pound`. Invalid specifications are rejected before the guest
starts. Screenshot runs log `frame nonzero=N colors=C`; the batch script
parses this line to distinguish a rendered frame from a blank one.

To capture a whole corpus:

```powershell
scripts/batch-screenshots.ps1 -GameDirectory path/to/mrp-games
```

The script accepts `-Frames`, `-InstructionLimit`, `-Binary`,
`-OutputDirectory` (default `docs/images`), `-SystemDirectory` and a
repeatable `-Press` schedule. `-Press 30:lsk,60:lsk,90:lsk` applies those
presses to every package; without it, captures are idle. It builds the
release binary when `-Binary` is omitted. Duplicate base names get a
numeric suffix; games under an `A`/`B`/`C` group directory are written to
the matching subdirectory. Per-title frame counts and press sequences can
be added to the `$recipes` table (an empty `Press` array opts a title out
of the global schedule) after the sequence is verified in
[Application Compatibility](Game-Compatibility.md).

## Key Mappings

The guest keypad model covers the direction keys, OK, the left/right soft
keys, and the number keys. The desktop frontend currently binds:

| Phone input | Keyboard |
| --- | --- |
| Direction pad | Arrow keys |
| OK / confirm | Enter |
| Left soft key | Z |
| Right soft key | X |
| Digits | 0–9 |
| Exit | Escape |

The RetroArch core uses its own input path; see the
[RetroArch Core](RetroArch-Core.md) guide.

The window displays the emulated framebuffer at the chosen scale. If all boot
paths fail, the desktop process reports a startup error for the package.
