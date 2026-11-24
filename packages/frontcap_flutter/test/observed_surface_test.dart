import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:frontcap_flutter/frontcap_flutter.dart';

void main() {
  final binding = ControlledFramesBinding();
  testWidgets(
    'unavailable targets reject without journaling or input effects',
    (tester) async {
      await tester.pumpWidget(const SizedBox());
      binding.startControl();
      final surface = ObservedSurface(binding);
      final directory = Directory.systemTemp.createTempSync(
        'surface-admission-',
      );
      final server = ClockServer(binding, directory, surface: surface);
      final effects = <String>[];
      Widget target(String label, {bool enabled = true, bool hidden = false}) {
        return Semantics(
          label: label,
          enabled: enabled,
          hidden: hidden,
          onTap: () => effects.add(label),
          excludeSemantics: true,
          child: GestureDetector(
            onTap: () => effects.add(label),
            behavior: HitTestBehavior.opaque,
            child: const SizedBox(width: 100, height: 40),
          ),
        );
      }

      try {
        binding.mount(
          Directionality(
            textDirection: TextDirection.ltr,
            child: Stack(
              clipBehavior: Clip.none,
              children: [
                Positioned(top: 20, left: 20, child: target('available')),
                Positioned(
                  top: 80,
                  left: 20,
                  child: target('disabled', enabled: false),
                ),
                Positioned(
                  top: 140,
                  left: 20,
                  child: target('hidden', hidden: true),
                ),
                Positioned(top: 2000, left: 20, child: target('outside')),
              ],
            ),
          ),
        );
        final observation = surface.observe();
        ClockRequest tap(String id, String target) => ClockRequest.parse({
          'generation': server.generation,
          'id': id,
          'operation': 'tap',
          'observation': observation.id,
          'target': target,
        });
        for (final label in ['disabled', 'hidden', 'outside']) {
          final node = observation.nodes.singleWhere(
            (node) => node.label == label,
          );
          expect(node.tappable, isFalse, reason: label);
          await expectLater(
            server.execute(tap(label, node.id)),
            throwsStateError,
          );
        }
        expect(effects, isEmpty);
        expect(File('${directory.path}/requests.jsonl').existsSync(), isFalse);
        final available = observation.nodes.singleWhere(
          (node) => node.label == 'available',
        );
        final request = tap('accepted', available.id);
        final result = await server.execute(request);
        expect(result['status'], 'completed');
        expect(effects, ['available']);
        expect(await server.execute(request), result);
        expect(effects, ['available']);
        final journal = File('${directory.path}/requests.jsonl');
        final before = journal.readAsStringSync();
        await expectLater(
          server.execute(tap('stale', available.id)),
          throwsStateError,
        );
        expect(journal.readAsStringSync(), before);
      } finally {
        surface.dispose();
        binding.mount(const SizedBox());
        binding.stopControl();
        directory.deleteSync(recursive: true);
      }
    },
  );
  testWidgets('committed range edits preserve Unicode and retry identity', (
    tester,
  ) async {
    await tester.pumpWidget(const SizedBox());
    binding.startControl();
    final surface = ObservedSurface(binding);
    final directory = Directory.systemTemp.createTempSync('text-edit-');
    final server = ClockServer(binding, directory, surface: surface);
    final controller = TextEditingController();
    final changes = <String>[];
    try {
      binding.mount(
        MaterialApp(
          home: Scaffold(
            body: TextField(
              controller: controller,
              onChanged: changes.add,
              decoration: const InputDecoration(labelText: 'Title'),
            ),
          ),
        ),
      );
      var observation = surface.observe();
      var field = observation.nodes.singleWhere((n) => n.label == 'Title');
      ClockRequest edit(String id, int start, int end, String text) =>
          ClockRequest.parse({
            'generation': server.generation,
            'id': id,
            'operation': 'replace_text',
            'observation': observation.id,
            'target': field.id,
            'start': start,
            'end': end,
            'text': text,
          });
      await expectLater(
        server.execute(edit('unfocused', 0, 0, 'no')),
        throwsStateError,
      );
      expect(File('${directory.path}/requests.jsonl').existsSync(), isFalse);
      surface.tap(observation, field.id);
      observation = surface.observe();
      field = observation.nodes.singleWhere((n) => n.label == 'Title');
      expect(field.focused, isTrue);
      expect(field.editable, isTrue);
      final first = edit('create', 0, 0, '부산 여행 🌊');
      final result = await server.execute(first);
      expect(result['status'], 'completed');
      expect(controller.text, '부산 여행 🌊');
      expect(changes, ['부산 여행 🌊']);
      expect(await server.execute(first), result);
      expect(changes, hasLength(1));
      await expectLater(
        server.execute(edit('stale', 0, 2, '서울')),
        throwsStateError,
      );
      observation = surface.observe();
      field = observation.nodes.singleWhere((n) => n.label == 'Title');
      final journal = File('${directory.path}/requests.jsonl');
      final before = journal.readAsStringSync();
      await expectLater(
        server.execute(edit('split-surrogate', 7, 8, '')),
        throwsFormatException,
      );
      expect(journal.readAsStringSync(), before);
      await server.execute(edit('correct', 3, 5, '주말 여행'));
      expect(controller.text, '부산 주말 여행 🌊');
      expect(changes, ['부산 여행 🌊', '부산 주말 여행 🌊']);
      expect(binding.timeline!.elapsed, Duration.zero);
      expect(
        () => ClockRequest.parse({
          ...first.toJson(),
          'text': String.fromCharCode(0xd800),
        }),
        throwsFormatException,
      );
    } finally {
      surface.dispose();
      binding.mount(const SizedBox());
      binding.stopControl();
      controller.dispose();
      directory.deleteSync(recursive: true);
    }
  });
  testWidgets(
    'platform insets invalidate old input and exclude obscured targets',
    (tester) async {
      await tester.pumpWidget(const SizedBox());
      binding.startControl();
      final surface = ObservedSurface(binding);
      final directory = Directory.systemTemp.createTempSync('changed-basis-');
      final server = ClockServer(binding, directory, surface: surface);
      var taps = 0;
      try {
        binding.mount(
          Directionality(
            textDirection: TextDirection.ltr,
            child: Stack(
              children: [
                Positioned(
                  bottom: 20,
                  left: 20,
                  child: Semantics(
                    label: 'bottom action',
                    onTap: () => taps++,
                    child: GestureDetector(
                      behavior: HitTestBehavior.opaque,
                      onTap: () => taps++,
                      child: const SizedBox(width: 100, height: 40),
                    ),
                  ),
                ),
              ],
            ),
          ),
        );
        final before = surface.observe();
        final target = before.nodes.singleWhere(
          (n) => n.label == 'bottom action',
        );
        expect(target.tappable, isTrue);
        tester.view.viewInsets = FakeViewPadding(
          bottom: 120 * tester.view.devicePixelRatio,
        );
        expect(
          () => surface.tap(before, target.id),
          throwsA(
            isA<ObservationChanged>()
                .having(
                  (error) => error.observedRevision,
                  'observed revision',
                  before.revision,
                )
                .having(
                  (error) => error.currentRevision,
                  'current revision',
                  greaterThan(before.revision),
                )
                .having(
                  (error) => error.metricsChanges,
                  'platform metrics changes',
                  greaterThan(0),
                ),
          ),
        );
        final request = ClockRequest.parse({
          'generation': server.generation,
          'id': 'changed-basis',
          'operation': 'tap',
          'observation': before.id,
          'target': target.id,
        });
        final outcome = await server.execute(request);
        expect(outcome['not_applied'], 'observation_changed');
        expect(outcome['logical_us'], 0);
        final obscured = surface.observe();
        expect(obscured.view.insets[3], 120);
        final covered = obscured.nodes.singleWhere(
          (n) => n.label == 'bottom action',
        );
        expect(covered.tappable, isFalse);
        expect(() => surface.tap(obscured, covered.id), throwsStateError);
        expect(taps, 0);
        expect(binding.timeline!.elapsed, Duration.zero);
        tester.view.resetViewInsets();
        expect(() => surface.resolve(obscured.id), throwsStateError);
        expect(await server.execute(request), outcome);
        expect(taps, 0);
        final restored = surface.observe();
        surface.tap(restored, target.id);
        expect(taps, 1);
      } finally {
        tester.view.resetViewInsets();
        surface.dispose();
        binding.mount(const SizedBox());
        binding.stopControl();
        directory.deleteSync(recursive: true);
      }
    },
  );
  testWidgets(
    'observed scroll preserves app time and retries do not repeat it',
    (tester) async {
      await tester.pumpWidget(const SizedBox());
      binding.startControl();
      final surface = ObservedSurface(binding);
      final controller = ScrollController();
      final directory = Directory.systemTemp.createTempSync('observed-scroll-');
      final server = ClockServer(binding, directory, surface: surface);
      try {
        binding.mount(
          MaterialApp(
            home: Scaffold(
              body: ListView.builder(
                controller: controller,
                itemExtent: 100,
                itemCount: 40,
                itemBuilder: (context, index) =>
                    ListTile(title: Text('Note $index')),
              ),
            ),
          ),
        );
        var observation = surface.observe();
        var region = observation.nodes.singleWhere(
          (n) => n.scrollActions.contains('up'),
        );
        expect(region.scrollPosition, 0);
        expect(region.scrollMax, greaterThan(0));
        final request = ClockRequest.parse({
          'generation': server.generation,
          'id': 'scroll',
          'operation': 'scroll',
          'observation': observation.id,
          'target': region.id,
          'direction': 'up',
        });
        final first = await server.execute(request);
        expect(first['status'], 'completed');
        expect(binding.timeline!.elapsed, Duration.zero);
        expect(controller.offset, greaterThan(0));
        binding.step(const Duration(seconds: 1));
        final offset = controller.offset;
        expect(offset, greaterThan(0));
        expect(await server.execute(request), first);
        binding.step(const Duration(seconds: 1));
        expect(controller.offset, offset);
        await expectLater(
          server.execute(
            ClockRequest.parse({...request.toJson(), 'id': 'stale-scroll'}),
          ),
          throwsStateError,
        );
        // Drive to the actual extent through repeated observed actions, bounded
        // by the known finite consumer. Do not use a missing cached child as end.
        for (var count = 0; count < 15; count++) {
          observation = surface.observe();
          region = observation.nodes.singleWhere((n) => n.id == region.id);
          if (!region.scrollActions.contains('up')) break;
          surface.scroll(observation, region.id, 'up');
          binding.step(const Duration(seconds: 1));
        }
        observation = surface.observe();
        region = observation.nodes.singleWhere((n) => n.id == region.id);
        expect(region.scrollPosition, region.scrollMax);
        expect(region.scrollActions, isNot(contains('up')));
        expect(region.scrollActions, contains('down'));
        expect(find.text('Note 39'), findsOneWidget);
        final before = File('${directory.path}/requests.jsonl')
            .readAsStringSync();
        await expectLater(
          server.execute(
            ClockRequest.parse({
              ...request.toJson(),
              'id': 'past-end',
              'observation': observation.id,
            }),
          ),
          throwsStateError,
        );
        expect(
          File('${directory.path}/requests.jsonl').readAsStringSync(),
          before,
        );
      } finally {
        surface.dispose();
        binding.mount(const SizedBox());
        binding.stopControl();
        controller.dispose();
        directory.deleteSync(recursive: true);
      }
    },
  );
  testWidgets('validation animation requires explicit app-time advance', (
    tester,
  ) async {
    await tester.pumpWidget(const SizedBox());
    binding.startControl();
    final surface = ObservedSurface(binding);
    final form = GlobalKey<FormState>();
    try {
      binding.mount(
        MaterialApp(
          home: Scaffold(
            body: Form(
              key: form,
              child: Column(
                children: [
                  TextFormField(validator: (value) => 'Add a title'),
                  TextButton(
                    onPressed: () => form.currentState!.validate(),
                    child: const Text('Save'),
                  ),
                ],
              ),
            ),
          ),
        ),
      );
      var observation = surface.observe();
      surface.tap(
        observation,
        observation.nodes
            .singleWhere((n) => n.label == 'Save' && n.tappable)
            .id,
      );
      bool hasError() =>
          surface.observe().nodes.any((n) => n.label.contains('Add a title'));
      expect(hasError(), isFalse);
      expect(binding.timeline!.elapsed, Duration.zero);
      binding.step(const Duration(milliseconds: 500));
      expect(hasError(), isTrue);
    } finally {
      surface.dispose();
      binding.mount(const SizedBox());
      binding.stopControl();
    }
  });
}
