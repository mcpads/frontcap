# Android result-finalization diagnostic preparation

This opt-in tool stages an existing **debuggable arm64 APK** for an exact `rename`/absolute-path `renameat` interruption.
It reuses the rename-boundary source in `../ios-storage-boundary/rename-stop.c`, with ELF symbol
interposition through `LD_PRELOAD`. It does not add a consumer SDK or modify application code/assets.
This is qualification tooling, not a general release-APK attachment API.

Run on a host with an Android NDK compiler, SDK build tools and a JDK:

```sh
python3 spikes/android-storage-boundary/prepare.py ORIGINAL.apk NEW_OUTPUT \
  /absolute/ndk/bin/aarch64-linux-android30-clang /absolute/sdk/build-tools/VERSION \
  /absolute/jdk/bin/keytool --rename-boundary \
  /absolute/guest/result.json.pending /absolute/guest/result.json /absolute/guest/reached.json
```

The input APK is unchanged. The output must not exist. The preparer requires one existing typed
`debuggable=true` and `extractNativeLibs` attribute in binary AndroidManifest.xml; it changes only
the latter's boolean value. It adds an arm64 preload library and `wrap.sh`, removes old JAR signing
entries, aligns the ZIP and signs with a new diagnostic-only key. Other entry contents must match
exactly. Signature verification and 16 KB library alignment checks precede the final inventory.
The temporary unsigned archive is removed after verification; the signed APK and preparation proof
remain. An exception leaves the original failed output for inspection, not an automatically retried
build. Prepared products must stay within isolated owned test installations.

[Android's wrap-script guide](https://developer.android.com/ndk/guides/wrap-script) requires a
debuggable APK and legacy native-library packaging. This preparer expresses the extraction change
in the existing binary manifest instead of rebuilding a consumer Gradle project. Whether a specific
app's runtime calls the interposed symbol still requires installed proof. APK signing changes its
identity; this is not production-signature equivalence or in-place modification of a user's app.

The external controller must verify the exact app PID/start identity and stop marker, retain pending
bytes and final-file absence, then restart or retire the owned process within a deadline. It must
not assume that a completed input/timer command proves successful application persistence.
Relative-path `renameat`, direct syscalls, non-arm64 launch, partial writes and power loss are outside
this probe. The selected Flutter engine imports `renameat`; wrapping `rename` alone does not qualify
its persistence boundary. Both entry points use the same exact-path, one-shot stop receipt.
