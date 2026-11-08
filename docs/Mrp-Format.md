# MRP Application Package Format (`.mrp`)

> **Status: preliminary reference.** This page is a community-compiled
> description of the `.mrp` container used by the mythroad platform, assembled
> from public documentation and inspection of real packages. The 200-byte
> header below is parsed by `mrpemu-core`; the file-list layout is **not yet
> verified** against enough samples and may be refined as research continues.

## Overview

An `.mrp` file is a package: a 200-byte header followed by the packaged file
area. The package carries the application's MiniJ bytecode, its resources, and
optionally `ext` native ARM libraries. All text in the package is GBK-encoded.

```
┌──────────────────────────┐
│ Header (200 bytes)       │
├──────────────────────────┤
│ File list (list_offset)  │
├──────────────────────────┤
│ File data (data_offset)  │
└──────────────────────────┘
```

## Header (200 bytes)

Integers marked LE are little-endian. The last two fields are big-endian.

| Offset | Size | Field | Description |
|-------:|-----:|-------|-------------|
| 0 | 4 | magic | Package signature identifying an `.mrp` file |
| 4 | 4 | data_offset | u32 LE — byte offset where the packaged file data begins |
| 8 | 4 | mrp_size | u32 LE — total package size in bytes |
| 12 | 4 | list_offset | u32 LE — byte offset of the package file list |
| 16 | 12 | name | Internal application name, GBK text |
| 28 | 24 | showName | Display name shown in the phone's application list, GBK text |
| 52 | 16 | imei | IMEI field used by the platform's device/licensing checks |
| 68 | 4 | version | u32 — application version |
| 72 | 4 | appid | u32 — platform-assigned application ID |
| 76 | 12 | reserved | Reserved |
| 88 | 40 | author | Author name, GBK text |
| 128 | 64 | desc | Application description, GBK text |
| 192 | 4 | ver | u32 **big-endian** — secondary version field |
| 196 | 4 | id | u32 **big-endian** — secondary ID field |

Notes:

- The string fields are fixed-size byte fields holding GBK text; the exact
  termination/padding convention still needs confirmation across samples.
- The mixed endianness is real: the integer fields up to `appid` are
  little-endian, while the trailing `ver`/`id` pair is big-endian. Parsers
  must not assume a single byte order for the header.

## File list

The file list starts at `list_offset` and describes the files packed into the
data area. Each entry is understood to contain the file's path, its offset in
the package, and its size.

> **To be verified.** The exact entry layout — field order, path encoding and
> length, entry-count encoding, and whether offsets are absolute or relative
> to `data_offset` — has not been confirmed against enough real packages.
> This section will be replaced with a byte-accurate table once verified.

Unverified sketch:

```
list_offset:
  [entry count]
  entry × count:
    path        (GBK)
    data offset (u32)
    data size   (u32)
```

## GBK text encoding

- Names, author, description and in-package paths use GBK, the dominant
  Chinese text encoding on MTK feature phones. GBK is ASCII-compatible for
  bytes below 0x80 and encodes Chinese characters in two bytes.
- Tooling must decode as GBK, not UTF-8; raw header bytes will not render
  correctly otherwise.
- `mrpemu-core` decodes the header's text fields with a GBK decoder when
  parsing a package.

## Verification status

| Area | Status |
| --- | --- |
| 200-byte header layout | Parsed by `mrpemu-core` (0.1.0) |
| `magic` value and validation rules | Known to the parser; not yet documented here |
| String padding/termination convention | To be verified |
| File-list entry layout | To be verified |

Found a package whose header or file list deviates from this description?
Open an issue describing the observed bytes — do not attach copyrighted
`.mrp` packages to public reports.
