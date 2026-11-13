# Security policy

Author: 𝓐.𝓒.𝓑

## Reporting a vulnerability

Please do not open a public issue for a security problem. Report it privately through GitHub instead: [open a private vulnerability report](https://github.com/IACBI/tinvo/security/advisories/new). Only the maintainer sees it, and the discussion, the fix and the advisory stay in one place.

A useful report says which version you ran (`tinvo --version`), on which system, what you did, and what happened. If a file triggers it, attach the file.

The fix ships in a new release, with an advisory that credits you unless you would rather not be named.

## Supported versions

Only the latest release gets fixes. TINVO is before 1.0, so a fix is not backported to older versions.

## What counts

TINVO reads input it did not write: `.tvo` project files, WAV files, the settings file, MIDI bytes and terminal input. Anything one of these can do beyond being refused with an error is a security problem: a crash, a hang, unbounded memory use, reading or writing files it should not, or running code. The project and WAV readers and the edit model are fuzzed in CI (`tinvo fuzz`), and a reproducing input is the most helpful thing a report can carry.

TINVO has no network code. The only program it starts is `valgrind`, and only for `tinvo bench --instructions`. It calls operating-system audio, MIDI and terminal APIs through its own FFI; a memory-safety bug in that code counts too.
