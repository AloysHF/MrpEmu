# EmulatorJS Core (Browser)

MrpEmu targets the browser as a standard [EmulatorJS](https://emulatorjs.org)
core. The build follows EmulatorJS's own pipeline: the Rust libretro core is
compiled for `wasm32-unknown-emscripten`, linked against the
`EmulatorJS/RetroArch` fork, and packaged as the `<core>-wasm.data` bundles
that an EmulatorJS page loads at runtime.

## Project player

`site/play.html` is a complete example page. It sets `EJS_core = "mrpemu"`,
loads a user-selected `.mrp` through a blob URL and uses `data/` as
`EJS_pathtodata`. The published player lives at
<https://AloysHF.github.io/MrpEmu/play.html>.

`.github/workflows/pages.yml` deploys `site/` at the repository root and
fetches the pinned EmulatorJS runtime plus the `mrpemu-emulatorjs.zip` core
from the rolling `nightly-build` release. It refuses to deploy when either
WASM bundle is missing, so `play.html` never ships without its core.

## What you get

Each nightly publishes a single asset:

| File | Contents |
|------|----------|
| `mrpemu-emulatorjs.zip` | Both core variants, the build report `cores/reports/mrpemu.json`, `core.json`, `build.json`, `license.txt` and a README |

The two `.data` files stay separate inside the ZIP: EmulatorJS requests
`mrpemu-wasm.data` or `mrpemu-legacy-wasm.data` by name depending on WebGL2
support, so one bundle cannot serve both cases.

The bundle is laid out like this:

```
mrpemu-emulatorjs.zip
├── cores/
│   ├── mrpemu-wasm.data
│   ├── mrpemu-legacy-wasm.data
│   └── reports/mrpemu.json
├── core.json
├── build.json
├── license.txt
└── README.md
```

Each `.data` file is a 7z archive containing `mrpemu_libretro.js`,
`mrpemu_libretro.wasm`, `core.json`, `build.json` and `license.txt`.

## Install into a site

EmulatorJS always fetches cores from `<EJS_pathtodata>cores/`, so host the
runtime yourself (or use the pinned CDN files) and drop the core beside it:

```
data/
├── loader.js
├── emulator.min.js
├── emulator.min.css
├── compression/
│   ├── extract7z.js       # extracts the core .data archive
│   └── extractzip.js      # extracts ZIP content
└── cores/
    ├── mrpemu-wasm.data
    ├── mrpemu-legacy-wasm.data
    └── reports/mrpemu.json
```

`emulator.min.zip` only contains `emulator.min.js` and `emulator.min.css`;
the `compression/` files are loaded on demand from the same EmulatorJS data
directory. Without `extract7z.js` the core download ends in a "Network Error".

To upgrade the runtime, replace the runtime files with a newer EmulatorJS
release (≥ 4.2.3) and keep this `cores/` directory.

## Use

```html
<div id="game"></div>
<script>
  EJS_player = "#game";
  EJS_core = "mrpemu";
  EJS_gameUrl = URL.createObjectURL(gameFile);
  EJS_gameName = gameFile.name;
  EJS_pathtodata = "data/";
  EJS_startOnLoaded = false;
</script>
<script src="data/loader.js"></script>
```

When the application comes from a blob URL, EmulatorJS writes it into the wasm
filesystem under `EJS_gameName`. Set that to the original file name, extension
included (for example `app.mrp`), so the frontend keeps a recognizable content
path for the core.

## Rendering

The core renders its framebuffer through libretro's software path in the
RGB565 pixel format at the platform's 240×320 size. The play stage scales the
output to fit the page; pixel-art applications look best with nearest-neighbor
scaling.

## Controls

EmulatorJS maps its RetroPad to the core's phone-keypad layout (once input
support lands):

| Input | Guest key |
|-------|-----------|
| Arrow keys / WASD | D-pad |
| Enter | OK / confirm |
| Z / X | Left / right soft keys |

Touch controls and physical gamepads are provided by EmulatorJS and arrive
through the libretro pointer and RetroPad devices.

## Saves

Applications have no battery-backed save RAM in this core, and save states are
not implemented yet. This section will be updated when state support lands.

## Building the bundle

The nightly workflow runs this automatically. To build locally you need an
activated [emsdk](https://emscripten.org/docs/getting_started/downloads.html)
(`emmake` in `PATH`), `p7zip`, `zip`, and the pinned nightly toolchain:

```bash
# Rust's prebuilt emscripten stdlib carries wasm exception handling, which
# Binaryen's Asyncify pass rejects. Rebuild std with panic=abort instead.
cargo +nightly build -p mrpemu-libretro \
  --target wasm32-unknown-emscripten --profile wasm-release \
  -Zbuild-std=std,panic_abort

scripts/build-emulatorjs-core.sh
```

The script clones the pinned `EmulatorJS/RetroArch` revision, links the
staticlib with `Makefile.emulatorjs`, builds the WebGL2 and legacy variants,
and writes `out/mrpemu-emulatorjs.zip`. `--work-dir` moves the RetroArch
checkout out of `target/`, which matters on CI because `rust-cache` restores
`target/` without the checkout's `.git`.

The Pages deployment fetches the pinned EmulatorJS runtime, downloads the
`mrpemu-emulatorjs.zip` asset from the rolling `nightly-build` release, and
redeploys after every successful Nightly Build.
