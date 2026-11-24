# File finalization interruption diagnostic

This opt-in Darwin interposer stops an owned process immediately before one exact `rename` pair.
It is qualification tooling, not a public storage API or a consumer dependency. The original app
must not link to it; load it only into an isolated diagnostic or staged development product.

Set `FC_RENAME_SOURCE`, `FC_RENAME_DESTINATION` and `FC_RENAME_MARKER` to absolute paths and load
`rename-stop.dylib` through `DYLD_INSERT_LIBRARIES` in a supported diagnostic launch. Nonmatching
renames proceed normally. A matching call exclusively creates and flushes a PID-bound marker,
then sends SIGSTOP to itself before calling the original rename. The external owner must verify
the exact PID/start identity, receipt and stopped process state, inspect both file paths, then
resume or kill and reap that process. Marker creation failure exits without renaming. Never use
this on an unowned process or without an external cleanup deadline.

The writer fixture flushes an ordinary pending file before rename. A killed stopped writer can
qualify interrupted finalization; it does not emulate partial writes, power loss or filesystem
durability after reboot. Simulator/device loading, engine symbol binding and actual consumer
recovery require their own installed evidence. `renameat`, direct syscalls and other storage APIs
are outside this probe.

Run `python3 spikes/ios-storage-boundary/probe.py /absolute/new/output` on a Darwin host with Xcode
command-line tools. The output must not exist. It compiles with strict warnings, then checks
nonmatching pass-through, resumed finalization and killed-before-rename persistence using separate
owned child processes. Every wait is bounded and every child is reaped, including on failure.

For an isolated prepared Simulator product, `spikes/ios-bundle-attachment/prepare.py` accepts
`--rename-boundary SOURCE DESTINATION MARKER`. All three paths must be distinct and relative to
the app's data-container HOME. The tool compiles the diagnostic into the existing platform loader;
it adds no extra consumer file or launch-environment override. The frozen preparation report
records the paths and diagnostic source hash. The constructor resolves those paths before normal
app work. This optional configuration remains diagnostic-only and must have an external owner
that can restart or retire a stopped app. Do not distribute this prepared product to users.
