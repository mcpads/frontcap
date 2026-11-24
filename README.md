# Frontcap

Use an MCP-compatible agent to inspect a web app, interact with its controls, and observe what changes as browser time advances. Give the agent a URL it can reach and describe the interaction you want to investigate.

> Open my local site, capture the initial screen, click the submit button once, and show me the resulting screen. Report whether the browser clock was controlled.

## Install from source

On macOS or Linux, install Rust with rustup and a system C linker. Run the following in a terminal. The checkout pins the Rust toolchain. Keep the checkout available after building so the server can find its browser manifest.

```sh
git clone https://github.com/mcpads/frontcap.git
cd frontcap
cargo build --locked --release -p frontcap-mcp -p frontcap-recorder
```

Register the server in your MCP client from this directory. For Codex, run:

```sh
codex mcp add frontcap -- "$PWD/target/release/frontcap-mcp"
```

For Claude Code, run:

```sh
claude mcp add frontcap -- "$PWD/target/release/frontcap-mcp"
```

Start a new client session and ask the agent to call Frontcap’s `bootstrap` tool. Follow its returned `next_action`. On first browser launch Frontcap uses a matching local patched engine if available, otherwise it installs the pinned headless Chromium build. The latter reports `engine_baseline` in the run’s taint flags. Check the clock status before drawing conclusions about timing. See the [engine build guide](docs/contributing/engine-build.md) for patched-engine setup.

## Try a browser interaction

From the repository root, serve the small counter used by the browser tests:

```sh
python3 -m http.server 8765 --bind 127.0.0.1 --directory crates/mcp/tests/fixtures
```

Ask your agent:

> Open http://127.0.0.1:8765/count.html in Frontcap. Read the counter, click the count button once, then read it again. Tell me both values and stop the session.

The page starts at 0 and should display 1 after the click. The second observation verifies that the app handled the input. Stop the local HTTP server with Ctrl-C.

## How agents use Frontcap

1. For a browser, call `bootstrap`, follow `next_action`, and use `launch` to start a generation (one browser run). Inspect `status` for the clock and any limits. For a configured Android emulator or iOS Simulator, start with `native` and `{"op":{"type":"describe"}}`. Native, Flutter and WebView support depends on the host profile and instrumentation.
2. Use `observe` to locate a control, `act` to send input, and `observe` or `capture` to verify the effect. Use `wait` for explicit progress when the selected runtime supports it. Front-panel tools describe their arguments with `{"describe":true}`.
3. Keep the original request ID if a reply is lost. Retrieve the original result before repeating a mutation. Finish browser runs with `stop` and native runs with the native stop operation. Inspect the retirement result.

See the [agent operating instructions](crates/mcp/src/server_instructions.txt) for branching, recovery, dialogs and evidence.

## Mobile sessions

Build the Android View worker and the iOS and WebView workers from the checkout.

```sh
cargo build --locked --release -p frontcap-adapter-native --bins \
  --example ios_session_worker --example android_webview_worker --example webview_session_worker
```

Place the host profile in `FRONTCAP_WORKSPACE` using the name `<runtime>-<platform>.json`. The supported runtime names are `native`, `host`, `flutter` and `webview`. Use `android` or `ios` for the platform. The profile identifies the session executable, device tools and application artifacts. Android View profiles must identify the package and activity explicitly.

Set `session_executable` to the absolute worker path. Android View uses `target/release/frontcap-native-session`. UIKit uses `target/release/examples/ios_session_worker`. Host and WebView profiles use `android_webview_worker` or `webview_session_worker` (iOS) from the same examples directory. Flutter launches the Python worker specified in its profile. See the [profile fields](crates/recorder/src/native/launcher.rs) and [platform types](crates/recorder/src/native/target.rs) for the required inputs.

For instrumented apps, see the [Android package](packages/frontcap_android/README.md), [UIKit package](packages/frontcap_uikit/README.md) and [Flutter package](packages/frontcap_flutter/README.md). Have the agent inspect the native operation schema and use the capabilities returned by the selected session.

## Configuration

- `FRONTCAP_WORKSPACE` selects the state and evidence directory. The default is `.frontcap` under the server’s working directory.
- `FRONTCAP_HOME` points to the checkout with `browsers.toml`. The default is the build checkout.
- `FRONTCAP_RECORDER` overrides the recorder executable. By default the server finds it beside its own executable.
- `FRONTCAP_EXTRA_ROOTS` supplies additional PEM certificate roots for test servers.
- `FRONTCAP_CELLS` selects the UID and GID for isolated browser processes on a configured privileged Linux host.

See the [documentation map](docs/README.md) for workspace layout, engine builds and contributor guidance.
