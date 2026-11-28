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

## Controls

For touchscreen applications, tap and drag on the game image with one finger
or the left mouse button. The core reads the frontend's pointer alongside
RetroPad input.

| Phone input | RetroPad |
| --- | --- |
| Direction keys | D-pad |
| OK | A |
| Left soft key | B |
| Right soft key | X |
| Digit 0 | Y |
| Star | Select |
| Pound | Start |

## Core Options

Quick Menu → Core Options exposes **Audio Volume** (100 down to 0 in ten
steps). The value applies immediately — no reset needed — and scales every
decoded format (MIDI, WAV, MP3, AMR). The application's own volume command
(`platEx` 1302) writes into the same control.

## Save States and Save Data

The core supports libretro save states (`retro_serialize` /
`retro_unserialize`): a snapshot is bound to the loaded content, so a
state saved for one application refuses to restore into another. Guest
files — game save data and settings — persist in the frontend's save
directory and reload with the content on the next run.

## Planned Features

- Core options
- Guest memory exposure
