# Application Compatibility

## Current status

MrpEmu can execute MRP applications. The headless video test drives startup
and key input at 30 guest frames per second. The video check requires
non-uniform RGB565 pixels across at least one eighth of the display rows
and at least 0.5% of its pixels. Thin strips and uniform frames are reported
separately. Playability requires a reproducible gameplay sequence.

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
| Blocked (network) | The tested entry path requires a network service unavailable in the emulator |
| Fails to start | Guest execution stops during startup |

## Application List

Applications are not included in this repository; supply legally obtained
`.mrp` packages separately. *Version* is the version from the package header;
*Screen* is the observed resolution.

| Application | Version | Screen | Status | Notes |
| --- | --- | --- | --- | --- |

Compatibility rows are recorded after the frontend and gameplay checks above.

## Reporting a Compatibility Issue

Include the package file name, the header version
and app ID, the screen resolution, the last visible screen, the triggering
input and the log output. Do not attach copyrighted `.mrp` packages to public
reports.
