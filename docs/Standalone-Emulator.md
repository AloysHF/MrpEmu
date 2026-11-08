# Standalone Emulator

This guide covers building and running the standalone `mrpemu` desktop
frontend, its command-line options, and the key mappings.

> **Status: early scaffold.** The frontend parses the `.mrp` package header
> and opens a window showing the core's RGB565 framebuffer. Guest applications
> do not execute yet — see
> [Architecture](architecture.md#current-implementation-status) for what is
> and is not implemented.

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
| `--scale <SCALE>` | integer | Integer window scale factor applied to the 240×320 framebuffer. |

More options (scaling filters, key remapping, headless mode) will appear as
the frontend grows; see the
[implementation status](architecture.md#current-implementation-status).

## Loading Applications

```bash
# Load an application (240×320 window)
mrpemu path/to/app.mrp

# Open a 2x window (480×640)
mrpemu path/to/app.mrp --scale 2

# Or straight from a source checkout
cargo run --release -p mrpemu -- path/to/app.mrp
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
| Exit | Escape |

The RetroArch core uses its own input path; see the
[RetroArch Core](RetroArch-Core.md) guide.

## What the window shows today

- The package header is parsed and the 240×320 RGB565 framebuffer is displayed
  at the chosen scale.
- Because the MiniJ VM is not implemented yet, the framebuffer does not show
  application output, and key presses are recorded by the core's keypad model
  without a guest to receive them.
