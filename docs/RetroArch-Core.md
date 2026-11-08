# RetroArch Core

This guide covers installing and running the MrpEmu libretro core for
RetroArch, loading content, and the current feature set.

> **Status: experimental.** MrpEmu is in scaffold stage: the core loads
> `.mrp` content and reports video to the frontend, but guest applications do
> not execute yet (the MiniJ VM is not implemented). See
> [Architecture](architecture.md#current-implementation-status).

## Supported Platforms

| Platform | Architecture | Libretro |
|----------|-------------|----------|
| Windows | x86_64 | ✅ (experimental) |
| macOS | x86_64, aarch64 | ✅ (experimental) |
| Linux | x86_64, aarch64 | ✅ (experimental) |

## Installation

### Manual Installation

Build the libretro core:

```bash
cargo build -p mrpemu-libretro --release
```

Cargo names the cdylib after its lib target, so this produces `mrpemu.dll` on
Windows (`libmrpemu.so` on Linux, `libmrpemu.dylib` on macOS) under
`target/release/`.

RetroArch expects the core file to be named `mrpemu_libretro.<ext>`, so rename
it accordingly before placing it into RetroArch's `cores/` directory. If the
repository ships `mrpemu_libretro.info`, copy it into RetroArch's `info/`
directory so the frontend can display the core metadata.

## Loading Content

1. Open RetroArch and select **Load Core > MrpEmu**.
2. Select **Load Content**.
3. Choose a `.mrp` file.

## Current Features

- `.mrp` content loading
- Video output at the platform's 240×320 resolution using the RGB565 pixel
  format

## Planned Features

Not yet implemented — listed so it is clear what is missing:

- Application execution (MiniJ VM) and the `mrc_` service bridge
- RetroPad and keyboard input mapped to the phone keypad
- Audio output
- Save states and reset
- Core options
- Guest memory exposure

## Current Limitations

- Applications do not execute; the core is a scaffold
- The core is experimental and its libretro surface may change
