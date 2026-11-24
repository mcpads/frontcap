# Engine Build

## Pinned browser

- The browser is a pinned Chrome for Testing `chrome-headless-shell`. Its version and the protocol JSON
  are pinned together.
- Downloaded browsers and built engines live under the repository's `browsers/` and are not committed.
  The container build contexts are `engine/linux/` and `tools/linux-host/`.
- A generation runs the patched build of the current patch set when it exists, and the Chrome for
  Testing baseline otherwise, with `engine_baseline` taint naming the patches it lacks. The baseline
  does not provide the patched engine's guarantees. The bundle's `engine_revision` is
  `<revision>+frontcap.<patchset>` for the patched engine, kept in the bundle's `engine.json` at launch.
- MCP launch supports only level `time` on the local host. `bootstrap` advertises `frame: false` and
  `container: false`; either requested capability is rejected before launch.

## Patched engine

frontcap's patched engine is the pinned tag with the patch set in `engine/`:
`engine/patches/frontcap.patch` and `engine/args.gn`. The patch set's identity is a digest of the
pinned version, `args.gn` and the patches (`browser-pin engine id`), so a new pin needs a new build.

`browser-pin engine build --src <chromium/src>` builds it from a clean checkout at the pinned tag. It
needs depot_tools on `PATH`; the checkout lives outside the repository and `--src` is required. The
result is kept under `browsers/patched/<patchset>/<platform>/` with a `manifest.json` written last, and
the checkout is reset afterwards (`git reset --hard`; untracked files are never touched). The kept
files are the target's `runtime_deps`.

- Every generation launch checks each file it uses against the manifest's digest, so the development
  profile builds `sha2` optimized (`Cargo.toml`). A process computes a file's digest once for each
  length and modification time it has, also when callers ask at once or reach the file through a link.
- A build killed with the patch set applied leaves `out/frontcap/.frontcap-applied`, and the next build
  resets those changes as its own; any other change refuses the build.
- On macOS the build needs Xcode's Metal Toolchain (`xcodebuild -downloadComponent MetalToolchain`) for
  ANGLE's shaders.

### V8 changes

V8 changes have their own optional input directory, `engine/v8/`: `base-revision` names the full
upstream commit and `.patch` files apply in filename order to the independent `src/v8` checkout. The
commit, patch names and contents enter the engine identity. Without that directory, existing engine
identities are unchanged.

Before applying or recovering patches, the builder locks and checks both repositories; a foreign edit
in either refuses the operation. Each checkout has its own ownership marker and restores only its
recorded changes, including after a failed patch. Even when V8 patches have been removed, a dirty V8
checkout is refused unless its changes belong to an interrupted build. Input-time records include V8
files against the shared Chromium Ninja log.

The active V8 patch reports synchronous atomic wait and wake transitions to the scheduler and uses its
virtual timeout clock.

### Test overlays

`engine/tests/` holds test-only engine overlays and their runner, described in
[its README](../../engine/tests/README.md). They are not patch inputs and do not enter the engine
identity.

## Other platforms and Linux hosts

`--platform <platform>` builds another platform's engine from a checkout synced for it. The build adds
that platform's `target_os` and `target_cpu` to `args.gn`, and nothing for the host's own platform, so
the patch set's identity does not depend on the host.

Chromium publishes its toolchain for x64 Linux hosts only. `sh engine/linux/build.sh
[linux-arm64|linux64]` runs the build in an amd64 container (Rosetta on Apple silicon) with its own
checkout in the Docker volume `frontcap-chromium-linux`, synced at the pinned tag on first use, and
installs the result under `browsers/patched/` like a host build, owned by the user who ran it.
`FRONTCAP_BUILD_JOBS` caps the build's parallel compiles. The Linux script sets six for a 16 GB Docker
VM, where twelve under Rosetta ran out of memory; twelve fit a 29 GB VM.

`sh tools/linux-host/run.sh <command>` runs a command, such as a test suite, on a Linux local host in
Docker as an unprivileged user (the engine's sandbox refuses root). `tools/linux-host/mcp.sh` serves
`frontcap-mcp` over stdio from that host as root with browsers in cells (`FRONTCAP_CELLS`), for
`tools/mcp-relay.py` (`FRONTCAP_MCP`).

### Hosts that restrict user namespaces

A Linux host that keeps unprivileged processes from using user namespaces (Ubuntu 23.10 and later,
`kernel.apparmor_restrict_unprivileged_userns=1`) refuses the engine's sandbox, and a launch fails
naming the fix: run `sudo target/debug/browser-pin engine allow-sandbox` once per host and engine build.

It copies the current patched engine, verified against its manifest, to
`/opt/frontcap/engines/<patchset>/<platform>/`, and the installed pinned browser to
`/opt/frontcap/engines/base-<version>/<platform>/`. It checks that only root can change those paths and
installs `/etc/apparmor.d/frontcap-engine`, which lets the browsers there use user namespaces and
confines nothing else, as Ubuntu's own Chrome and Chromium profiles do. On such a host each resolves to
its copy while the copy matches; the Docker runner mounts them at the same path. `--print` shows the
profile without installing anything. On other hosts the command reports that nothing is needed.

## Rebuild cost

The build tool decides by modification time, and the build resets the checkout and patches it again,
which gives unchanged patched files new times. A normally exited build, including a compile failure,
records each changed file's content digest and time with the build log's state
(`out/frontcap/.frontcap-tree.json`); the next build gives a re-applied file whose content was compiled
its recorded time back, so only real changes rebuild.

Any build since the record voids it, because its objects may be of other content, and a record is used
once. GN generation and a tool that fails to start also preserve reusable input times when a previous
Ninja log exists. Each step consumes its predecessor's record before it starts, so a signaled compiler
cannot leave GN's earlier record behind. Failed builds remain failed and do not install a manifest.
Ninja still rebuilds unfinished outputs after input times are restored.

An official build right after the previous one, or after an in-tree build recorded with
`browser-pin engine record-tree --src <chromium/src>`, then takes about a minute instead of about
twenty.

## Iterating on a patch

1. Apply every patch to the checkout with `git apply --index`, as the official build does, so files a
   patch adds are tracked and recorded too.
2. Set every changed file's time before the last build (`git diff --name-only HEAD | xargs touch -t
   <old time>`); the build then has nothing to do.
3. Edit, and run `autoninja -C out/frontcap headless_shell`. A changed `.mojom` or protocol `.pdl`
   rebuilds widely.
4. When it passes, regenerate the maintained diff against the pinned upstream source in a scratch
   repository. Never revert a live checkout to calculate it.
5. Prove that applying the regenerated diff produces the intended file contents and modes.
6. Record the owned compiled tree with `browser-pin engine record-tree`, reset only that owned
   checkout, and use the official build.

Do not change active patch inputs while builds or test runs reference them: their identity changes and
tests stop finding their build.

## Tests and the engine

Tests of a patch's guarantee fail without the patched engine unless `FRONTCAP_ALLOW_BASELINE=1`, which
makes them check the baseline's `engine_baseline` taint instead.

MCP integration tests keep their recorder build separate from fault-injection builds under the
effective Cargo target directory's `mcp-tests/<os>-<architecture>/`. The helper honors
`CARGO_TARGET_DIR`. Host-specific subdirectories are required when macOS and Linux tests run against
the same mounted repository: their executables must not overwrite each other while launch, recovery or
replay is starting. `FRONTCAP_TEST_RECORDER=<absolute path>` makes the MCP tests use an explicitly
built recorder instead.
