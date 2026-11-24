# Ordinary Android speech host

`SpeechHost.kt` exposes Android TextToSpeech to a Flutter activity.

Retain `dev.frontcap.speech.SpeechHost(context, messenger, mutedOutput)` from the Flutter
activity's `configureFlutterEngine` and call `close()` from `cleanUpFlutterEngine`. Include
`android.intent.action.TTS_SERVICE` in a manifest `<queries><intent>` entry. Use the Dart method channel `dev.frontcap/speech`.

The service waits for native initialization with a bounded pending-call queue and deadline.
`speak` accepts text/language, selects an installed voice that does not require networking,
and invokes native `TextToSpeech.speak`. It excludes voices marked as needing downloaded data,
rather than silently substituting a voice or sending text to a network service.
Native progress callbacks are marshalled onto the main thread and carry per-instance utterance
identities. Only the current utterance's callbacks are forwarded. `stop` uses native stop;
`pause` and `resume` explicitly return unsupported because this native API has no such operation.
The ordinary service does not claim deterministic callback timing or crash/restart continuity.

`mutedOutput=true` submits volume zero while retaining synthesis/playback/completion.
This setting is an output policy, not a measured-silence claim or virtual-time control.
The bounded `files/native-speech.jsonl` records metadata and callbacks, not spoken text.
Closing unregisters the channel, fails pending calls and shuts down the native engine.

These choices follow the Android references for
[TextToSpeech](https://developer.android.com/reference/android/speech/tts/TextToSpeech),
[progress callbacks](https://developer.android.com/reference/android/speech/tts/UtteranceProgressListener),
[voice network requirements](https://developer.android.com/reference/android/speech/tts/Voice), and
[engine parameters](https://developer.android.com/reference/android/speech/tts/TextToSpeech.Engine).

## Installed diagnostic

`observe.py` reuses the shared native lock/fence and owned-emulator retirement. Pass the APK, activity and emulator configuration. If the app needs a local HTTP service, specify its listening port with `--service-port PORT`.

```sh
python3 spikes/android-speech-host/observe.py \
  --apk /absolute/path/app-debug.apk \
  --component com.example.consumer/.MainActivity \
  --avd Medium_Phone_API_35 --output /new/output/directory
```

Trigger speech in the app. The diagnostic correlates native start and completion callbacks by utterance ID.

For an environment with preinstalled voice data, pass both `--data-image /path/userdata.img`
and `--encryption-key /path/encryptionkey.img`. Both files must exist and belong to the same
prepared environment and compatible AVD/runtime. The shared runner keeps read-only execution;
it does not download voice data, select an arbitrary engine or mutate the original AVD. Preserve image hashes and recheck them after execution. Android may create QCOW sidecars next
to the inputs, so inspect those separately rather than inferring immutable state from raw-file
hashes alone.

To observe framework boundaries while preserving this ordinary service, optionally pass
`--agent-library /path/libspeech_observer.so`. The
[separate platform agent](../android-speech-observer/README.md) is attached externally to the
same APK. Its checks are additional to ordinary synthesis/feedback checks; neither set implies
controlled Android speech.

The agent also has explicit `file-worker`, `hold-worker`, `control-worker` and `sequential-worker` build modes. Pass `--worker-jar` for platform file synthesis, add `--hold-speech` for held-request
qualification, and add `--control-speech` for external sample advancement and ordinary listener
delivery. These modes use different acceptance checks; ordinary completion is not expected
during the hold. The [agent README](../android-speech-observer/README.md) owns their commands
and limits.

Use `sequential-worker` with `--speech-utterances 2` in addition to the control arguments
to qualify two actual app requests. This mode admits at most two
serial requests on the same owner/listener; it does not implement general queues or flush.
