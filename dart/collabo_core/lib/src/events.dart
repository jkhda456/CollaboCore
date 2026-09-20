import 'dart:convert';
import 'dart:typed_data';

/// Result of a command run in the sandbox.
class ExecResult {
  ExecResult._(this.exitCode, this.signal, this.stdout, this.stderr, this.duration, this.timedOut, this.truncated);

  factory ExecResult.fromJson(Map<String, Object?> json) => ExecResult._(
        json['exitCode'] as int?,
        json['signal'] as int?,
        base64.decode(json['stdoutBase64'] as String),
        base64.decode(json['stderrBase64'] as String),
        Duration(milliseconds: json['durationMs'] as int? ?? 0),
        json['timedOut'] as bool? ?? false,
        json['truncated'] as bool? ?? false,
      );

  /// Exit status, or null when the command was killed by [signal].
  final int? exitCode;
  final int? signal;
  final Uint8List stdout;
  final Uint8List stderr;
  final Duration duration;
  final bool timedOut;

  /// Output beyond 16 MiB per stream was dropped.
  final bool truncated;

  bool get ok => exitCode == 0;
  String get stdoutText => utf8.decode(stdout, allowMalformed: true);
  String get stderrText => utf8.decode(stderr, allowMalformed: true);

  @override
  String toString() => 'ExecResult(exitCode: $exitCode, signal: $signal, ${stdout.length}+${stderr.length} bytes, $duration)';
}

/// Something the sandbox did on the network. Every attempt is reported, allowed or not.
class NetworkEvent {
  NetworkEvent(this.raw);

  /// The runtime's event as sent (see runtime/src/network.mjs for the fields).
  final Map<String, Object?> raw;

  /// "api" (request level: hfetch, python collabo_core) or "net" (sockets through the NIC).
  String get via => raw['via'] as String? ?? '';

  /// "request", "dns" or "connect".
  String get kind => raw['kind'] as String? ?? '';

  /// start / response / failed (requests), open / closed / failed (connections).
  String? get phase => raw['phase'] as String?;
  bool get blocked => raw['blocked'] == true || raw['errorKind'] == 'denied';
  String? get url => raw['url'] as String?;
  String? get host => raw['host'] as String?;
  String? get ip => raw['ip'] as String?;
  int? get port => raw['port'] as int?;
  int? get status => raw['status'] as int?;
  String? get reason => (raw['reason'] ?? raw['error']) as String?;

  @override
  String toString() => 'NetworkEvent($via $kind ${phase ?? ''} ${url ?? host ?? '$ip:$port'}${blocked ? ' BLOCKED' : ''})';
}

/// The sandbox asks to run a host program ([kind] "exec") or open a file/URL ([kind] "open").
class PermissionRequest {
  PermissionRequest(this.raw);
  final Map<String, Object?> raw;

  int get id => raw['requestId'] as int;
  String get kind => raw['kind'] as String;
  List<String> get argv => [for (final a in (raw['argv'] as List? ?? const [])) a as String];
  String? get cwd => raw['cwd'] as String?;
  bool get gui => raw['gui'] == true;
  String? get target => raw['target'] as String?;

  @override
  String toString() => kind == 'open' ? 'open $target' : '${gui ? 'start' : 'run'} ${argv.join(' ')}${cwd != null ? ' in $cwd' : ''}';
}

/// Why the sandbox stopped.
class SandboxExit {
  SandboxExit(this.reason, [this.message]);

  /// "stopped" (normal), "panic" (the guest kernel crashed), "error", or "runtime-exited".
  final String reason;
  final String? message;

  @override
  String toString() => 'SandboxExit($reason${message != null ? ': $message' : ''})';
}

/// An error reported by the runtime. [kind] is machine-readable, e.g. "bad-request", "failed".
class CollaboException implements Exception {
  CollaboException(this.kind, this.message);
  final String kind;
  final String message;

  @override
  String toString() => 'CollaboException($kind): $message';
}

/// A host function call failed in the app; its [kind] and [message] go back to the sandbox.
class HostCallFailure implements Exception {
  HostCallFailure(this.message, {this.kind = 'failed'});
  final String kind;
  final String message;
}
