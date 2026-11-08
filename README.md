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

> **Early scaffold (version 0.1.0).** Package header parsing, the RGB565
> framebuffer, the keypad input model, the in-memory filesystem skeleton, the
> desktop frontend and the libretro core shell work. Guest applications do
> **not** execute yet: the MiniJ VM, the `mrc_` service bridge, ext (native
> ARM library) execution, audio, save states and networking are not
> implemented. No application has been verified playable — see
> [Game Compatibility](docs/Game-Compatibility.md).

## Features

### Implemented

- **`.mrp` package parsing** — the 200-byte package header: magic, offsets and
  sizes, internal name, display name, IMEI, version, app ID, author and
  description fields, with GBK text decoding
- **RGB565 framebuffer** — 240×320 16-bit (5-6-5) display surface owned by the
  core
- **Feature-phone keypad model** — direction keys, OK, the left/right soft
  keys, and the number keys
- **In-memory VFS skeleton** — sandboxed guest filesystem foundation for
  packaged and guest-created files
- **Standalone desktop frontend** — the `mrpemu` binary (minifb): loads an
  `.mrp` file and opens a scaled window
- **Libretro core skeleton** — `mrpemu_libretro` loads content and reports
  240×320 RGB565 video to the frontend

### Planned (Roadmap)

- **MiniJ VM interpreter** — execute the bytecode inside `.mrp` packages
- **`mrc_` API bridge** — the phone services applications call: display,
  input, files, timers, and the rest of the platform surface
- **ext (native ARM library) execution** — an open strategy decision; see
  [Architecture](docs/architecture.md)
- **Audio output**
- **Save states**
- **Network services** — the platform's online features
- **EmulatorJS browser core** — the WebAssembly build of the libretro core

## Usage

### Standalone Mode

```bash
cargo run --release -p mrpemu -- path/to/app.mrp
```

See the [Standalone Emulator](docs/Standalone-Emulator.md) guide for the
command-line options and key mappings.

### RetroArch Mode

The libretro core can be built and loaded today; it reports video but cannot
run applications yet.

See the [RetroArch Core](docs/RetroArch-Core.md) guide for installation and
the current feature set.

### Browser Mode (EmulatorJS)

Planned: the same libretro core compiled to WebAssembly for
[EmulatorJS](https://emulatorjs.org). See
[EmulatorJS Core](docs/EmulatorJS-Core.md) for the target pipeline.

## Building

Requires [Rust](https://www.rust-lang.org/tools/install) (stable); the
workspace uses edition 2021. The default workspace members are `mrpemu-core`,
`mrpemu` and `mrpemu-libretro`, so a plain `cargo build` at the repository root
builds all three.

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

- **`mrpemu-core`** — package parsing, the RGB565 framebuffer, the keypad
  input model and the in-memory VFS; the MiniJ interpreter and the `mrc_`
  service bridge will live here too. The core depends on no windowing system
  and no frontend.
- **`mrpemu`** — the standalone desktop frontend: command line, window loop,
  keyboard input.
- **`mrpemu-libretro`** — the libretro core shell: content loading and video
  reporting for RetroArch and other libretro frontends.

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
| Exit | Escape |

RetroArch input mapping is not implemented yet; see the
[RetroArch Core](docs/RetroArch-Core.md) guide for the current state.

## Game Compatibility

MrpEmu cannot execute applications yet, so no application has been verified.
See [Game Compatibility](docs/Game-Compatibility.md) for the verification
criteria and the compatibility table template.

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
MiniJ VM, the service bridge, documentation, or (once applications run)
compatibility testing, we'd love your help. See
[CONTRIBUTING.md](docs/CONTRIBUTING.md) for details.

## License

This project is licensed under the [GNU General Public License v3.0 or
later](LICENSE) (GPL-3.0-or-later).
