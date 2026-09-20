import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'config.dart';
import 'events.dart';
import 'runtime.dart';

typedef HostFunction = Future<Object?> Function(Map<String, Object?> args);
typedef PermissionHandler = Future<bool> Function(PermissionRequest request);

/// A running sandbox: the collaboCore runtime process and the Linux machine inside it.
///
/// The runtime is a child process speaking newline-delimited JSON on stdin/stdout
/// (runtime/src/protocol.mjs). If this app exits, the runtime notices its stdin close and
/// shuts the machine down, so no sandbox outlives its owner.
class CollaboCore {
  CollaboCore._(this._process, this.runtime) {
    _process.stdout.transform(utf8.decoder).transform(const LineSplitter()).listen(_onLine, onDone: _onRuntimeGone);
    _process.stderr.transform(utf8.decoder).listen(_diagnostics.add);
    _process.exitCode.then((code) {
      _exitCode = code;
      _onRuntimeGone();
    });
  }

  final Process _process;
  final CollaboRuntime runtime;
  int _nextId = 1;
  int? _exitCode;
  final _pending = <int, Completer<Map<String, Object?>>>{};
  final _execStreams = <int, void Function(String stream, Uint8List data)>{};
  final _console = StreamController<Uint8List>.broadcast();
  final _network = StreamController<NetworkEvent>.broadcast();
  final _diagnostics = StreamController<String>.broadcast();
  final _exit = Completer<SandboxExit>();
  final _hostFunctions = <String, HostFunction>{};

  /// Decides "ask" permission requests (hostExec: ask). Without a handler they are refused.
  PermissionHandler? onPermission;

  /// Starts a sandbox and returns once its guest agent accepts commands (a few seconds).
  static Future<CollaboCore> start(CollaboConfig config, {CollaboRuntime? runtime, Duration timeout = const Duration(minutes: 2)}) async {
    final rt = runtime ?? CollaboRuntime.locate();
    final process = await Process.start(rt.executable, [...rt.arguments, '--stdio'], workingDirectory: rt.directory);
    final core = CollaboCore._(process, rt);
    try {
      await core._call('start', {'config': config.toJson()}).timeout(timeout);
    } catch (_) {
      process.kill();
      rethrow;
    }
    return core;
  }

  /// Everything the guest console prints (its root shell, and kernel messages while booting).
  Stream<Uint8List> get console => _console.stream;

  /// Every network access attempt of the sandbox.
  Stream<NetworkEvent> get networkEvents => _network.stream;

  /// The runtime's own diagnostics (stderr).
  Stream<String> get diagnostics => _diagnostics.stream;

  /// Completes when the sandbox is gone.
  Future<SandboxExit> get done => _exit.future;

  /// Offers a function to the sandbox. The name must be listed in [CollaboConfig.hostFunctions].
  /// Return a JSON-encodable value; throw [HostCallFailure] to report an error.
  void onHostCall(String name, HostFunction handler) => _hostFunctions[name] = handler;

  /// Runs a program in the sandbox. [argv]\[0] is looked up in the guest's PATH.
  /// [onOutput] receives output as it arrives ("stdout" / "stderr").
  Future<ExecResult> exec(
    List<String> argv, {
    String? cwd,
    Map<String, String>? env,
    List<int>? stdin,
    Duration timeout = const Duration(minutes: 2),
    void Function(String stream, Uint8List data)? onOutput,
  }) =>
      _exec({'argv': argv}, cwd, env, stdin, timeout, onOutput);

  /// Runs a shell command line (`/bin/sh -c`) in the sandbox.
  Future<ExecResult> run(
    String command, {
    String? cwd,
    Map<String, String>? env,
    List<int>? stdin,
    Duration timeout = const Duration(minutes: 2),
    void Function(String stream, Uint8List data)? onOutput,
  }) =>
      _exec({'command': command}, cwd, env, stdin, timeout, onOutput);

  Future<ExecResult> _exec(Map<String, Object?> what, String? cwd, Map<String, String>? env, List<int>? stdin, Duration timeout,
      void Function(String, Uint8List)? onOutput) async {
    final id = _nextId++;
    if (onOutput != null) _execStreams[id] = onOutput;
    try {
      final result = await _callWithId(id, 'exec', {
        ...what,
        if (cwd != null) 'cwd': cwd,
        if (env != null) 'env': env,
        if (stdin != null) 'stdinBase64': base64.encode(stdin),
        'timeoutMs': timeout.inMilliseconds,
        'stream': onOutput != null,
      });
      return ExecResult.fromJson(result);
    } finally {
      _execStreams.remove(id);
    }
  }

  /// Reads a file in the sandbox.
  Future<Uint8List> readFile(String path) async =>
      base64.decode((await _call('readFile', {'path': path}))['dataBase64'] as String);

  Future<String> readText(String path) async => utf8.decode(await readFile(path), allowMalformed: true);

  /// Writes a file in the sandbox, creating its directories. [mode] e.g. 0x1ed (0755).
  Future<void> writeFile(String path, List<int> data, {int? mode}) =>
      _call('writeFile', {'path': path, 'dataBase64': base64.encode(data), if (mode != null) 'mode': mode});

  Future<void> writeText(String path, String text, {int? mode}) => writeFile(path, utf8.encode(text), mode: mode);

  /// Types into the guest console.
  Future<void> writeConsole(Object data) =>
      _call('console.write', {'dataBase64': base64.encode(data is String ? utf8.encode(data) : data as List<int>)});

  Future<void> resizeConsole(int columns, int rows) => _call('console.resize', {'cols': columns, 'rows': rows});

  /// Changes network rules or host access while running; applies to the next request.
  Future<void> updatePolicy({NetworkPolicy? network, HostExecPolicy? hostExec, List<String>? hostFunctions}) =>
      _call('policy.update', {
        if (network != null) 'network': network.toJson(),
        if (hostExec != null) 'hostExec': hostExec.name,
        if (hostFunctions != null) 'hostFunctions': hostFunctions,
      });

  /// Writes a mounted folder ([guestPath] as in [Mount.guestPath]) to [outFile] as a zip
  /// (stored, uncompressed). Returns the number of entries.
  Future<int> exportZip(String guestPath, String outFile) async =>
      (await _call('exportZip', {'guestPath': guestPath, 'outFile': outFile}))['entries'] as int;

  /// Shuts the sandbox down.
  Future<SandboxExit> stop() async {
    if (!_exit.isCompleted) {
      await _call('stop').timeout(const Duration(seconds: 5), onTimeout: () => const {}).catchError((_) => const <String, Object?>{});
    }
    return done.timeout(const Duration(seconds: 10), onTimeout: () {
      _process.kill();
      return SandboxExit('killed');
    });
  }

  // ---- protocol --------------------------------------------------------------------------

  Future<Map<String, Object?>> _call(String method, [Map<String, Object?> params = const {}]) =>
      _callWithId(_nextId++, method, params);

  Future<Map<String, Object?>> _callWithId(int id, String method, Map<String, Object?> params) {
    if (_exit.isCompleted) return Future.error(CollaboException('stopped', 'the sandbox has stopped'));
    final completer = Completer<Map<String, Object?>>();
    _pending[id] = completer;
    _process.stdin.writeln(jsonEncode({'id': id, 'method': method, 'params': params}));
    return completer.future;
  }

  void _onLine(String line) {
    if (line.trim().isEmpty) return;
    final message = jsonDecode(line) as Map<String, Object?>;
    final event = message['event'] as String?;
    if (event == null) {
      final completer = _pending.remove(message['id']);
      if (completer == null) return;
      final error = message['error'] as Map<String, Object?>?;
      if (error != null) {
        completer.completeError(CollaboException(error['kind'] as String? ?? 'failed', error['message'] as String? ?? ''));
      } else {
        completer.complete((message['result'] as Map?)?.cast<String, Object?>() ?? const {});
      }
      return;
    }
    switch (event) {
      case 'console':
        _console.add(base64.decode(message['dataBase64'] as String));
      case 'execOutput':
        _execStreams[message['execId']]?.call(message['stream'] as String, base64.decode(message['dataBase64'] as String));
      case 'network':
        _network.add(NetworkEvent(message));
      case 'hostCall':
        unawaited(_answerHostCall(message));
      case 'permission':
        unawaited(_answerPermission(PermissionRequest(message)));
      case 'exit':
        _finish(SandboxExit(message['reason'] as String? ?? 'stopped', message['message'] as String?));
    }
  }

  Future<void> _answerHostCall(Map<String, Object?> message) async {
    final id = message['callId'] as int;
    final handler = _hostFunctions[message['fn']];
    Map<String, Object?> reply;
    try {
      if (handler == null) throw HostCallFailure('this app did not register "${message['fn']}"', kind: 'unknown-function');
      final result = await handler((message['args'] as Map?)?.cast<String, Object?>() ?? const {});
      reply = {'id': id, 'result': result};
    } on HostCallFailure catch (e) {
      reply = {'id': id, 'error': {'kind': e.kind, 'message': e.message}};
    } catch (e) {
      reply = {'id': id, 'error': {'kind': 'failed', 'message': '$e'}};
    }
    await _call('reply', reply).catchError((_) => const <String, Object?>{});
  }

  Future<void> _answerPermission(PermissionRequest request) async {
    var allow = false;
    try {
      allow = await (onPermission?.call(request) ?? Future.value(false));
    } catch (_) {
      allow = false;
    }
    await _call('reply', {'id': request.id, 'allow': allow}).catchError((_) => const <String, Object?>{});
  }

  void _onRuntimeGone() => _finish(SandboxExit('runtime-exited', _exitCode == null ? null : 'exit code $_exitCode'));

  void _finish(SandboxExit exit) {
    if (_exit.isCompleted) return;
    _exit.complete(exit);
    for (final c in _pending.values) {
      c.completeError(CollaboException('stopped', 'the sandbox stopped (${exit.reason})'));
    }
    _pending.clear();
    _console.close();
    _network.close();
  }
}
