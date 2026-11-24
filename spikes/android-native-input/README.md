# External Android contact input

This platform-owned input producer runs a small DEX program as the ADB shell user, outside the
consumer process. It requires no consumer library, instrumentation runner, manifest edit or APK
rebuild. The direct invocation below is a
diagnostic interface and does not supply those adapter guarantees on its own.

The implementation uses Android's `MotionEvent` pointer IDs/action indices and the hidden
`InputManagerGlobal.injectInputEvent` path also used by the
[AOSP input shell command](https://android.googlesource.com/platform/frameworks/base/+/8417c7bb1cbb/services/core/java/com/android/server/input/InputShellCommand.java).
See the [MotionEvent reference](https://developer.android.com/reference/android/view/MotionEvent)
for contact action semantics. Hidden API availability is runtime-specific.

## Build and run

Use JDK 17, SDK platform 35 and build tools 35.0.0:

```sh
bash spikes/android-native-input/build.sh /absolute/fresh-build-directory
```

Under the existing native lock/fence, acquire an owned emulator through `OwnedEmulator`.
Install/launch the consumer normally, then push `touch-input.jar` to an owned `/data/local/tmp/`
path. Run an interactive subprocess using the exact owned serial and JAR path:

```sh
adb -s OWNED_SERIAL shell -T \
  'CLASSPATH=OWNED_JAR_PATH app_process /system/bin dev.frontcap.input.TouchInput'
```

The first stdout JSON line contains the process ID, UID 2000 and a fresh generation.
Send newline-delimited requests through that same process's stdin:

```json
{"generation":"RETURNED_GENERATION","request_id":1,"expected_revision":0,"contacts":[{"id":1,"x":300,"y":400}]}
```

Coordinates are absolute display pixels, not normalized app-window or CSS coordinates.
The caller must bind the consumer process, check current foreground ownership and derive points
from the current screenshot. The driver itself has no consumer identity or overlay hit-test guard.
The installed diagnostic caller uses the full `dumpsys window` output for its focus check.

## State and evidence

Supply the complete desired contact set. Add an ID to press, change its coordinates to move,
omit it to release, or send an empty set to release all contacts. IDs 1–5 are unique.
Absent contacts release before surviving contacts move and new contacts are added. Each native
event carries the then-current pointer array and correct action index. A gesture retains its
initial `downTime`; event time uses Android uptime, not controlled page time.

Replace `contacts` with `cancel: true` to cancel the whole held set, or `finish: true` to finish
only when no contacts remain. Each accepted request increments `contact_revision`. Requests
require the current generation, sequential request ID and expected revision; rejections consume
the request ID but do not advance the contact revision. Limits default to 24 requests
(explicit capacity up to 4096), five contacts,
8192 characters per request, and finite coordinates in 0–16384.

Ready advertises `short_tap: true`. Replacing `contacts` with
`"tap":{"x":300,"y":400}` sends one DOWN/UP pair in the same producer request, requiring an empty
held set. The UP event must be created within 100 ms of the gesture start; timestamps are measured,
not synthesized. Native injection itself may take longer. Failure after DOWN dispatch remains
`unknown`, ends the producer and does not automatically replay UP or cancel. Two accepted receipts
and an empty final set prove native dispatch only; inspect the consumer separately.

Replies retain requested native action values, pointer arrays, timestamps and injection results.
`injected` means native dispatch returned success in wait-for-finish mode; it does not prove game
admission, rendered effects or exact page delivery. Inspect the independent page pointer observer,
native screenshots and consumer result. Empty updates/cancellation emit no events.

Failure after dispatch starts is `unknown` and ends the driver. A timeout, lost reply or EOF does
not authorize replay. The caller must bound reply waits and process lifetime and retire its owned
target. The driver does not auto-cancel on exit: cancellation may commit consumer work.
On failure the returned contact list is only driver bookkeeping, not an observed device state.
No restart recovery, native time virtualization, multi-display/rotation support or arbitrary
device compatibility is claimed.

Its immediate page reads sometimes lag native completion; later received events establish delivery.

## Retained file transport

Pass one absolute, nonexistent guest directory as the driver argument to select file transport.
The driver creates it exclusively and publishes `ready.json`. It refuses to restart in an existing
directory. No-argument stdin mode retains the default capacity; ready replies also report it.

The owning controller must first verify the ready generation/PID/UID and the acquired consumer,
foreground window and screenshot basis. Publish `request-N.json` atomically (push to a temporary
path, then rename) with the same request fields as stdin mode. The driver copies the bounded
payload to `accepted-N.json` before validation or native dispatch, then publishes `reply-N.json`
by atomic rename. Original admitted payloads and replies cannot be replaced by the producer.
A missing reply is uncertainty, even if an accepted file exists; never resend or start a replacement
driver to resolve it. Read the original result or retire the owned target. A controller must compare
the admitted payload with its original request before accepting a reply.

File mode defaults to a five-minute lifetime; a second driver argument selects milliseconds in
1..900000 (at most 15 minutes). It stops admitting requests after that lifetime and rejects nonregular, linked or
over-8192-byte request files. This bounds request waiting, not Android's native injection call;
the host must still supervise the process and target lifetime. An optional third argument declares
request capacity (1..4096, default 24), echoed in `ready.json`. It retains the declared request
limit, explicit finish/cancel semantics and absence of automatic cancellation. Atomic publication
is not a claim of storage survival across machine power loss. Guest evidence must be collected
before retiring the emulator if it is needed afterward.

Run the JVM journal safety checks separately from native dispatch qualification:

```sh
javac -d /absolute/fresh-test-classes InputFiles.java InputFilesTest.java
java -cp /absolute/fresh-test-classes dev.frontcap.input.InputFilesTest
```

The checks cover preservation of admitted requests/replies, rejection of replacement ownership,
changed repeated requests, oversize/symlink inputs and expiry. They do not qualify native input
delivery or product recovery.
