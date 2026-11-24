import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:math';

import 'package:flutter/foundation.dart' show debugPrintSynchronously;

import 'controlled_frames_binding.dart';
import 'observed_surface.dart';

class ClockRequest {
  ClockRequest._(
    this.generation,
    this.id,
    this.operation,
    this.deltaUs,
    this.observationId,
    this.targetId,
    this.start,
    this.end,
    this.text,
    this.direction,
  );
  final String generation, id, operation;
  final int? deltaUs, start, end;
  final String? text, direction;
  final String? observationId, targetId;

  factory ClockRequest.parse(Object? value) {
    if (value is! Map<String, dynamic>) {
      throw const FormatException('Expected object');
    }
    final operation = value['operation'];
    final fields = {
      'generation',
      'id',
      'operation',
      if (operation == 'advance') 'delta_us',
      if (operation == 'tap' ||
          operation == 'scroll' ||
          operation == 'replace_text' ||
          operation == 'capture')
        'observation',
      if (operation == 'tap' ||
          operation == 'scroll' ||
          operation == 'replace_text')
        'target',
      if (operation == 'replace_text') ...['start', 'end', 'text'],
      if (operation == 'scroll') 'direction',
    };
    if (value.length != fields.length || !fields.every(value.containsKey)) {
      throw const FormatException('Unexpected request fields');
    }
    final generation = value['generation'];
    final id = value['id'];
    if (generation is! String ||
        id is! String ||
        !RegExp(r'^[a-z0-9][a-z0-9-]{0,99}$').hasMatch(id) ||
        ![
          'read',
          'hold',
          'advance',
          'stop',
          'observe',
          'tap',
          'scroll',
          'replace_text',
          'capture',
        ].contains(operation)) {
      throw const FormatException('Unsupported request');
    }
    final delta = value['delta_us'];
    if (operation == 'advance' &&
        (delta is! int || delta < 0 || delta > 9007199254740991)) {
      throw const FormatException('Invalid delta_us');
    }
    final observation = value['observation'];
    final target = value['target'];
    if ((operation == 'tap' ||
            operation == 'scroll' ||
            operation == 'replace_text' ||
            operation == 'capture') &&
        (observation is! String ||
            !RegExp(r'^o[0-9]+$').hasMatch(observation))) {
      throw const FormatException('Invalid observation reference');
    }
    if ((operation == 'tap' ||
            operation == 'scroll' ||
            operation == 'replace_text') &&
        (target is! String || !RegExp(r'^s[0-9]+$').hasMatch(target))) {
      throw const FormatException('Invalid target reference');
    }
    final start = value['start'], end = value['end'], text = value['text'];
    if (operation == 'replace_text' &&
        (start is! int ||
            end is! int ||
            start < 0 ||
            start > end ||
            end > 16384 ||
            text is! String ||
            text.length > 16384 ||
            utf8.decode(utf8.encode(text)) != text)) {
      throw const FormatException('Invalid committed text edit');
    }
    final direction = value['direction'];
    if (operation == 'scroll' &&
        !['up', 'down', 'left', 'right'].contains(direction)) {
      throw const FormatException('Unsupported scroll direction');
    }
    return ClockRequest._(
      generation,
      id,
      operation as String,
      delta as int?,
      observation as String?,
      target as String?,
      start as int?,
      end as int?,
      text as String?,
      direction as String?,
    );
  }

  Map<String, Object?> toJson() => {
    'generation': generation,
    'id': id,
    'operation': operation,
    if (deltaUs != null) 'delta_us': deltaUs,
    if (observationId != null) 'observation': observationId,
    if (targetId != null) 'target': targetId,
    if (start != null) 'start': start,
    if (end != null) 'end': end,
    if (text != null) 'text': text,
    if (direction != null) 'direction': direction,
  };
}

class ClockServer {
  ClockServer(this.binding, this.directory, {this.surface});
  final ObservedSurface? surface;
  Future<void> _queue = Future<void>.value();
  final ControlledFramesBinding binding;
  final Directory directory;
  final String generation = _secret();
  final String token = _secret();
  final Completer<void> _stopped = Completer<void>();
  final Map<String, String> _requests = {};
  final Map<String, Map<String, Object?>> _results = {};
  bool _uncertain = false;
  late HttpServer _server;

  static String _secret() {
    final random = Random.secure();
    return List.generate(
      24,
      (_) => random.nextInt(256).toRadixString(16).padLeft(2, '0'),
    ).join();
  }

  Future<void> get stopped => _stopped.future;

  Future<Map<String, Object?>> start() async {
    directory.createSync(recursive: true);
    _server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
    _server.listen(_handle);
    return {'port': _server.port, 'generation': generation, 'token': token};
  }

  void _record(Map<String, Object?> record) {
    final file = File('${directory.path}/requests.jsonl')
        .openSync(mode: FileMode.append);
    try {
      file.writeStringSync('${jsonEncode(record)}\n');
      file.flushSync();
    } finally {
      file.closeSync();
    }
  }

  Future<Map<String, Object?>> execute(ClockRequest request) {
    final result = Completer<Map<String, Object?>>();
    _queue = _queue.then((_) async {
      try {
        result.complete(await _execute(request));
      } catch (error, stack) {
        result.completeError(error, stack);
      }
    });
    return result.future;
  }

  Future<Map<String, Object?>> _execute(ClockRequest request) async {
    if (request.generation != generation) {
      throw const FormatException('Stale generation');
    }
    final canonical = jsonEncode(request.toJson());
    final previous = _requests[request.id];
    if (previous != null) {
      if (previous != canonical) {
        throw const FormatException('Request identity conflict');
      }
      return _results[request.id] ??
          {'status': 'pending', 'id': request.id, 'generation': generation};
    }
    if (_uncertain) throw StateError('Journal uncertain; replace target');
    final time = binding.timeline!;
    if (request.operation == 'advance' &&
        request.deltaUs! > 9007199254740991 - time.elapsed.inMicroseconds) {
      throw const FormatException(
        'Logical instant exceeds exact integer range',
      );
    }
    if (time.faulted &&
        request.operation != 'read' &&
        request.operation != 'stop') {
      throw StateError('Timeline faulted; replace target');
    }
    if ([
          'observe',
          'tap',
          'scroll',
          'replace_text',
          'capture',
        ].contains(request.operation) &&
        surface == null) {
      throw const FormatException('Surface operations unavailable');
    }
    SurfaceObservation? observed;
    try {
      observed = request.observationId == null
          ? null
          : surface!.resolve(request.observationId!);
      if (request.operation == 'tap') {
        surface!.requireTarget(observed!, request.targetId!);
      }
      if (request.operation == 'replace_text') {
        surface!.requireTextEdit(
          observed!,
          request.targetId!,
          request.start!,
          request.end!,
          request.text!,
        );
      }
      if (request.operation == 'scroll') {
        surface!.requireScroll(
          observed!,
          request.targetId!,
          request.direction!,
        );
      }
    } on ObservationChanged catch (change) {
      debugPrintSynchronously(
        'FRONTCAP_FLUTTER_OBSERVATION_CHANGED ${jsonEncode({'generation': generation, 'request_id': request.id, 'operation': request.operation, 'observation': request.observationId, ...change.toJson()})}',
      );
      // This exact latest basis changed after observation, before any action.
      // Bind a durable no-effect result; an identical retry must never act later.
      _requests[request.id] = canonical;
      try {
        _record({'phase': 'prepared', 'request': request.toJson()});
        final result = <String, Object?>{
          'status': 'completed',
          'generation': generation,
          'id': request.id,
          'operation': request.operation,
          'logical_us': time.elapsed.inMicroseconds,
          'held': true,
          'faulted': time.faulted,
          'not_applied': 'observation_changed',
        };
        _record({
          'phase': 'completed',
          'request': request.toJson(),
          'result': result,
        });
        _results[request.id] = result;
        return result;
      } catch (_) {
        _uncertain = true;
        return {
          'status': 'pending',
          'id': request.id,
          'generation': generation,
        };
      }
    }
    _requests[request.id] = canonical;
    try {
      _record({'phase': 'prepared', 'request': request.toJson()});
      final advance = request.operation == 'advance'
          ? binding.advance(Duration(microseconds: request.deltaUs!))
          : null;
      SurfaceObservation? observation;
      SurfaceCapture? capture;
      if (request.operation == 'observe') observation = surface!.observe();
      if (request.operation == 'tap') {
        surface!.tap(observed!, request.targetId!);
      }
      if (request.operation == 'replace_text') {
        surface!.replaceText(
          observed!,
          request.targetId!,
          request.start!,
          request.end!,
          request.text!,
        );
      }
      if (request.operation == 'scroll') {
        surface!.scroll(observed!, request.targetId!, request.direction!);
      }
      if (request.operation == 'capture') {
        capture = await surface!.capture(observed!);
      }
      final result = <String, Object?>{
        'status': 'completed',
        'generation': generation,
        'id': request.id,
        'operation': request.operation,
        'logical_us': time.elapsed.inMicroseconds,
        'held': true,
        'faulted': time.faulted,
        if (advance != null) 'advance': advance.toJson(),
        if (observation != null) 'surface': observation.toJson(),
        if (capture != null)
          'capture': {
            'state': capture.png == null ? 'partial' : 'complete',
            'surface': capture.observation.toJson(),
            'error': capture.error,
            if (capture.png != null) 'png_base64': base64Encode(capture.png!),
            'raster': 'framework-layer-png',
          },
      };
      _record({
        'phase': 'completed',
        'request': request.toJson(),
        'result': result,
      });
      _results[request.id] = result;
      return result;
    } catch (_) {
      // A missing terminal record cannot authorize a second mutation, even if
      // the app already advanced. Reads of this exact identity stay pending.
      _uncertain = true;
      return {'status': 'pending', 'id': request.id, 'generation': generation};
    }
  }

  Future<void> _handle(HttpRequest http) async {
    var stop = false;
    try {
      if (http.method != 'POST' ||
          http.uri.path != '/clock' ||
          http.headers.value(HttpHeaders.authorizationHeader) !=
              'Bearer $token') {
        http.response.statusCode = HttpStatus.forbidden;
        return;
      }
      final bytes = <int>[];
      await for (final chunk in http) {
        if (bytes.length + chunk.length > 128 * 1024) {
          throw const FormatException('Request too large');
        }
        bytes.addAll(chunk);
      }
      final request = ClockRequest.parse(jsonDecode(utf8.decode(bytes)));
      final result = await execute(request);
      stop = request.operation == 'stop' && result['status'] == 'completed';
      http.response.headers.contentType = ContentType.json;
      http.response.write(jsonEncode(result));
    } on FormatException catch (error) {
      http.response.statusCode = HttpStatus.badRequest;
      http.response.write(
        jsonEncode({'status': 'rejected', 'error': error.message}),
      );
    } on StateError catch (error) {
      http.response.statusCode = HttpStatus.conflict;
      http.response.write(
        jsonEncode({'status': 'rejected', 'error': error.message}),
      );
    } finally {
      await http.response.close();
      if (stop && !_stopped.isCompleted) _stopped.complete();
    }
  }

  Future<void> close() async {
    await _server.close(force: true);
  }
}
