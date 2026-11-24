# Bounded Simulator data feasibility probe

Run only on macOS with an available Simulator runtime and a previously qualified SystemUI xctestrun.
The probe takes the shared native-test lock and requires a retired host session and no booted devices.
It creates one default-set device and one preallocated APFS image, copies only that new device's data
with `ditto`, verifies initial file contents/symlinks, and replaces its original data directory with
a link into the mounted filesystem.

```sh
python3 -B spikes/native-simulator-storage/probe.py \
  --output /absolute/fresh/output \
  --xctestrun /absolute/SystemUI.xctestrun \
  --runtime com.apple.CoreSimulator.SimRuntime.iOS-27-2 \
  --device-type com.apple.CoreSimulator.SimDeviceType.iPhone-16 \
  --size-mib 8192
```

The image must have actual backing allocation; 64 MiB is reserved for post-writer retirement. The
probe boots the owned device, enumerates `testSystemBridge`, checks that the runner's data container
resolves onto the bounded filesystem, and captures a screenshot. Enumeration is not execution of
the test body or verification of system input. SDK commands retain bounded output and original child
exit; Xcode enumeration uses the existing process-group capture/cleanup helper.

Cleanup shuts down and deletes only the returned UUID, confirms its directory/inventory absence,
then releases the reserve, ordinarily detaches and removes the original image. A transient detach
failure gets a 30-second retry window within the original live owner; every attempt repeats the
original image/device-map/mount validation and retains its command budget and result. No force-detach,
saved-owner adoption, shared-device modification or CrashReporter preference change is used. Cleanup
failure leaves the host fence unretired when owned storage remains uncertain. Evidence, source hashes,
command records, storage observations and owned crash-report identities remain outside the image.

This does not constrain CoreSimulator's shared services, runtime installation, host logs or caller
outputs. A successful probe is not whole-Run quota enforcement, an exhaustion/owner-loss qualification,
or a guarantee that every device write passes through the relocated data directory.

Add `--exercise-system-input` to execute the existing SystemUI test body after enumeration. It runs
one nonparallel XCTest on the same UUID, rediscovers the runner container through CoreSimulator
if XCTest reinstalls it, verifies that the new container remains in the relocated tree, observes
system alerts, installs/launches the existing
platform placeholder app, sends an actual XCTest Home input and foreground activation, and checks
state transitions 4 → 2/3 → 4 with the same app PID. The placeholder's data container must also reside
on the relocated filesystem. It copies hash-verified observation artifacts outside disposable data,
sends Finish only while the request cursor is certain, and requires successful XCTest completion
before device/volume cleanup. Pending input is never retried; the owned Xcode command retains its
120-second execution budget and existing process-group retirement behavior on failure.

This check exercises the installed SystemUI runner and verifies visibility transitions using the platform placeholder app.
