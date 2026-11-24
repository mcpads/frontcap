# IME frame-change control

This diagnostic input method requests a 126 px input view, then requests 883 px after 500 ms on
its ordinary main Handler. It records both requests and real uptime to `files/events.jsonl`.
It intentionally has no typing UI. Use only on an owned disposable emulator: selecting an IME
changes the device's input method. The standard runner does not install or enable this fixture.

Build with the installed Android 35 SDK and Java tools:

```sh
bash spikes/s18-native-time/android/ime-frame/build.sh .frontcap/ime-frame-build
adb -s emulator-5580 install --no-incremental .frontcap/ime-frame-build/frame-ime.apk
adb -s emulator-5580 shell ime list -a -s
# Continue only after dev.frontcap.ime/.FrameIme appears in the service list.
adb -s emulator-5580 shell ime enable dev.frontcap.ime/.FrameIme
adb -s emulator-5580 shell ime set dev.frontcap.ime/.FrameIme
```

The validated comparison retains app inset 126 versus system height 883 across host waits at
virtual zero. A deliberate advance just past the observed queued IME deadline then delivers 883. That intervention is diagnostic, not a fix: automatic advancement would change consumer semantics. A fixed thirty-second advance also exposed a subsequent read timeout and pending traversal barrier;
do not replace the measured deadline with a large arbitrary jump or classify that Run as passing.

The original natural failure remains open until the runtime boundary is repaired and the relevant
consumer regression passes. This fixture reproduces a mechanism; it does not establish the earlier
third-party keyboard's exact internal event sequence.
