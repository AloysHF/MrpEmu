# Release Process

This document describes how to publish a new release of MrpEmu.

## Prerequisites

- Push access to the `master` branch
- Permission to create tags and releases on GitHub

## Steps

### 1. Update version numbers

Version numbers must be updated in **two files**:

| File | Field | Current |
|------|-------|---------|
| `Cargo.toml` (workspace root) | `[workspace.package] version` | `"0.1.0"` |
| `crates/mrpemu-libretro/mrpemu_libretro.info` | `display_version` | `"0.1.0"` |

Both values must match. The `.info` file is copied directly into the
release artifacts — RetroArch reads `display_version` to display the core
version to users. (If the core metadata file does not exist yet, create it
when adding core metadata so the two stay in sync.)

Run `cargo check --workspace` after editing the versions to keep the local
`Cargo.lock` consistent with the workspace package versions.

```bash
# Example: bumping to 0.2.0
# 1. Edit Cargo.toml
sed -i 's/^version = "0.1.0"/version = "0.2.0"/' Cargo.toml

# 2. Edit .info file
sed -i 's/^display_version = "0.1.0"/display_version = "0.2.0"/' \
  crates/mrpemu-libretro/mrpemu_libretro.info
```

### 2. Commit the version bump

```bash
git add Cargo.toml crates/mrpemu-libretro/mrpemu_libretro.info docs/Release-Process.md
git commit -m "chore: bump version to 0.2.0"
git push origin master
```

### 3. Create and push a tag

The release workflow triggers on tags matching `v*` (e.g. `v0.2.0`).

```bash
git tag v0.2.0
git push origin v0.2.0
```

### 4. CI builds and creates a draft release

Pushing the tag triggers the release workflow, which:

1. **Builds standalone binaries** for Windows, macOS and Linux
2. **Builds libretro cores** for the same platforms plus mobile targets, using
   the platform-specific core filenames (`mrpemu_libretro`,
   `mrpemu_libretro_android`, `mrpemu_libretro_ios`) and bundling
   `mrpemu_libretro.info`
3. **Builds the EmulatorJS browser core** (`mrpemu-emulatorjs.zip`) with the
   pinned Emscripten toolchain
4. **Creates a draft GitHub Release** with:
   - Auto-generated release notes (commits since the previous tag)
   - A download table for standalone and libretro artifacts
   - All build artifacts attached

### 5. Review and publish the release

1. Go to [Releases](https://github.com/AloysHF/MrpEmu/releases)
2. Find the draft release created by CI
3. Review the auto-generated changelog — edit if needed (you can refer to the
   changelog of the previous official release)
4. Verify the expected artifacts are attached: standalone archives per
   platform, per-platform `mrpemu-*-libretro` archives, and
   `mrpemu-emulatorjs.zip`
5. Click **Publish release**

### 6. Sync upstream libretro metadata and builds

Submit core metadata changes to
[libretro-super](https://github.com/libretro/libretro-super/tree/master/dist/info)
under `dist/info/mrpemu_libretro.info`, using the current
`crates/mrpemu-libretro/mrpemu_libretro.info` from this repository.
The separate `libretro-core-info` repository is a mirror. The libretro
buildbot mirror for this project lives at
<https://git.libretro.com/libretro/mrpemu>. Updating this repository alone
does not publish a Core Downloader entry.

## Nightly builds

Every successful CI run on `master` also refreshes the rolling
**nightly-build** release: the same artifacts are published under the
`nightly-build` tag, so users can track progress between versioned releases.
The [project site](https://aloyshf.github.io/MrpEmu/) deployment downloads the
`mrpemu-emulatorjs.zip` asset from this rolling release after every
successful Nightly Build.

## Troubleshooting

### CI build fails

- Check the [Actions](https://github.com/AloysHF/MrpEmu/actions) tab for the
  failed run
- The most common failure is a missing Linux build dependency for the desktop
  frontend's windowing libraries — the CI installs those packages
  automatically

### Re-triggering a release

The release workflow only runs on tag pushes. To re-trigger:

```bash
# 1. Delete the tag locally and remotely
git tag -d v0.2.0
git push origin --delete v0.2.0

# 2. Re-push the tag (CI will re-run)
git push origin v0.2.0
```

If a draft release was already created by the failed run, delete it from the
[Releases](https://github.com/AloysHF/MrpEmu/releases) page before
re-pushing the tag, otherwise the new run may conflict with the existing draft.

### Release artifacts missing

- Verify the tag name starts with `v` (e.g. `v0.2.0`, not `0.2.0`)
- The release workflow only triggers on tag pushes (`v*`); manually dispatching
  the workflow from the Actions tab will not create a release

### Version mismatch in RetroArch

- Ensure both files have the same version string:
  `Cargo.toml` and `mrpemu_libretro.info`
- The `.info` file is bundled as-is into the release artifacts
