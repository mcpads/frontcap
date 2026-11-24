import 'dart:io';
import 'dart:convert';

import 'package:flutter/foundation.dart' show debugPrintDone;
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

import 'clock_server.dart';
import 'controlled_frames_binding.dart';
import 'observed_surface.dart';

void runControlledAppTest(
  Widget Function() createApp, {
  ObservedSurface Function(ControlledFramesBinding)? createSurface,
}) {
  final binding =
      IntegrationTestWidgetsFlutterBinding.instance as ControlledFramesBinding;
  testWidgets('owned Flutter session', (tester) async {
    // TestWidgetsFlutterBinding.reset substitutes '/' before every test. An installed session
    // must expose the engine's actual OS launch route, including a cold URL Intent.
    binding.platformDispatcher.clearDefaultRouteNameTestValue();
    await tester.pumpWidget(const SizedBox());
    binding.startControl();
    final surface = createSurface?.call(binding) ?? ObservedSurface(binding);
    final directory = await Directory.systemTemp.createTemp(
      'frontcap-session-',
    );
    final server = ClockServer(binding, directory, surface: surface);
    try {
      binding.timeline!.run(() => binding.mount(createApp()));
      debugPrint(
        'FRONTCAP_FLUTTER_ENDPOINT ${jsonEncode({...await server.start(), 'scenario': 'session'})}',
      );
      await debugPrintDone;
      await server.stopped.timeout(const Duration(hours: 1));
      final encoded = base64Encode(
        zlib.encode(File('${directory.path}/requests.jsonl').readAsBytesSync()),
      );
      final count = (encoded.length / 1024).ceil();
      for (var index = 0; index < count; index++) {
        final end = ((index + 1) * 1024).clamp(0, encoded.length);
        debugPrint(
          'FRONTCAP_FLUTTER_JOURNAL_CHUNK ${jsonEncode({'index': index, 'count': count, 'data': encoded.substring(index * 1024, end)})}',
        );
      }
      await debugPrintDone.timeout(const Duration(seconds: 30));
    } finally {
      await server.close();
      surface.dispose();
      binding.mount(const SizedBox());
      binding.stopControl();
    }
  }, timeout: const Timeout(Duration(minutes: 61)));
}
