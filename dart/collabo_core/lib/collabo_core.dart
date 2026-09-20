/// collaboCore: an isolated Linux sandbox for AI agents, for Flutter desktop apps
/// (macOS, Windows, Linux) and plain Dart programs.
///
/// ```dart
/// final sandbox = await CollaboCore.start(CollaboConfig(
///   mounts: [Mount(hostPath: projectDir, guestPath: '/work')],
///   network: NetworkPolicy(allow: ['api.anthropic.com'],
///       secrets: [Secret(host: 'api.anthropic.com', header: 'x-api-key', value: apiKey)]),
/// ));
/// final result = await sandbox.run('python3 main.py', cwd: '/work');
/// print(result.stdoutText);
/// await sandbox.stop();
/// ```
library;

export 'src/config.dart';
export 'src/events.dart';
export 'src/runtime.dart';
export 'src/sandbox.dart';
export 'src/tools.dart';
