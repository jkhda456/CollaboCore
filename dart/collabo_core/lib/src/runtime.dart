import 'dart:convert';
import 'dart:ffi' show Abi;
import 'dart:io';

/// The collaboCore runtime folder for this platform: `collabo-core-<platform>/` from
/// `scripts/package-runtime.sh`, holding `bin/`, `app/` and `manifest.json`.
///
/// The manifest's `entry` is the command to run, relative to the folder: the program first,
/// then its own arguments (the engine's images, or the Node runtime's script).
class CollaboRuntime {
  CollaboRuntime(this.directory) {
    final manifestFile = File(_join(directory, 'manifest.json'));
    if (!manifestFile.existsSync()) {
      throw StateError('not a collaboCore runtime (no manifest.json): $directory');
    }
    manifest = jsonDecode(manifestFile.readAsStringSync()) as Map<String, Object?>;
    final entry = (manifest['entry'] as List).cast<String>();
    executable = _join(directory, entry[0]);
    arguments = entry.sublist(1);
    if (!File(executable).existsSync()) throw StateError('runtime is missing ${entry[0]}: $directory');
  }

  final String directory;
  late final Map<String, Object?> manifest;
  late final String executable;

  /// The arguments that come before `--stdio`, as the manifest gives them.
  late final List<String> arguments;

  /// e.g. "darwin-arm64"
  String get platform => manifest['platform'] as String;

  /// This machine's platform key, as used in runtime folder names.
  static String get currentPlatform {
    final abi = Abi.current();
    const names = {
      Abi.macosArm64: 'darwin-arm64',
      Abi.macosX64: 'darwin-x64',
      Abi.windowsX64: 'win-x64',
      Abi.windowsArm64: 'win-arm64',
      Abi.linuxX64: 'linux-x64',
      Abi.linuxArm64: 'linux-arm64',
    };
    final name = names[abi];
    if (name == null) throw UnsupportedError('collaboCore has no runtime for $abi');
    return name;
  }

  /// Finds the runtime. In order:
  ///  1. [directory] if given (the runtime folder itself, or a folder containing
  ///     `collabo-core-<platform>/`); then nothing else is tried,
  ///  2. the `COLLABO_CORE_RUNTIME` environment variable (same meaning),
  ///  3. next to the app: `<exe dir>/collabo_core_runtime` (Windows, Linux) and
  ///     `<App>.app/Contents/Resources/collabo_core_runtime` (macOS), again either the runtime
  ///     itself or a folder with `collabo-core-<platform>/` inside.
  static CollaboRuntime locate({String? directory}) {
    final exeDir = File(Platform.resolvedExecutable).parent.path;
    final candidates = directory != null
        ? [directory]
        : <String>[
            if (Platform.environment['COLLABO_CORE_RUNTIME'] case final env?) env,
            _join(exeDir, 'collabo_core_runtime'),
            if (Platform.isMacOS) _join(File(exeDir).parent.path, 'Resources', 'collabo_core_runtime'),
          ];
    final tried = <String>[];
    for (final base in candidates) {
      for (final dir in [base, _join(base, 'collabo-core-$currentPlatform')]) {
        tried.add(dir);
        if (File(_join(dir, 'manifest.json')).existsSync()) return CollaboRuntime(dir);
      }
    }
    throw StateError('collaboCore runtime for $currentPlatform not found; looked in:\n  ${tried.join('\n  ')}');
  }

  static String _join(String a, [String? b, String? c]) =>
      [a, b, c].whereType<String>().join(Platform.pathSeparator).replaceAll('/', Platform.pathSeparator);
}
