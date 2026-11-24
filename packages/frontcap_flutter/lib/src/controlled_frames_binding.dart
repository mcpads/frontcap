// Diagnostic framework control only; ordinary Dart clocks remain live.
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

import 'controlled_timeline.dart';

class ControlledFramesBinding extends IntegrationTestWidgetsFlutterBinding {
  ControlledTimeline? timeline;
  AdvanceOutcome? lastAdvance;
  final captureRoot = GlobalKey();
  int revision = 0;
  int metricsChanges = 0;
  int frames = 0;
  int droppedFrames = 0;

  void startControl({int callbackLimit = 10000}) {
    revision++;
    timeline = ControlledTimeline(callbackLimit: callbackLimit);
    framePolicy = LiveTestWidgetsFlutterBindingFramePolicy.benchmark;
    resetEpoch();
  }

  void stopControl() {
    revision++;
    timeline = null;
    framePolicy = LiveTestWidgetsFlutterBindingFramePolicy.onlyPumps;
  }

  @override
  void handleMetricsChanged() {
    // Keyboard/view changes can arrive while app time is held. Existing input
    // and in-flight captures must not retain authority over the old geometry.
    revision++;
    metricsChanges++;
    super.handleMetricsChanged();
  }

  @override
  void handleBeginFrame(Duration? rawTimeStamp) {
    if (timeline == null) {
      super.handleBeginFrame(rawTimeStamp);
    } else {
      droppedFrames++;
    }
  }

  @override
  void handleDrawFrame() {
    if (timeline == null) super.handleDrawFrame();
  }

  void mount(Widget widget) {
    revision++;
    timeline!.run(
      () => attachRootWidget(
        wrapWithDefaultView(RepaintBoundary(key: captureRoot, child: widget)),
      ),
    );
    step();
  }

  AdvanceOutcome advance(Duration duration) {
    revision++;
    final time = timeline!;
    return lastAdvance = time.advance(
      duration,
      frame: () {
        super.handleBeginFrame(time.elapsed);
        time.drainMicrotasks();
        super.handleDrawFrame();
        frames++;
      },
    );
  }

  void step([Duration duration = Duration.zero]) {
    final result = advance(duration);
    if (!result.drainCompleted || !result.frameCompleted) {
      throw StateError('Controlled frame incomplete: ${result.toJson()}');
    }
  }

  void tap(WidgetTester tester, Finder target) {
    tapAt(tester.getCenter(target));
  }

  void tapAt(Offset position) {
    revision++;
    final time = timeline!;
    final pointer = TestPointer(1);
    time.run(() {
      handlePointerEventForSource(
        pointer.down(position),
        source: TestBindingEventSource.test,
      );
      handlePointerEventForSource(
        pointer.up(),
        source: TestBindingEventSource.test,
      );
    });
    final result = time.advance(Duration.zero);
    if (!result.drainCompleted) {
      throw StateError('Input drain incomplete: ${result.toJson()}');
    }
  }
}
