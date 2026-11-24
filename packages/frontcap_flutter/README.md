# Flutter test-build instrumentation

This local, unpublished package owns the controlled UI-isolate timeline, explicit framework-frame
binding, semantic observations, framework raster capture and authenticated test-session endpoint.
It is a `dev_dependencies` dependency of an instrumented app, not a production runtime dependency.
The package preserves the ownership model and now provides focused committed-text/range editing
and observed directional scrolling through semantics, using fresh observations.
Real IME composition, plugin and whole-app clock support are not implied.

Add a path dependency on this directory in the app's `dev_dependencies`. Freeze the repository
containing both the app and package when submitting a run; copying only the app directory is
insufficient. Commit the consuming app's resolved dependency lockfile.

Install the binding early in `integration_test/flutter_test_config.dart`:

```dart
import 'dart:async';
import 'package:frontcap_flutter/frontcap_flutter.dart';

Future<void> testExecutable(FutureOr<void> Function() testMain) async {
  if (const bool.fromEnvironment('FRONTCAP_CONTROLLED_FRAMES')) {
    ControlledFramesBinding();
  }
  await testMain();
}
```

The app owns its `integration_test/session_test.dart` bootstrap:

```dart
import 'package:frontcap_flutter/frontcap_flutter.dart';
import 'package:my_app/main.dart';

void main() => runControlledAppTest(() => const MyApp());
```

The app factory runs inside the controlled zone. It must return the application's root widget;
setup requiring asynchronous platform initialization needs an explicitly designed bootstrap and is
not provided by this synchronous factory. The existing worker sets the binding define, reads the
endpoint announcement, owns acquisition/retirement and retains the journal. Session lifetime and
transport deadlines remain real time. Ordinary app code need not import this package.

Fault-injection fixtures may supply `createSurface: (binding) => DiagnosticSurface(binding)`.
The session owns and disposes that surface just like the default `ObservedSurface`. This optional
factory runs before mounting the app; it does not change the app factory's controlled-zone scope.
