# Application Compatibility

## Current status

**MrpEmu has no verified playable applications yet.** The emulator is in
scaffold stage: package headers parse and the frontends present a framebuffer,
but the MiniJ VM and the `mrc_` service bridge are not implemented, so no
application executes. This page will list tested applications once execution
lands; until then the table below is the template contributors should fill.

## How applications are verified

Compatibility rows are only added after reproducible runs:

- A run loads a legally obtained `.mrp` package in the standalone frontend or
  the RetroArch core and follows a described input sequence.
- A row records what was actually observed (screens reached, input that
  worked), not intentions. A loading screen or a correct-looking window alone
  does not establish compatibility.
- "Playable" requires entering and operating core gameplay in a scripted run;
  complete playthroughs and audio are tracked separately in the notes.

Status levels used in the table:

| Status | Meaning |
| --- | --- |
| Playable | Core gameplay entered and operated in scripted runs (not necessarily completed) |
| Partial | Interactive screens verified; gameplay unverified or only partly verified |
| Boots only | Renders startup screens; interaction not verified |
| Blocked (network) | The only entry path is the platform's retired online service |
| Fails to start | Guest execution stops during startup |

## Application List

Applications are not included in this repository; supply legally obtained
`.mrp` packages separately. *Version* is the version from the package header;
*Screen* is the observed resolution.

| Application | Version | Screen | Status | Notes |
| --- | --- | --- | --- | --- |

*(empty — no application has been verified yet)*

## Reporting a Compatibility Issue

Once applications execute, include the package file name, the header version
and app ID, the screen resolution, the last visible screen, the triggering
input and the log output. Do not attach copyrighted `.mrp` packages to public
reports.
