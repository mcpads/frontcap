# iOS bundle attachment prerequisite

This opt-in experiment tests a platform-owned attachment in a **separate arm64 Simulator app copy**.
An ordinary `simctl openurl` launch does not use the `SIMCTL_CHILD_*` environment of the existing
`simctl launch` path. A staged main executable loads a platform bootstrap, which sets the existing
attachment's page/mute configuration and loads its native input library before application main.
No consumer source import, framework choice or testing callback is required.

Preparation can now feed the native host through explicit `app.prepared` admission.
This supplies the prepared product for declared ordinary-host adapter cold-entry support. It is not a
physical-device signing solution, arbitrary release-app qualification or a guarantee that hooks run
before every dependency initializer. The current bootstrap deliberately supports one declared
configuration: page registry enabled, the supplied timeline script and output muting enabled. It
does not capture or reproduce arbitrary launch environment variables.

## Preparation

Run on the target Mac with Xcode. Keep the environment and products outside the repository:

```sh
python3 -m venv /absolute/work/venv
/absolute/work/venv/bin/pip install --only-binary=:all: -r spikes/ios-bundle-attachment/requirements.txt
/absolute/work/venv/bin/python spikes/ios-bundle-attachment/prepare.py \
  /absolute/original/Consumer.app /absolute/native-input.dylib \
  /absolute/timeline.js /absolute/work/prepared
```

`prepare.py` refuses an existing destination, a non-Simulator product, linked input files and an
existing `FrontcapAttachment` directory. It retains original/staged file inventories and preparation
source hashes. It extracts the arm64 executable, appends the bootstrap dependency with
[LIEF's Mach-O API](https://lief.re/doc/stable/formats/macho/python.html), compiles `loader.c` for
iphonesimulator and ad-hoc signs the changed product. No source rebuild is performed.

The dependency is appended to preserve existing library ordinals. Preparation compares the original
symbol/library/addend/weak-import bindings after rewriting; a valid signature alone is insufficient.
Only the main executable and root signature may differ among existing consumer files. The original
product must still match its full input inventory. The staged product includes its own attachment
and script, so subsequent URL starts do not depend on launch-time injection environment variables.
Generate its typed native-host app configuration without rebuilding or resigning:

```sh
/absolute/work/venv/bin/python spikes/ios-bundle-attachment/manifest.py \
  /absolute/work/prepared/preparation.json /absolute/work/prepared/app-launch.json
```

Use that JSON object as `target.config.app` in `host-ios.json`. It supplies the original/prepared
inventories and Mach-O shared-library list through `prepared`. The adapter checks original and staged
bytes, verifies the actual installed bundle, and launches without a second environment injection.
A prepared product supplied without this admission is rejected. Current scope requires empty consumer
launch environment, page registry and output muting; other bootstrap configurations remain unsupported.
