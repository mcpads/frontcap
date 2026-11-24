# Platform-owned Android speech observation

This opt-in ART agent observes framework TTS boundaries in an existing debuggable app.
It attaches to the running APK.
The observer uses method breakpoints and reads local variables.
Breakpoints may deoptimize code and add scheduling overhead; this is not transparent observation
or controlled speech time.

Build on the configured macOS Android host:

```sh
bash spikes/android-speech-observer/build.sh /new/build/directory
```

Pass the resulting `libspeech_observer.so` to the ordinary
[speech diagnostic](../android-speech-host/README.md) with `--agent-library`.
The runner creates the app-private files directory, copies the library and dispatches
`am attach-agent` once. It records the selected PID before dispatch and checks it afterward.
It does not retry an uncertain attachment. Native cleanup runs even if log collection fails.

The agent receives the app's files directory as its option and exclusively creates
`speech-observer.jsonl`. Use fresh app data; an old receipt is not overwritten. It scans framework
classes and listens for later class preparation, binding executable methods of `TextToSpeech`
and its connection classes. `speak/stop` and `onStart/onSuccess/onError/onStop` are candidates.
Utterance-ID slots are discovered from local-variable metadata at the actual method entry;
no consumer field offsets or fixed local slots are assumed.

The selected standard utterance-ID strings are mapped to process-local ordinal identifiers.
Spoken text and the original ID strings are not written to evidence. At most 64 IDs and 256
boundary events are retained; zero means no usable ID and the last retained event marks the
event limit. `stop` has no ID argument, so its absent-local-information result is expected.
The default observation build replaces no argument, return value or callback.

Setup `ready` alone is not proof of observation coverage. Inspect method bindings, actual
correlated calls, errors, truncation and the unchanged host outcome.

## Lifecycle-only flow diagnostic

Build with `bash spikes/android-speech-observer/build.sh /new/build/directory flow`
and pass the library with `--require-flow` to the speech diagnostic. Set `JAVA_HOME`
when the installed JDK is not registered with macOS `java_home`.
This passive mode binds only request, stop, start, success, error and cancellation
boundaries. It neither binds PCM callbacks nor copies audio buffers, so PCM traffic
cannot exhaust its 256 lifecycle-event budget. Consumer code remains unchanged.

Each token records an observed request count and correlation label: `unattributed`,
`single-observed-request`, or `reused-token`. Tokens are process-local consumer-ID
ordinals, not unique requests or synthesizer identities. A reused token stays ambiguous;
the diagnostic does not join an earlier start to a later request's completion.
Qualification requires an ordered, error-free request/start/success triple for a
single observed request, plus the runner's existing loss and ownership checks.
This is occurrence evidence, not audible-output or waveform interpretation.

Flow builds also request optional method-exit observation for the standard `speak` call.
`ready.submission_observed` advertises availability; unsupported runtimes retain the existing
callback observer. Exit events are enabled only on a thread with an observed call and disabled
when its last tracked call exits. Nested calls are matched by method and stack depth. One bounded
`submission` record names the original request sequence and token, the unmodified integer return
or exception, or an observer error. It never replaces the native return or synthesizes a callback.
Per-call sequence correlation does not make later callbacks using a reused token unambiguous.

The typed reader exposes `submissions` and `pending_submissions` independently of playback events. An accepted submission does not prove a start; a missing exit remains pending when observation is
available. Older producers explicitly have unavailable submission coverage. The byte bound remains
256 KiB, with at most 256 submission records in addition to the 256 lifecycle records. The [submission fixture](submission-fixture/README.md) compares accepted and rejected calls with the framework return values.

The flow build also accepts fenced attachment options:
`/absolute/app-private/directory|PID|START_TICKS|ATTACHMENT_UUID`.
The producer checks its own PID and `/proc/self/stat` birth ticks before creating
the evidence file or installing breakpoints. Successful attach evidence includes
`attachment` and `start_ticks`; a replacement process cannot accept the original
birth identity. The directory must already exist. Only the flow build accepts this
form; the legacy directory-only diagnostic remains available and is explicitly unfenced.
Attachment is one-shot even when validation fails; retire an uncertain/failed target
rather than retrying it. The controller must still check ownership around dispatch
and read, bind the unique directory and attachment ID, and persist the original outcome.
Build/helper checks and selected installed fenced attachment qualification pass.

New fenced flow builds also advertise `sampling`. The observer remembers up to eight TTS
receivers weakly and reports their IDs on speak and stop boundaries. Stop does not create an
utterance identity; it can expose a receiver even when the consumer rejects speech before submission. A platform JVMTI agent thread
queries the standard framework `isSpeaking` implementation on demand; it does not override
consumer callbacks or run the query under the breakpoint mutex. Collected references and
query failures are explicit unknown states, not `speaking=false`.

The host atomically renames `state-request` containing `INDEX|REQUEST_HASH` plus a newline;
the hash is 32 lowercase hexadecimal characters. Increasing positive 64-bit indices admit
one sample. Equal or older indices never measure new state. The latest result is atomically
published as `state-response.json`, with attachment/process identity, request index/hash,
clock/sequence windows, faults, reference capacity and receiver states. Admission uses constant
memory rather than an ever-growing request set. Original host receipts provide historical
recovery; the producer's single response file is not a historical journal. Earlier lifecycle evidence remains scoped to its original producer builds.

## Limits before control

This is a process-lifetime diagnostic, not a production attachment protocol. There is no hot
unload/retry. Failed or uncertain setup requires target retirement. PID checks are observational;
the directory-only diagnostic does not enforce a process-birth fence during dispatch.
The optional fenced flow mode adds producer-side birth checks; host command integration has
focused and installed recovery checks. Multiple synthesizers,
reused consumer IDs, absent debug metadata, class-preparation
interleavings and concurrent teardown need separate handling before general integration.
Neither framework method visibility nor native completion qualifies PCM preparation, exact
advancement, completion admission, cancellation or replay.

## PCM diagnostic

The agent also binds `onBeginSynthesis` and `onAudioAvailable`. Their format integers and byte-array
argument slots come from installed method metadata. It records sample rate, channel count, native
format identifier, chunk byte count and nonzero-byte count. A transient copy is limited to 1 MiB
per chunk; larger chunks are explicitly marked omitted. Original audio bytes are not written.

Add `--require-pcm` to the host diagnostic with `--agent-library` to require one completed
utterance's signed 16-bit PCM declaration, aligned chunks, nonzero data and ordering before success. The resulting report includes counts and a duration calculated from sample count/rate. Chunk events share the existing
256-event bound; exhaustion fails the diagnostic instead of proving a partial stream complete.

## Separate file-preparation experiment

Build with a second argument `file-worker` to also produce `speech-worker.jar` and enable
the platform worker. Pass both `--agent-library` and `--worker-jar` to the host diagnostic,
normally with `--require-pcm`. The runner makes the DEX archive read-only before attachment.
No consumer source or APK changes are needed; the selected app must permit debugger attachment.

On the first ordinary `speak`, the agent reads the framework receiver, immutable String text,
current engine and parameters through JVMTI/JNI, then calls the injected Java helper without
holding its observer mutex. Framework fields are identified through JVMTI metadata; Android's
hidden-API policy and the app's SDK settings are not changed. Unsupported arguments or field
access fail this experiment with a named dispatch stage.

The helper creates another TTS client with the same offline voice and engine and requests
`synthesizeToFile`. It does not play that file. On completion it checks the actual RIFF/WAV
format and data size against the received PCM byte count, records the frame count and deletes
the audio file before publishing success. Only numeric/voice metadata is collected. A timeout
or late callback invalidates preparation. An unrelated original app speech request still follows
its ordinary playback path; this option does not suppress that output or control its time.

The worker's random identity separates its callbacks from ordinary observation. In this mode
the framework observer stops recording after the first ordinary utterance succeeds; the helper
maintains its own receipt. One worker is admitted per process. Reused IDs, multiple consumers,
ownership changes, cancellations and sample-cursor integration remain outside this experiment.

## Hold before ordinary playback

The separate `hold-worker` build requests JVMTI `can_force_early_return`. After dispatching
the first real file-preparation request, it calls `ForceEarlyReturnInt` on the intercepted
ordinary `speak` frame with `TextToSpeech.SUCCESS`. The method body therefore does not submit
ordinary playback when early return succeeds. The agent records the actual operation status.
Other requests and methods are not controlled by this experiment.

Run the host diagnostic with `--hold-speech`, `--agent-library` and `--worker-jar`.
Do not combine this with `--require-pcm`: that option checks ordinary playback PCM, which
this experiment must prevent. The hold diagnostic instead requires the worker's validated
WAV/frame receipt, no ordinary synthesis callbacks, no host completion and at least six
seconds of repeated observations after preparation. It retains the observations and then
retires the target; it does not resume the request or inject completion.

This establishes request interception separately from the next sample-cursor/callback work.
It is not a full TTS replacement: `isSpeaking`, subsequent requests, stop/flush, ownership,
preparation failures and recovery still need coordinated behavior. Returning success admits
work; it does not claim the work has completed. A preparation or interception failure fails
the diagnostic and requires retirement.

## External sample control

Build with `control-worker` and run the host diagnostic with
`--hold-speech --control-speech --agent-library <library> --worker-jar <jar>`.
This links the shared `speech_progress.c` cursor, registers the platform helper's JNI methods
and binds the original framework listener through debugger metadata. No consumer library is
required. Preparation initializes the cursor from verified WAV frames; the first positive
advance delivers start, and exact completion delivers done through the existing listener.

The app-private `speech-control.json` publishes cursor state and a random token.
Publish `speech-request.json` atomically with `token`, positive integer `request`, `op`
(`read` or `advance`) and integer `delta_ns`. Read the matching `speech-response.json`.
The same request ID and operation/delta return its original response; conflicting reuse is
rejected. At most 128 identities are retained for this process. This diagnostic transport
does not supply crash recovery or the product's durable request journal.

Crossing the next completion boundary is rejected without mutation. Reads do not advance. Weak source/listener references and current-listener checks guard delivery; installed replacement
and reentrancy still need qualification. The mode controls one utterance only. Subsequent
speak calls on its source are rejected, and calls during active progress fail the cursor. Main-thread stop on the selected source now cancels progress and delivers one ordinary `onStop`;
later advancement cannot complete the cancelled utterance. Off-main-thread stop is rejected. It does not yet implement normal queues/flush, `isSpeaking` or page-clock coordination.

Add `--cancel-speech-at X Y` to qualify partial-progress cancellation through the app's cancellation control. Derive the point from a native screenshot; the diagnostic checks current focus,
retains another screenshot and dispatches one native tap. To target preparation instead, also pass `--cancel-during-preparation`. The runner taps while
the real worker reports preparation; it adds no synthesis delay and never retries a missed window. The control receipt's `stop_phase` records the actual phase at cancellation, and
`discarded_preparations` records successful preparation results ignored after cancellation. The diagnostic requires a preparing-phase stop, no start/done, one unstarted stop callback,
and unchanged cancellation after actual synthesis completion and later advances/replay. For this short window, optionally pass `--native-input-jar <touch-input.jar>` built with
`spikes/android-native-input/build.sh`. The runner starts that existing shell-UID driver before
launching the app, then sends one down/up pair after the same screenshot/focus checks. It verifies
generation, request and contact revision on every reply and closes the owned helper on retirement. This removes per-tap process startup; it does not pause or delay synthesis. Do not infer coverage from the flag. Duplicate stops and owner/reentrancy cases still need qualification.

## Following utterance experiment

The separate `sequential-worker` build admits at most two serial requests on the same source
and listener. Run it with `--hold-speech --control-speech --speech-utterances 2` and the usual
agent/JAR arguments. The second request must come from the real app after the first terminates;
the controller does not manufacture another speak call. This is not general queue/flush support.

Main-thread preparation returns explicit admission success. A rejected preparation returns
speech ERROR rather than allowing the original playback path to run. Following admission requires
a finished/cancelled cursor, a closed prior worker and unchanged owner/listener, and is rejected
during callback delivery. A distinct token derived from the attachment nonce and zero cursor bind the new synthesis.
Each worker retains its own `speech-worker-<token>.json` receipt; `speech-worker.json`
is the current worker's receipt. Private WAV files are still deleted before preparation success.

Request identities remain process-wide across both utterances. Cached replay is checked against
its original token/operation/delta before current-token admission, so an old completed request
can return its original result. A new operation with the old token is rejected and cannot advance
the new cursor. Ordinary overlapping requests, a third utterance, cross-owner handoff and general
reentrant continuation remain outside this experiment.
