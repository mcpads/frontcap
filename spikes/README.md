# Verification programs

The browser programs form a separate Rust workspace. From this directory, run `cargo run -p <package>` with a package listed in [Cargo.toml](Cargo.toml). Reports are written under `results/`.

Platform-specific programs include their own run scripts. Android probes require the Android SDK and NDK. Simulator probes require Xcode on macOS.
