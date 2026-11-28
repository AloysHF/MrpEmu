# RetroArch Core

This guide covers installing and running the MrpEmu libretro core for
RetroArch, loading content, and the current feature set.

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

The core looks for optional device files in RetroArch's **System/BIOS**
directory and then beside the content. Use the same filenames and directory
layout as the [standalone emulator](Standalone-Emulator.md#optional-device-files).

## Current Features

- `.mrp` content loading
- RGB565 video at 240×320 by default. A file or parent folder labeled
  `240x400` or `320x480` selects that size. Labels can also use `×`.
- Content reset with the original screen geometry
- RetroPad input mapped to the phone keypad

| Phone input | RetroPad |
| --- | --- |
| Direction keys | D-pad |
| OK | A |
| Left soft key | B |
| Right soft key | X |
| Digit 0 | Y |
| Star | Select |
| Pound | Start |

## Planned Features

- Save states
- Core options
- Guest memory exposure
