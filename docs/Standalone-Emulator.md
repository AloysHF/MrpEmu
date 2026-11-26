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
