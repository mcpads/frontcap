# Ordinary iOS speech host

Use this Flutter host service to speak text on an iOS Simulator.

`SpeechHost.swift` uses AVSpeechSynthesizer and the Flutter method channel `dev.frontcap/speech`.
The host must retain it and pass its engine registrar's messenger. Methods are `speak` (text and
language), `stop`, `pause` and `resume`. Events describe native delegate callbacks for the current
utterance; superseded utterances are logged without forwarding feedback. The bounded local JSONL
log contains utterance identifiers and character counts, not spoken text.

`observe.py` is a controller for `spikes/android-webview-clock/run_ios.py`. It reuses the external
WebView clock probe, then samples that clock and native speech records for 30 host-time intervals.
Use the ordinary runner arguments with the service-enabled app, injected WebView timeline library
and script, and `--controller spikes/ios-speech-host/observe.py`. It does not issue speech-control
commands. The containing frozen checkout must include the existing WebView spike and native tools.

No ordinary-host callback or
method-channel acknowledgement proves audio output or deterministic speech progression.

`failure_probe.py --output /absolute/new-directory` is a separate opt-in framework diagnostic. It builds `failure-fixture.m`, injects the existing passive observer into a fresh owned iOS
Simulator, and compares a real duplicate-utterance exception against the app's independent
oracle.
