# Xcode custom device-set feasibility

This host-only experiment redirects `SimServiceContext` device-set selection inside one
`xcodebuild` process. It is not a supported launch-profile option or consumer library. It cannot
establish storage capacity, cleanup authority or complete UIKit/Flutter integration by itself.

The interception point comes from the upstream idb
[Xcode launcher](https://github.com/facebook/idb/blob/8909cd5c214fb3d4637d0312fb79c1b20199fc14/FBXCTestCore/Utility/XcodeBuildOperation.swift)
and [reporter shim](https://github.com/facebook/idb/blob/8909cd5c214fb3d4637d0312fb79c1b20199fc14/Shims/Shimulator/TestReporterShim/XCTestReporterShim.m).
Those implementations pair custom-set selection with host-library injection; a path environment
variable alone is not the mechanism. The probe implements only that selection, without test
reporting hooks or an idb runtime dependency.

Build on the selected macOS SDK host:

```sh
xcrun clang -dynamiclib -fobjc-arc -framework Foundation device_set.m -o device_set.dylib
codesign --force --sign - device_set.dylib
```

An executor must own a fresh device created by `simctl --set <directory>`, hold the native host
lease and retain command/cleanup evidence. For the one Xcode child, set
`FRONTCAP_XCODE_DEVICE_SET` to that absolute directory and `DYLD_INSERT_LIBRARIES` to the absolute
library path. The constructor removes both variables before Xcode can spawn children. Do not
set them globally or modify the user's default Simulator set.

The probe fails before execution if its directory, framework or selector is unavailable. It uses
a private SDK method; successful enumeration is only discovery evidence. `-enumerate-tests` may
boot a Simulator and launch the platform test runner, so even this probe requires owned retirement.
Further integration must prove app-container discovery, input/system runner operation and cleanup.

A separate tiny-host control loads the
library and checks environment removal. Preserve that distinction before reusing this probe.
