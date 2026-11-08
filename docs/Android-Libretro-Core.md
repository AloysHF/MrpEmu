# Android Libretro Core

The MrpEmu libretro core can be built for Android, so it can be used by
Android RetroArch-based frontends.

> **Status: experimental.** The core is a scaffold and cannot execute `.mrp`
> applications yet; see [RetroArch Core](RetroArch-Core.md).

## Install in RetroArch on Android

### Manual installation

Availability in RetroArch's Core Downloader is not assumed; install a core you
built or obtained from the
[Releases](https://github.com/AloysHF/MrpEmu/releases) page (when available)
manually.

1. **Obtain** the `mrpemu_libretro_android.so` matching your device's ABI
   (most modern devices are `arm64-v8a`).
2. **Install the core**: copy it into RetroArch's `cores/` directory
   (typically `/storage/emulated/0/RetroArch/cores/` or the app's internal
   `cores/` path), and copy `mrpemu_libretro.info` into RetroArch's `info/`
   directory when the repository ships it.
3. **Load** the core and content the same way as on desktop.

## Building the Android core locally

Building for Android requires the [Android NDK](https://developer.android.com/ndk)
and [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk):

```bash
cargo install cargo-ndk
rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
export ANDROID_NDK_HOME=/path/to/android-ndk
# Android 15 devices require 16 KB page alignment for native libraries.
export RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384"

# Build all four ABIs (artifacts land in target/<triple>/release/)
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86 -t x86_64 --platform 21 \
  build -p mrpemu-libretro --release
```

Each ABI produces `libmrpemu.so`; rename it to
`mrpemu_libretro_android.so` when installing into RetroArch on Android.

## Notes for Android hosts

- Applications in this core have no battery-backed save RAM, and save states
  are not implemented yet.
- Load failures are reported through the core log.
- Cores should be linked with 16 KB page alignment for Android 15 devices (see
  the build command above).
