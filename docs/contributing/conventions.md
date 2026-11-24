# Repository and Code Conventions

## Language and types

- Product code is Rust.
- External protocol messages are never handled as hand-written `serde_json::Value`. Chrome DevTools
  Protocol types are generated at build time from `crates/cdp/protocol/protocol.json`, which
  `browser-pin dump-protocol` writes from the patched engine when one is built (so it includes the patch
  set's `frontcap*` commands); `protocol.meta.json` names the browser and patch set it came from.
- Do not use Playwright action APIs. Their actionability checks wait on `requestAnimationFrame` and
  wall-clock timeouts, so they stall while the virtual clock is paused.
- When target-specific code requires another language ecosystem, it runs as a separate bridge process
  and the boundary is validated against a strongly typed schema.

## Versions

Internal interfaces evolve in place while frontcap is unreleased. Do not add or increment protocol
or schema versions to count implementation progress; Git records that history. The MCP controller
and recorder are built together, and their internal hello reports the launch identity without a
protocol version negotiation. Supporting independently released clients would require an explicit
compatibility policy at that time. Versions required by external protocols and package tooling, and
dependency and engine pins, remain meaningful and are retained.

## Workspace layout

Every directory under `crates/` is a workspace member.

```
crates/
  core/              package frontcap-core (lib frontcap_core): identifiers, virtual time and the clock
                     state machine, external event agenda, request journal, predicates, network rules
                     and cassettes, trace event types and projections, bundle format, saved-basis
                     manifests, atomic state files, process identity, and typed native request,
                     observation and receipt data. Does not depend on the CDP client.
  cdp-codegen/       CDP code generation from the pinned protocol JSON; a build dependency of `cdp`
  cdp/               strongly typed CDP client; its protocol types are generated at build time by
                     `cdp-codegen`
  browser-pin/       the pinned browser: download, verification, launch arguments, the patched engine
                     build (`browser-pin engine ...`), Linux cells and process images, and the sandbox
                     allowance for hosts that restrict user namespaces
  adapter-chromium/  package frontcap-adapter-chromium (lib frontcap_chromium): one Chromium generation;
                     observer world `frontcap`, binding `__frontcapReport`
  adapter-native/    package frontcap-adapter-native (lib frontcap_native): experimental native request
                     admission, durable bindings and supervised bridge exchanges, Android/iOS
                     acquisition, long-lived sessions, sequential observations and observed-tap
                     readiness, Flutter and WebView targets, resource groups and storage volumes;
                     connected to the experimental native recorder; the private session binary
                     `frontcap-native-session`
  service-fixture/   package frontcap-service-fixture, binary `frontcap-service-fixture`: controlled
                     local HTTP responses with durable release identities; owned by optional native
                     recorder launches
  proxy/             package frontcap-proxy: HTTP, SSE, and WebSocket proxy with its TLS authority and
                     message buffering
  recorder-protocol/ package frontcap-recorder-protocol: strongly typed messages between control servers
                     and the recorder, including the lease token
  recorder/          package frontcap-recorder, binary `frontcap-recorder`; hosts the adapter, the
                     proxy, the clock driver and agenda, the trace writer, the request journal, leases
                     and control state; also saved bases, branch construction and replay, and the
                     experimental native recorder (launch, preparation, result recovery and session
                     storage)
  mcp/               package frontcap-mcp, binary `frontcap-mcp`: control MCP server over stdio; a
                     stateless recorder client
engine/              the patch set applied to the pinned engine, its build arguments, container build
                     context and test-only overlays
spikes/              prerequisite verification programs, kept for re-verification; not workspace members
tools/               opt-in engine probes (`<name>_probe.py` with `<name>_probe.rs`), the MCP relay,
                     the Linux local host runner (`linux-host/`), and the
                     native and Flutter tooling (`native_*`, `android_*`, `ios_*`, `mobile_*`,
                     `flutter_*`)
packages/            test-build instrumentation for native and Flutter consumer apps (`frontcap_android`,
                     `frontcap_uikit`, `frontcap_flutter`, each with its README); not workspace members
```

Integration-test fixtures (static pages and the like) live with the crate whose tests use them, under
`crates/<crate>/tests/fixtures/`.

The Core package is `frontcap-core` because a package named `core` would shadow Rust's `core` crate.

Not built: an adapter SPI crate (the capability traits Core requires of a target, and capability
advertisement types); the recorder dispatches directly to its Chromium backend or experimental native
session backend. A `frontcap` CLI for bundle inspection and video encoding is also not built.

### Crate dependencies

- `core`, `cdp-codegen` and `browser-pin` depend on no other workspace crate. `core` never depends on
  `adapter-chromium` or `cdp`.
- `cdp → cdp-codegen` as a build dependency.
- `proxy → core`, `service-fixture → core`, `recorder-protocol → core`.
- `adapter-native → core, cdp`.
- `adapter-chromium → core, cdp, browser-pin, proxy` (the platform's own stream connections).
- `recorder → core, adapter-chromium, adapter-native, proxy, recorder-protocol, service-fixture`.
- `mcp → core, recorder-protocol, recorder`.

`mcp` never uses the recorder's engine side and never talks to the engine directly. From the recorder
it uses only:

- the launcher: launch registry, process identity, spawning and relaunching recorders, and read-only
  saved-result lookup;
- runtime-record reading;
- saved-basis storage and branch construction, which drive a child through the same recorder protocol;
- process-image directory lookup and bundle replay;
- the trace-derived console, request and watch-history projections it answers for stopped generations;
- the native recorder's launcher (host profiles, launch registration and spawning), launch
  specification, launch-preparation control, and read-only session, observation and storage lookups.

The files a run leaves on disk and the form of identifiers are described in
[Workspace directory and identifiers](../reference/workspace-directory.md). Building the engine is
described in [Engine build](engine-build.md).

## Tests

### What to run

- Run the checks for the changed behavior and the regressions it could plausibly cause. The complete
  workspace suite is for integration milestones; a small edit alone does not justify it.
- Before an expensive run, state the changed behavior, the affected consumers and the gap the run will
  close. Build the combined changes once, run focused checks, then reuse one healthy session for
  compatible scenarios.
- Repeat a passing check only after a relevant change or failure. Report focused results separately
  from the last complete run.
- A failing integration run leads to targeted investigation and verification of the affected cases, not
  repeated full runs until one is green.
- Reuse an earlier result only when the implementation, inputs and runtime assumptions still match, and
  name the source it was run on.

### Commands

- The complete run is `cargo test --workspace --all-features --no-fail-fast`, one suite at a time: the
  suites are sensitive to host load.
- The lint gate is `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- A recorder-only run needs `--features fault-injection`.
- The Python tool suite needs both discovery patterns:
  `python3 -m unittest discover -s tools -p '*_tests.py'` and
  `python3 -m unittest discover -s tools -p 'test_*.py'`.
- `FRONTCAP_KEEP_WORKSPACE` keeps a recorder or MCP test's workspace (an MCP test names it on standard
  error) and `FRONTCAP_RECORDER_STDERR` shows the recorder's standard error.

### Rules

- Unit tests live in sibling `*_tests.rs` files.
- The clock state machine, agenda splitting and same-instant ordering, rule matching order, cassette key
  normalization, and control-state persistence are tested without a browser. The clock state machine and
  agenda splitting also use property-based tests (proptest).
- Integration tests run against the real engine (the patched build when it exists, otherwise the pinned
  `chrome-headless-shell`) and pages under the crate's `tests/fixtures/`.
- Determinism tests compare trace projections: `behavior` across runs with the same commands,
  regardless of acquisition, and `replay` between a recorded run and its replay. Not built: `behavior`
  across host classes, and the `acquisition` and `strict` projections.
- Recovery tests kill the control server at every step of a request, and kill the recorder at each
  request-journal step: before dispatch, after the target applied the effect but before the result was
  recorded, and after recording but before the response was delivered. Every case must resolve to a
  definite outcome.
- A test that needs a host capability (container runtime, ffmpeg) skips with a printed reason when the
  capability is missing. A capability whose required tests were skipped stays unverified; a skipped run
  never counts as a pass.
- Do not assert on incidental shape such as generated line counts or file sizes.
- Wall-time criteria depend on the host's power settings. On a Linux host used for them, set the CPU
  energy preference to `performance` and disable every CPU idle state whose exit latency exceeds
  100 µs, on every CPU. Each frame of the clock loop waits on many sequential cross-process wakeups,
  and with deep idle states an otherwise idle host runs the loop several times slower.

### Fault injection

Recovery tests abort processes at named points. The oracles exist only in builds with the
`fault-injection` feature; set their variables only in the test child's environment. They inject
process exits and errors. They do not simulate a physical filesystem or power loss.

Recorder (`frontcap-recorder` with `fault-injection`):

- `FRONTCAP_FAULT=<request_id>@<point>` aborts the recorder at a named point, which the target observes
  exactly as a killed process.
- `FRONTCAP_STATE_FAULT=<filename>@after_partial_write|after_sync|after_rename` aborts the subprocess
  writing a state file; `no_space_after_partial_write` returns `ENOSPC` after writing half the bytes.
- `FRONTCAP_EXCEPTION_FAULT`, `FRONTCAP_INTERCEPTION_FAULT` and `FRONTCAP_STREAM_FAULT` take a
  `<point>` named where each is defined in the adapter.
- `FRONTCAP_INTERCEPTION_DELAY=<request URL part>@<ms>` hands matching `Fetch.requestPaused` events
  over that much later.
- `FRONTCAP_ARRIVAL_REPORT_DELAY=<ms>` makes pre-renderer recovery treat the engine's main-thread
  arrival report as absent for that long after each synchronous fulfillment. A renderer still reading
  the response shows the same thing.

The two delay variables model a slower host without aborting anything.

Native adapter (its own `fault-injection` feature):

- `FRONTCAP_ANDROID_WEBVIEW_FAULT=<request_id>@before_result`, in the Android WebView and Android host
  workers, returns a publication error after the operation ran and before the caller result is saved,
  allowing normal owned retirement. `@after_home` stops a WebView Home/return after a verified Home,
  before Activate.
- `FRONTCAP_IOS_HOST_FAULT=<request_id>@before_result` fails after successful ordinary iOS host
  execution and before its outer result; `@before_entry_ready` fails after replacement producers are
  acquired and before cold-entry readiness is published. These typed errors pass through both host and
  worker envelopes without saving a substitute failed operation, and owned retirement still runs.
- `FRONTCAP_IOS_HOST_LOSE_WORKER=<request>@<stage>` sends SIGKILL to the current worker at an iOS host
  publication point.
- `FRONTCAP_ANDROID_HOST_RESTART_BEFORE_PROOF=<request_id>` returns an error before the durable proof
  of an Android host replacement, with normal owned cleanup.
- `FRONTCAP_ANDROID_HOST_RESTART_LOSE_WORKER_BEFORE_PROOF=<request_id>` records the reached boundary
  and sends SIGKILL to the current operation worker.

The `before_result` and `before_entry_ready` points are publication-error boundaries, not abrupt worker
death. Where a worker is killed, proof of the original effect and independent cleanup of its resource
group are checked separately; a killed worker cannot publish its own completion.

## Development outputs

Keep what reproduces a result: committed code, the command, and the inputs and environment it needs.

- Per-run source copies, compiled binaries, build trees, disk images and routine screenshots are
  temporary. Reuse one build for compatible checks, and discard reproducible outputs once the result is
  summarized and the processes and volumes that own them have retired.
- Keep raw diagnostics only for an unresolved failure that still needs them, an observation that is
  hard to reproduce, or inputs that cannot be reconstructed. Keep the smallest useful subset.
- Check ownership and mounted-volume boundaries before deleting.

This covers development and test outputs, not exports a user asked for or the product's live recovery
records.

## Spikes and probes

Each spike under `spikes/<name>/` keeps its program and fixture so it can be re-run when its source,
engine or host runtime changes. Its README states what the spike checks, the command, the toolchain it
needs and the limits of its conclusion. A conclusion that changes a guarantee is applied to the
document that owns the guarantee.

An engine probe is an opt-in adapter test kept outside the suite: `python3 tools/<name>_probe.py`
copies `tools/<name>_probe.rs` to `crates/adapter-chromium/tests/frontcap_<name>_probe.rs` (refusing to
overwrite), runs that test, writes its log to `target/browser-finish-checks/<name>-probe.log`
(underscores as hyphens) and removes the copy. Remove a probe when nothing current depends on it; Git
keeps the program.

Native experiments use their target toolchains and their host-side tools outside the product Cargo
workspace.
