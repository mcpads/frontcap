# WebView timeline instrumentation

These scripts observe page events, timers, pointer input, media and navigation in an instrumented WebView. The native controller associates observations with the current page and document. Navigation can replace the document and invalidate earlier input coordinates.

The Android adapter embeds the shared JavaScript observers at compile time. For the JavaScript checks, run `node --test *.test.mjs` from this directory after installing the dependencies declared in [package.json](package.json).

The Objective-C sources require the Apple Simulator SDK. The [UIKit package](../../packages/frontcap_uikit/README.md) describes app and XCTest instrumentation.
