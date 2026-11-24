# Owned emulator write confinement prerequisite

`launch.py` explores whether the macOS Android SDK emulator can run with direct file writes
restricted to the adapter's independently mounted scratch filesystem. It runs beneath the
existing resource-process supervisor, whose bounded stdout/stderr collectors and live retirement
authority remain intact. It requires Python 3.11 or newer and macOS `sandbox-exec`.

The installed probe uses the existing Android host profile, frozen AVD inventory and supervised
scratch-volume configuration. Its `device.emulator` points to an executable wrapper that invokes:

```text
python3 -B launch.py --evidence OWNED_RUN_DIRECTORY --emulator ORIGINAL_SDK_EMULATOR \
  --sha256 ORIGINAL_EMULATOR_SHA256 -- ORIGINAL_EMULATOR_ARGUMENTS...
```

The wrapper must preserve every emulator argument and use the declared Python interpreter. Retain
its bytes, this launcher's bytes, the profile, original emulator digest and parent recipe as inputs.
Do not substitute it into an unrelated active target. The normal native ownership lock and port
preflight still apply. The evidence directory must already exist; original receipts are never
overwritten. `ANDROID_TMP` must name the owned mounted scratch root, and `ANDROID_AVD_HOME` must
name the staged inventory beneath it. The launcher does not create, adopt or delete a volume.

Before entering the sandbox, the launcher creates a private home and temporary directory beneath
scratch, saves the policy and root identity, and opens an empty descriptor-control file in the
evidence directory. It makes that descriptor inheritable, inventories all non-stdio descriptors,
then closes them. The policy denies file writes except under scratch and to `/dev/null`.
The restricted entry checks:

- The recorded scratch device/inode still matches.
- No non-stdio descriptor survived exec, including the deliberately injected descriptor.
- A new scratch file can be written, read and removed.
- Creating an outside file fails with `EPERM`, leaving no file.
- The outside descriptor-control file is still empty.

The checks emit one JSON record through the inherited supervisor output pipe. Only then does the
entry verify the emulator digest and replace itself with the original SDK executable. Failure
refuses the SDK exec and remains in normal resource evidence. Audit the original run, target
retirement, source inventory and recovery separately; a canary record alone is not app acceptance.

This probe qualifies a selected descendant/direct-write boundary. Its bootstrap evidence writes,
recorder and caller storage, pre-existing ADB/system services, other SDK entry points and shared
dependencies remain outside the restriction. It does not reserve whole-Run physical capacity or
qualify exhaustion, arbitrary consumers, concurrent namespace mutation, hostile code or iOS. Production integration must establish ownership and identity before acquisition, account for every
writer/root, preserve separate retirement capacity, and qualify the required overflow/loss cases.
