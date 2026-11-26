# MrpEmu — A mythroad/MRP application emulator written in Rust

<p align="center">
  <a href="https://aloyshf.github.io/MrpEmu/"><img src="https://img.shields.io/badge/Website-MrpEmu-00B8A9?logo=githubpages&logoColor=white" alt="Website"></a>
  <a href="https://github.com/AloysHF/MrpEmu/actions/workflows/ci.yml"><img src="https://github.com/AloysHF/MrpEmu/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://git.libretro.com/libretro/mrpemu/-/pipelines"><img src="https://img.shields.io/badge/dynamic/json?url=https%3A%2F%2Fgit.libretro.com%2Fapi%2Fv4%2Fprojects%2Flibretro%252Fmrpemu%2Fpipelines%3Fref%3Dmaster%26per_page%3D1&query=%24%5B0%5D.status&label=Pipeline%20Status&logo=gitlab&cacheSeconds=300" alt="Gitlab Pipeline Status" ></a>
  <a href="https://github.com/AloysHF/MrpEmu/releases/latest"><img src="https://img.shields.io/github/v/release/AloysHF/MrpEmu" alt="Release"></a>
  <a href="https://github.com/AloysHF/MrpEmu/releases"><img src="https://img.shields.io/github/downloads/AloysHF/MrpEmu/total" alt="Downloads"></a>
  <a href="https://sonarcloud.io/dashboard?id=AloysHF_MrpEmu"><img src="https://sonarcloud.io/api/project_badges/measure?project=AloysHF_MrpEmu&metric=alert_status" alt="Quality Gate Status"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-GPL%203.0-blue.svg" alt="License: GPL 3.0"></a>
  <a href="https://discord.gg/E4SQjXUkZy"><img src="https://img.shields.io/badge/Discord-Join%20Us-5865F2?logo=discord&logoColor=white" alt="Discord"></a>
  <a href="https://qm.qq.com/q/LAO7DKAWUC"><img src="https://img.shields.io/badge/QQ%E7%BE%A4-Join%20Us-12B7F5?logo=tencent-qq&logoColor=white" alt="QQ Group"></a>
</p>

MrpEmu is a Rust emulator for the mythroad (MRP) application platform — the
download-and-run application ecosystem that Sky-mobi operated on MTK-based
feature phones. Applications ship as `.mrp` packages containing MiniJ VM
bytecode plus optional native (`ext`) ARM libraries, run on 240×320 RGB565
screens, and store their text in GBK. MrpEmu loads these packages and, through
a pure-Rust high-level emulation of the platform, aims to run them on the
desktop, in RetroArch, and in the browser.

MrpEmu uses the `mrpemu` command, crates and libretro core identifiers.

## Status

> **Experimental (version 0.1.0).** MrpEmu executes MiniJ bytecode and native
> ARM/Thumb extensions and presents game frames in the desktop frontend and
> the libretro core. Compatibility varies by application. See
> [Game Compatibility](docs/Game-Compatibility.md) for the verification criteria.

## Features

### Implemented

- **`.mrp` package parsing** — header metadata, the file list, and gzip
  members, with GBK text decoding
- **Application execution** — MiniJ interpretation, an ARM/Thumb interpreter,
  and high-level implementations of phone services
- **RGB565 framebuffer** — 240×320 by default, with 240×400 and 320×480
  selected from resolution labels in content paths. Both frontends advance
  the guest clock at 30 frames per second
- **Feature-phone keypad model** — direction keys, OK, the left/right soft
  keys, and the number keys
- **In-memory guest filesystem** — packaged and session-created files,
  including resources from guest-loaded RAM packages and optional device files
- **Standalone desktop frontend** — the `mrpemu` binary (minifb): loads an
  `.mrp` file and opens a scaled window
- **Libretro core** — `mrpemu_libretro` presents RGB565 game frames and maps
  RetroPad controls to the phone keypad
- **Desktop PCM audio** — plays supported WAV clips through the host audio device

### Planned (Roadmap)

- **Platform service coverage and game compatibility**
- **Additional audio formats and libretro audio output**
- **Save states**
- **Network services** — the platform's online features
- **EmulatorJS browser core** — the WebAssembly build of the libretro core

## Usage

### Standalone Mode

```bash
cargo run --release -p mrpemu -- path/to/app.mrp
```

Use `--system-dir path/to/device-files` to load optional Mythroad plugins
and fonts. For RetroArch, use its configured System/BIOS directory.

Headless runs and PNG captures do not open a window. The command below runs
200 deterministic frames and saves the presented frame:

```bash
mrpemu path/to/app.mrp --screenshot shot.png --screenshot-frames 200 \
  --press 30:lsk --press 65:ok
```

`--press` repeats `frame:key` pairs (`up`, `down`, `left`, `right`, `ok`,
`lsk`, `rsk`, `0`-`9`, `star`, `pound`). To capture a whole corpus, run
`scripts/batch-screenshots.ps1 -GameDirectory <dir>`; it builds the release
binary when `-Binary` is omitted and writes PNGs to `docs/images`.
Add `-Press 30:lsk,60:lsk,90:lsk` to apply one bring-up schedule to every
package; verified per-title overrides live in the script's `$recipes` table.

See the [Standalone Emulator](docs/Standalone-Emulator.md) guide for the
full option list and key mappings.

### RetroArch Mode

See the [RetroArch Core](docs/RetroArch-Core.md) guide for installation and
the current feature set.

### Browser Mode (EmulatorJS)

Planned: the same libretro core compiled to WebAssembly for
[EmulatorJS](https://emulatorjs.org). See
[EmulatorJS Core](docs/EmulatorJS-Core.md) for the target pipeline.

## Building

Requires [Rust](https://www.rust-lang.org/tools/install) (stable); the
workspace uses edition 2021. Use `cargo build --workspace` to build both
frontends and the shared core. A plain `cargo build` builds the libretro core
and its dependencies.

### Standalone Mode

```bash
cargo build -p mrpemu --release
cargo run -p mrpemu --release -- path/to/app.mrp
```

### Libretro Core (for RetroArch)

```bash
cargo build -p mrpemu-libretro --release
```

The binary is produced at `target/release/mrpemu.dll` (`libmrpemu.so` on
Linux, `libmrpemu.dylib` on macOS). Rename it to `mrpemu_libretro.<ext>`
before placing it in RetroArch's `cores/` directory.

For Android cross-compilation, see [Android Libretro Core](docs/Android-Libretro-Core.md).
For iOS, see [iOS Libretro Core](docs/iOS-Libretro-Core.md).

### EmulatorJS Core (Browser, planned)

The EmulatorJS bundles need an activated emsdk, `p7zip`, and a nightly
toolchain that can rebuild std:

```bash
cargo +nightly build -p mrpemu-libretro \
  --target wasm32-unknown-emscripten --profile wasm-release \
  -Zbuild-std=std,panic_abort

scripts/build-emulatorjs-core.sh
```

The result is `out/mrpemu-emulatorjs.zip`. See
[EmulatorJS Core](docs/EmulatorJS-Core.md) for requirements and host layout.

## Architecture

```
crates/
├── mrpemu-core/         # Platform-independent emulator engine (library)
├── mrpemu/              # Standalone desktop frontend, minifb (binary: mrpemu)
└── mrpemu-libretro/     # Libretro core (cdylib: mrpemu_libretro.{dll,so,dylib})
```

- **`mrpemu-core`** — package parsing, MiniJ and ARM/Thumb execution, the
  `mrc_` service bridge, the framebuffer, the keypad, and guest storage.
- **`mrpemu`** — the standalone desktop frontend: command line, window loop,
  keyboard input.
- **`mrpemu-libretro`** — content loading, video, and input for RetroArch
  and other libretro frontends.

See [Architecture](docs/architecture.md) for the platform background, the
layered design, and the current implementation status.

## Controls

Applications expect a feature-phone keypad. The guest keypad model covers the
direction keys, OK, the left/right soft keys and the number keys. The desktop
frontend currently binds:

| Phone input | Keyboard |
| --- | --- |
| Direction pad | Arrow keys |
| OK / confirm | Enter |
| Left soft key | Z |
| Right soft key | X |
| Digits | 0–9 |
| Exit | Escape |

See the [RetroArch Core](docs/RetroArch-Core.md) guide for the RetroPad mapping.

## Game Compatibility

See [Game Compatibility](docs/Game-Compatibility.md) for the verification
criteria and the captured 105-title hot-game table with screenshots.

## Documentation

- [Architecture](docs/architecture.md)
- [MRP Application Format](docs/Mrp-Format.md)
- [Standalone Emulator](docs/Standalone-Emulator.md)
- [RetroArch Core](docs/RetroArch-Core.md)
- [Android Libretro Core](docs/Android-Libretro-Core.md)
- [iOS Libretro Core](docs/iOS-Libretro-Core.md)
- [EmulatorJS Core](docs/EmulatorJS-Core.md)
- [Game Compatibility](docs/Game-Compatibility.md)
- [Contributing](docs/CONTRIBUTING.md)
- [Release Process](docs/Release-Process.md)

## Testing

Run the tests:

```bash
cargo test --workspace
```

Application packages are not included. Supply legally obtained `.mrp` files
separately.

## Contributing

Contributions are welcome! Whether you're interested in format research, the
MiniJ VM, the service bridge, documentation, or
compatibility testing, we'd love your help. See
[CONTRIBUTING.md](docs/CONTRIBUTING.md) for details.

## License

This project is licensed under the [GNU General Public License v3.0 or
later](LICENSE) (GPL-3.0-or-later).
