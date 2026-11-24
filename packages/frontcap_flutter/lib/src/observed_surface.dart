import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:flutter/rendering.dart';

import 'controlled_frames_binding.dart';

const _scrollActions = <String, SemanticsAction>{
  'up': SemanticsAction.scrollUp,
  'down': SemanticsAction.scrollDown,
  'left': SemanticsAction.scrollLeft,
  'right': SemanticsAction.scrollRight,
};

class ObservationChanged extends StateError {
  ObservationChanged(
    this.observedRevision,
    this.currentRevision,
    this.metricsChanges,
  ) : super('Observed geometry or frame changed before dispatch');

  final int observedRevision, currentRevision, metricsChanges;

  Map<String, Object> toJson() => {
    'observed_revision': observedRevision,
    'current_revision': currentRevision,
    'metrics_changes': metricsChanges,
  };
}

class ObservedSurface {
  ObservedSurface(this.binding) : _semantics = binding.ensureSemantics();
  final ControlledFramesBinding binding;
  final SemanticsHandle _semantics;
  int _sequence = 0;
  SurfaceObservation? _latest;
  int _observedMetricsChanges = 0;

  SurfaceObservation observe() {
    binding.step();
    final view = binding.renderViews.single;
    final root = view.owner!.semanticsOwner?.rootSemanticsNode;
    if (root == null) throw StateError('Semantics unavailable');
    final insets = view.flutterView.viewInsets;
    final ratio = view.configuration.devicePixelRatio;
    final geometry = SurfaceView(view.size.width, view.size.height, [
      insets.left / ratio,
      insets.top / ratio,
      insets.right / ratio,
      insets.bottom / ratio,
    ]);
    final viewport = geometry.unobscured;
    final nodes = <ObservedNode>[];
    void visit(SemanticsNode node, Matrix4 parent) {
      final transform = parent.clone();
      if (node.transform != null) transform.multiply(node.transform!);
      final data = node.getSemanticsData();
      final rect = MatrixUtils.transformRect(transform, node.rect);
      nodes.add(
        ObservedNode(
          id: 's${node.id}',
          label: data.label,
          tooltip: data.tooltip,
          value: data.value,
          rect: rect,
          scrollActions:
              !data.flagsCollection.isHidden &&
                  data.flagsCollection.isEnabled != ui.Tristate.isFalse &&
                  !rect.isEmpty &&
                  viewport.contains(rect.center)
              ? [
                  for (final entry in _scrollActions.entries)
                    if (data.hasAction(entry.value)) entry.key,
                ]
              : const [],
          scrollPosition: data.scrollPosition?.isFinite == true
              ? data.scrollPosition
              : null,
          scrollMin: data.scrollExtentMin?.isFinite == true
              ? data.scrollExtentMin
              : null,
          scrollMax: data.scrollExtentMax?.isFinite == true
              ? data.scrollExtentMax
              : null,
          editable:
              data.hasAction(SemanticsAction.setText) &&
              data.flagsCollection.isTextField &&
              !data.flagsCollection.isReadOnly &&
              !data.flagsCollection.isObscured &&
              !data.flagsCollection.isHidden &&
              data.flagsCollection.isEnabled != ui.Tristate.isFalse &&
              !rect.isEmpty &&
              viewport.contains(rect.center),
          focused: data.flagsCollection.isFocused == ui.Tristate.isTrue,
          tappable:
              data.hasAction(SemanticsAction.tap) &&
              !data.flagsCollection.isHidden &&
              data.flagsCollection.isEnabled != ui.Tristate.isFalse &&
              !rect.isEmpty &&
              viewport.contains(rect.center),
        ),
      );
      node.visitChildren((child) {
        visit(child, transform);
        return true;
      });
    }

    // Semantics is in view physical coordinates; input and captured layers use
    // local logical coordinates. Invert the complete view transform, including
    // scale and viewport translation, rather than assuming a fixed pixel ratio.
    final physicalToLogical = view.configuration.toMatrix()..invert();
    visit(root, physicalToLogical);
    _observedMetricsChanges = binding.metricsChanges;
    return _latest = SurfaceObservation(
      id: 'o${++_sequence}',
      revision: binding.revision,
      frame: binding.frames,
      logicalUs: binding.timeline!.elapsed.inMicroseconds,
      nodes: nodes,
      view: geometry,
    );
  }

  void _requireCurrent(SurfaceObservation observation) {
    if (!identical(observation, _latest)) {
      throw StateError('Stale observation');
    }
    if (observation.revision != binding.revision) {
      throw ObservationChanged(
        observation.revision,
        binding.revision,
        binding.metricsChanges - _observedMetricsChanges,
      );
    }
  }

  SurfaceObservation resolve(String id) {
    final observation = _latest;
    if (observation == null || observation.id != id) {
      throw StateError('Stale observation');
    }
    _requireCurrent(observation);
    return observation;
  }

  ObservedNode requireTarget(SurfaceObservation observation, String target) {
    _requireCurrent(observation);
    final node = observation.nodes.singleWhere((node) => node.id == target);
    if (!node.tappable) throw StateError('Target is not tappable');
    return node;
  }

  void tap(SurfaceObservation observation, String target) {
    final node = requireTarget(observation, target);
    _latest = null; // Invalidate before dispatch, including a failed input.
    binding.tapAt(node.rect.center);
    binding.step();
  }

  void requireScroll(
    SurfaceObservation observation,
    String target,
    String direction,
  ) {
    _requireCurrent(observation);
    final node = observation.nodes.singleWhere((node) => node.id == target);
    if (!node.scrollActions.contains(direction)) {
      throw StateError('Scroll action unavailable for observed target');
    }
  }

  void scroll(SurfaceObservation observation, String target, String direction) {
    requireScroll(observation, target, direction);
    _latest = null;
    binding.timeline!.run(() {
      binding.renderViews.single.owner!.semanticsOwner!.performAction(
        int.parse(target.substring(1)),
        _scrollActions[direction]!,
      );
    });
    binding.step();
  }

  String requireTextEdit(
    SurfaceObservation observation,
    String target,
    int start,
    int end,
    String text,
  ) {
    _requireCurrent(observation);
    final node = observation.nodes.singleWhere((node) => node.id == target);
    if (!node.editable || !node.focused) {
      throw StateError('Target is not a focused editable field');
    }
    final value = node.value;
    bool boundary(int offset) =>
        offset >= 0 &&
        offset <= value.length &&
        !(offset > 0 &&
            offset < value.length &&
            value.codeUnitAt(offset - 1) >= 0xd800 &&
            value.codeUnitAt(offset - 1) <= 0xdbff &&
            value.codeUnitAt(offset) >= 0xdc00 &&
            value.codeUnitAt(offset) <= 0xdfff);
    if (start > end ||
        !boundary(start) ||
        !boundary(end) ||
        value.length - (end - start) + text.length > 16384) {
      throw const FormatException('Invalid UTF-16 edit range or text length');
    }
    return value.replaceRange(start, end, text);
  }

  void replaceText(
    SurfaceObservation observation,
    String target,
    int start,
    int end,
    String text,
  ) {
    final replacement = requireTextEdit(observation, target, start, end, text);
    _latest = null;
    binding.timeline!.run(() {
      binding.renderViews.single.owner!.semanticsOwner!.performAction(
        int.parse(target.substring(1)),
        SemanticsAction.setText,
        replacement,
      );
    });
    binding.step();
  }

  Future<SurfaceCapture> capture(SurfaceObservation observation) async {
    _requireCurrent(observation);
    try {
      final boundary =
          binding.captureRoot.currentContext!.findRenderObject()!
              as RenderRepaintBoundary;
      if (boundary.debugNeedsPaint) {
        throw StateError('Framework paint incomplete');
      }
      final image = await boundary
          .toImage(pixelRatio: 1)
          .timeout(const Duration(seconds: 10));
      try {
        final data = await image
            .toByteData(format: ui.ImageByteFormat.png)
            .timeout(const Duration(seconds: 10));
        if (data == null) throw StateError('PNG encoding unavailable');
        _requireCurrent(observation);
        return SurfaceCapture(
          observation,
          data.buffer.asUint8List(data.offsetInBytes, data.lengthInBytes),
          null,
        );
      } finally {
        image.dispose();
      }
    } catch (error) {
      // Keep the state/semantics observation even when raster evidence is incomplete.
      return SurfaceCapture(observation, null, error.toString());
    }
  }

  void dispose() => _semantics.dispose();
}

class ObservedNode {
  const ObservedNode({
    required this.id,
    required this.label,
    required this.tooltip,
    required this.value,
    required this.rect,
    required this.tappable,
    required this.editable,
    required this.focused,
    required this.scrollActions,
    this.scrollPosition,
    this.scrollMin,
    this.scrollMax,
  });
  final String id, label, tooltip, value;
  final Rect rect;
  final bool tappable, editable, focused;
  final List<String> scrollActions;
  final double? scrollPosition, scrollMin, scrollMax;
  Map<String, Object?> toJson() => {
    'id': id,
    'label': label,
    'tooltip': tooltip,
    'value': value,
    'tappable': tappable,
    'editable': editable,
    'focused': focused,
    'scroll_actions': scrollActions,
    'scroll_position': scrollPosition,
    'scroll_min': scrollMin,
    'scroll_max': scrollMax,
    'rect': [rect.left, rect.top, rect.right, rect.bottom],
  };
}

class SurfaceView {
  const SurfaceView(this.width, this.height, this.insets);
  final double width, height;
  // Platform-reported obscured edges, in logical pixels: left/top/right/bottom.
  final List<double> insets;
  Rect get unobscured => Rect.fromLTRB(
    insets[0],
    insets[1],
    width - insets[2],
    height - insets[3],
  );
  Map<String, Object?> toJson() => {
    'width': width,
    'height': height,
    'insets': insets,
  };
}

class SurfaceObservation {
  const SurfaceObservation({
    required this.id,
    required this.revision,
    required this.frame,
    required this.logicalUs,
    required this.nodes,
    required this.view,
  });
  final String id;
  final int revision, frame, logicalUs;
  final List<ObservedNode> nodes;
  final SurfaceView view;
  Map<String, Object?> toJson() => {
    'observation': id,
    'revision': revision,
    'framework_frame': frame,
    'logical_us': logicalUs,
    'nodes': nodes.map((node) => node.toJson()).toList(),
    'view': view.toJson(),
  };
}

class SurfaceCapture {
  const SurfaceCapture(this.observation, this.png, this.error);
  final SurfaceObservation observation;
  final Uint8List? png;
  final String? error;
}
