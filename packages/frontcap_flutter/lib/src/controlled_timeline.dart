import 'dart:async';

import 'package:fake_async/fake_async.dart';

/// UI-isolate asynchronous work only. Ordinary Dart clocks stay real.
/// A failed drain fences this timeline; callers must replace the generation.
class ControlledTimeline {
  ControlledTimeline({this.callbackLimit = 10000}) {
    if (callbackLimit <= 0) throw ArgumentError.value(callbackLimit);
  }

  final int callbackLimit;
  final FakeAsync _time = FakeAsync();
  Object? _failure;
  bool _advancing = false;
  int _callbacks = 0;
  int _cancelledOnFailure = 0;

  Duration get elapsed => _time.elapsed;
  bool get faulted => _failure != null;
  int get pendingTimers => _time.pendingTimers.length;

  void _fail(Object error) {
    _failure ??= error;
    final timers = _time.pendingTimers;
    _cancelledOnFailure += timers.length;
    for (final timer in timers) {
      timer.cancel();
    }
  }

  void _invoke(void Function() callback) {
    if (faulted) return;
    if (_callbacks >= callbackLimit) {
      _fail(StateError('Asynchronous callback limit exceeded'));
      return;
    }
    _callbacks++;
    callback();
  }

  T run<T>(T Function() body) {
    if (faulted) throw StateError('Timeline faulted; replace the generation');
    return _time.run(
      (_) => runZoned(
        body,
        zoneSpecification: ZoneSpecification(
          scheduleMicrotask: (self, parent, zone, callback) {
            if (!faulted) {
              parent.scheduleMicrotask(zone, () => _invoke(callback));
            }
          },
          createTimer: (self, parent, zone, duration, callback) =>
              parent.createTimer(zone, duration, () => _invoke(callback)),
          createPeriodicTimer: (self, parent, zone, duration, callback) =>
              parent.createPeriodicTimer(
                zone,
                duration,
                (timer) => _invoke(() => callback(timer)),
              ),
          handleUncaughtError: (self, parent, zone, error, stack) =>
              _fail(error),
        ),
      ),
    );
  }

  void drainMicrotasks() {
    if (!_advancing) throw StateError('Drain requires an active advance');
    _time.flushMicrotasks();
  }

  AdvanceOutcome advance(Duration delta, {void Function()? frame}) {
    if (delta.isNegative) throw ArgumentError.value(delta, 'delta');
    if (_advancing) throw StateError('An advance is already active');
    if (faulted) throw StateError('Timeline faulted; replace the generation');
    final before = elapsed;
    _callbacks = 0;
    _advancing = true;
    var frameCompleted = false;
    try {
      run(() {
        _time.elapse(delta);
        _time.flushMicrotasks();
        if (!faulted && frame != null) {
          frame();
          _time.flushMicrotasks();
          frameCompleted = !faulted;
        }
      });
    } catch (error) {
      _fail(error);
    } finally {
      _advancing = false;
    }
    return AdvanceOutcome(
      before: before,
      requested: before + delta,
      after: elapsed,
      callbacks: _callbacks,
      drainCompleted: !faulted,
      frameCompleted: frameCompleted,
      failure: _failure?.toString(),
      cancelledOnFailure: _cancelledOnFailure,
    );
  }
}

class AdvanceOutcome {
  const AdvanceOutcome({
    required this.before,
    required this.requested,
    required this.after,
    required this.callbacks,
    required this.drainCompleted,
    required this.frameCompleted,
    required this.failure,
    required this.cancelledOnFailure,
  });

  final Duration before, requested, after;
  final int callbacks, cancelledOnFailure;
  final bool drainCompleted, frameCompleted;
  final String? failure;

  Map<String, Object?> toJson() => {
    'before_us': before.inMicroseconds,
    'requested_us': requested.inMicroseconds,
    'after_us': after.inMicroseconds,
    'time_changed': after != before,
    'callbacks': callbacks,
    'drain_completed': drainCompleted,
    'framework_frame_completed': frameCompleted,
    'failure': failure,
    'cancelled_on_failure': cancelledOnFailure,
  };
}
