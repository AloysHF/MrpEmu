# Contributing

## How to Contribute

1. **Fork** this repository
2. **Create** a feature branch (`git checkout -b feature/your-feature`)
3. **Commit** your changes (`git commit -m 'Add your feature'`)
4. **Push** to the branch (`git push origin feature/your-feature`)
5. **Open** a Pull Request

## Code Style

- Run `cargo fmt --all` and keep your diff formatting-clean
- `cargo clippy --workspace --all-targets` must pass without warnings
- Use English for all comments and documentation
- Use `snake_case` for functions and variables, `PascalCase` for types and
  structs
- Log through the `log` facade — no `println!`/`eprintln!` diagnostics in
  library code
- Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)

## Layering Rules

- `mrpemu-core` stays platform-independent: it must not depend on a window
  toolkit (minifb), on libretro, or on any host GUI. Frontends talk to the
  core through its framebuffer and input interfaces.
- No `unsafe` outside the libretro ABI boundary. The only permitted `unsafe`
  lives in `mrpemu-libretro`'s FFI exports, and each `unsafe` block needs a
  `// Safety:` comment explaining its invariants.

## Clean-room Policy

MrpEmu is a clean-room reimplementation of the mythroad platform:

- Do not copy proprietary code — no decompiled mythroad runtime or SDK
  sources, no disassembly listings of `.mrp` packages or platform libraries.
- Implement from public documentation and from black-box observation of
  packages you legally own.
- Do not attach copyrighted `.mrp` packages or SDK binaries to issues or pull
  requests; describe the behavior and quote the relevant hex bytes instead.

## Areas That Need Help

- **`.mrp` format research** — verify the file-list layout and the header
  string conventions against real packages (see
  [MRP Format](Mrp-Format.md))
- **MiniJ VM** — the bytecode interpreter is the central open work item
- **`mrc_` service bridge** — implement and document platform services
- **ext/ARM strategy** — prototype options for native library execution
- **Compatibility testing** — begins once applications execute; see
  [Game Compatibility](Game-Compatibility.md)
- **Documentation** — improve docs and code comments
- **Bug reports** — open an issue with the package's header metadata (name,
  version, app ID) and a description of the problem

## Getting Started

Check the [open issues](https://github.com/AloysHF/MrpEmu/issues) for tasks
labeled `good first issue` or `help wanted`. If you have questions, feel free
to open a discussion issue.

To understand the package format, see [MRP Format](Mrp-Format.md); for how
the emulator is put together, see [Architecture](architecture.md).

## Testing

Run the tests:

```bash
cargo test --workspace
```

New parsing or VM behavior should come with unit tests.
