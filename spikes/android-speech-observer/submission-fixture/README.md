# Installed Android speech submission diagnostic

This standalone debuggable fixture calls the real Android `TextToSpeech` framework. It records
the direct integer returns independently of the external flow observer: an ordinary muted
request, a request exceeding the framework input limit, and a request after shutdown. All use
the same consumer token to expose the difference between exact call/return correlation and
ambiguous later callbacks. It neither requires nor adds a testing SDK to consumer apps.

On the prepared Android test host, build in a fresh directory with the existing SDK/JDK:

```sh
bash spikes/android-speech-observer/submission-fixture/build.sh /absolute/new-product
python3 spikes/android-speech-observer/submission-fixture/run.py \
  --apk /absolute/new-product/speech-submission.apk \
  --library /absolute/flow/libspeech_observer.so \
  --avd-home /absolute/frozen-avd-home --avd Frontcap_Result_Persistence_API_35 \
  --tools-directory /absolute/checkout/tools --output /absolute/new-run
```

The runner uses the shared native lock/fence and `OwnedEmulator`, copies the original AVD into
its own output, checks source hashes, retains raw observer/oracle records and retires its target
in `finally`. A retired copied AVD is removed; evidence and original failures remain. Output
is muted. No voice download is performed. Engine initialization failure or unsupported return
observation is a failed qualification, not proof of silence or failure callback coverage.

For read-only typed validation of the retained records:

```sh
FRONTCAP_ANDROID_SPEECH_FIXTURE=/absolute/run/speech-observer.jsonl \
FRONTCAP_ANDROID_SPEECH_ORACLE=/absolute/run/submission-oracle.jsonl \
FRONTCAP_ANDROID_SPEECH_PID=ACTUAL_RECORDED_PID \
cargo test -p frontcap-core android_speech_matches_installed_framework_returns --lib -- --ignored
```

The results cover the framework calls exercised by this fixture.
