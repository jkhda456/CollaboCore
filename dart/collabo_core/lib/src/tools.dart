import 'dart:convert';

import 'sandbox.dart';

/// Tools that let an LLM agent work inside a sandbox: definitions in the Anthropic Messages API
/// tool format (`name`, `description`, `input_schema`) and a dispatcher that runs them.
///
/// ```dart
/// final tools = SandboxTools(sandbox, workdir: '/work');
/// // request body: {"tools": tools.definitions, ...}
/// // for each tool_use block in the response:
/// final text = await tools.call(block['name'], block['input']);  // -> tool_result content
/// ```
class SandboxTools {
  SandboxTools(this.sandbox, {this.workdir = '/work', this.maxOutputChars = 30000, this.commandTimeout = const Duration(minutes: 2)});

  final CollaboCore sandbox;

  /// Default working directory for commands (usually a mounted folder).
  final String workdir;

  /// Longer output is cut in the middle, so the model sees both ends.
  final int maxOutputChars;
  final Duration commandTimeout;

  List<Map<String, Object?>> get definitions => [
        {
          'name': 'run_command',
          'description': 'Run a shell command (/bin/sh) in an isolated Linux sandbox and return its exit code, stdout and '
              'stderr. Python 3.13 (python3, pip), BusyBox tools, curl, git and ssh are available. Network access goes '
              'through the app\'s policy: hosts it does not allow fail (or wait for the user\'s answer). API keys the app '
              'holds are added by the host, so do not look for them: use `hfetch URL`, python `collabo_core`, or plain '
              'curl/git/requests to those hosts. '
              'The working directory $workdir is a folder shared with the user.',
          'input_schema': {
            'type': 'object',
            'properties': {
              'command': {'type': 'string', 'description': 'The command line to run.'},
              'cwd': {'type': 'string', 'description': 'Working directory (default $workdir).'},
              'timeout_seconds': {'type': 'integer', 'description': 'Kill the command after this many seconds (default ${commandTimeout.inSeconds}).'},
            },
            'required': ['command'],
          },
        },
        {
          'name': 'read_file',
          'description': 'Read a text file in the sandbox.',
          'input_schema': {
            'type': 'object',
            'properties': {'path': {'type': 'string', 'description': 'Absolute path, or relative to $workdir.'}},
            'required': ['path'],
          },
        },
        {
          'name': 'write_file',
          'description': 'Create or replace a text file in the sandbox (parent directories are created).',
          'input_schema': {
            'type': 'object',
            'properties': {
              'path': {'type': 'string', 'description': 'Absolute path, or relative to $workdir.'},
              'content': {'type': 'string', 'description': 'The complete new file content.'},
            },
            'required': ['path', 'content'],
          },
        },
        {
          'name': 'list_directory',
          'description': 'List a directory in the sandbox (names, sizes, types).',
          'input_schema': {
            'type': 'object',
            'properties': {'path': {'type': 'string', 'description': 'Directory (default $workdir).'}},
          },
        },
      ];

  /// Runs one tool call and returns the text for the tool_result block. Failures are returned as
  /// text too (starting with "Error:"), so the model can react to them.
  Future<String> call(String name, Map<String, Object?> input) async {
    try {
      switch (name) {
        case 'run_command':
          final seconds = input['timeout_seconds'];
          final r = await sandbox.run(input['command'] as String,
              cwd: (input['cwd'] as String?) ?? workdir,
              timeout: seconds is int ? Duration(seconds: seconds) : commandTimeout);
          final parts = <String>[
            r.timedOut ? 'timed out (killed)' : 'exit code: ${r.exitCode ?? 'signal ${r.signal}'}',
            if (r.stdout.isNotEmpty) 'stdout:\n${_cut(r.stdoutText)}',
            if (r.stderr.isNotEmpty) 'stderr:\n${_cut(r.stderrText)}',
          ];
          return parts.join('\n');
        case 'read_file':
          return _cut(await sandbox.readText(_path(input['path'])));
        case 'write_file':
          final content = input['content'] as String;
          await sandbox.writeText(_path(input['path']), content);
          return 'wrote ${utf8.encode(content).length} bytes to ${_path(input['path'])}';
        case 'list_directory':
          final r = await sandbox.exec(['ls', '-la', _path(input['path'] ?? '.')]);
          return r.ok ? _cut(r.stdoutText) : 'Error: ${r.stderrText.trim()}';
        default:
          return 'Error: unknown tool "$name"';
      }
    } catch (e) {
      return 'Error: $e';
    }
  }

  String _path(Object? p) {
    final path = (p as String?) ?? '.';
    return path.startsWith('/') ? path : '$workdir/$path';
  }

  String _cut(String text) {
    if (text.length <= maxOutputChars) return text;
    final half = maxOutputChars ~/ 2;
    return '${text.substring(0, half)}\n… [${text.length - maxOutputChars} characters omitted] …\n${text.substring(text.length - half)}';
  }
}

/// Tools for GUI programs in the sandbox (the `gui` command of the tools image, [GuiSettings]): start one, look at
/// its window, click, type, use the clipboard. Every window is a whole canvas of its own, named
/// by its program (`demo`, or `demo:2` for its second window), so there is nothing to find or
/// uncover; screenshots come back as image blocks.
///
/// ```dart
/// final gui = GuiTools(sandbox);
/// // request body: {"tools": [...SandboxTools(sandbox).definitions, ...gui.definitions]}
/// // for each tool_use block whose name gui.handles(name):
/// final content = await gui.callContent(block['name'], block['input']);  // tool_result content blocks
/// ```
///
/// Screenshots wider than [maxImageWidth] are scaled down; the coordinates the model then gives
/// [gui_input] are taken in that screenshot's pixels and scaled back for it.
class GuiTools {
  GuiTools(this.sandbox, {this.workdir = '/work', this.maxImageWidth = 1280, this.settle = const Duration(milliseconds: 200)});

  final CollaboCore sandbox;

  /// Where programs started with gui_run run.
  final String workdir;
  final int maxImageWidth;

  /// After input, the screenshot waits until the window has not changed for this long (at most
  /// a few seconds), so it shows what the input did.
  final Duration settle;

  /// Canvas pixels per screenshot pixel, per target, from the last screenshot.
  final _scale = <String, double>{};

  static const _names = {'gui_run', 'gui_list', 'gui_screenshot', 'gui_input', 'gui_clipboard', 'gui_window'};

  bool handles(String name) => _names.contains(name);

  static const _target = {
    'type': 'string',
    'description': 'The program\'s name as gui_list shows it (its first window), or NAME:N for its window N.',
  };

  List<Map<String, Object?>> get definitions => [
        {
          'name': 'gui_run',
          'description': 'Start a GUI program in the sandbox and wait until its window has drawn. Windows are headless: '
              'nothing is visible until you take a screenshot. Each window is a whole canvas of its own (no positions, '
              'nothing overlaps). gui-demo is a small test program.',
          'input_schema': {
            'type': 'object',
            'properties': {
              'command': {'type': 'string', 'description': 'The command line (run by /bin/sh in $workdir).'},
              'name': {'type': 'string', 'description': 'Its name for the other gui tools (default: the program\'s name).'},
              'size': {'type': 'string', 'description': 'Window size WIDTHxHEIGHT (default and maximum: gui settings).'},
            },
            'required': ['command'],
          },
        },
        {
          'name': 'gui_list',
          'description': 'List the GUI programs and their windows: name, pid, state, size, frames drawn, focus (*), title.',
          'input_schema': {'type': 'object', 'properties': <String, Object?>{}},
        },
        {
          'name': 'gui_screenshot',
          'description': 'Look at a window: an image of all of it (never covered). Wide windows come scaled down; give '
              'gui_input coordinates in the pixels of the image you last saw.',
          'input_schema': {
            'type': 'object',
            'properties': {
              'target': _target,
              'region': {'type': 'string', 'description': 'Only this part, X,Y,WIDTH,HEIGHT in window pixels.'},
            },
            'required': ['target'],
          },
        },
        {
          'name': 'gui_input',
          'description': 'Mouse and keyboard input to a window (it gets the focus), then a screenshot of the result. '
              'Coordinates are in the pixels of your last screenshot of that window. key_down/key_up hold keys across '
              'calls (e.g. shift for a range selection); release lets go of everything held; leave moves the pointer '
              'out of the window (hover ends).',
          'input_schema': {
            'type': 'object',
            'properties': {
              'target': _target,
              'action': {
                'type': 'string',
                'enum': [
                  'click', 'double_click', 'right_click', 'middle_click', 'move', 'drag', 'scroll', 'mouse_down', 'mouse_up',
                  'leave', 'key', 'key_down', 'key_up', 'release', 'type',
                ],
              },
              'modifiers': {
                'type': 'string',
                'description': 'Mouse actions: modifier keys held around it, e.g. "ctrl" (ctrl+click), "shift", "ctrl+shift".',
              },
              'x': {'type': 'integer'},
              'y': {'type': 'integer'},
              'to_x': {'type': 'integer', 'description': 'drag: where to'},
              'to_y': {'type': 'integer', 'description': 'drag: where to'},
              'amount': {'type': 'integer', 'description': 'scroll: wheel notches, positive is down (default 3)'},
              'keys': {
                'type': 'string',
                'description': 'key: key combinations in order, separated by spaces, e.g. "ctrl+a Delete", "Return", '
                    '"shift+Tab", "alt+F4", "ctrl+shift+t"',
              },
              'text': {'type': 'string', 'description': 'type: the text to type (any language; \\n is Return)'},
              'ime': {
                'type': 'boolean',
                'description': 'type: compose Hangul as a Korean input method does (the composition, then the commit), for '
                    'text fields that need it; default: each character as a key',
              },
              'screenshot': {'type': 'boolean', 'description': 'Return a screenshot afterwards (default true).'},
            },
            'required': ['target', 'action'],
          },
        },
        {
          'name': 'gui_clipboard',
          'description': 'Read or set the clipboard the GUI programs share (text).',
          'input_schema': {
            'type': 'object',
            'properties': {
              'action': {'type': 'string', 'enum': ['get', 'set']},
              'text': {'type': 'string', 'description': 'set: the text'},
            },
            'required': ['action'],
          },
        },
        {
          'name': 'gui_window',
          'description': 'Manage a program or window: close (as its close button), kill (end the program), focus, '
              'resize (to size WIDTHxHEIGHT), logs (what the program printed).',
          'input_schema': {
            'type': 'object',
            'properties': {
              'target': _target,
              'action': {'type': 'string', 'enum': ['close', 'kill', 'focus', 'resize', 'logs']},
              'size': {'type': 'string', 'description': 'resize: WIDTHxHEIGHT'},
            },
            'required': ['target', 'action'],
          },
        },
      ];

  /// Runs one tool call; returns the tool_result content blocks (text, and images for
  /// screenshots). Failures come back as text starting with "Error:".
  Future<List<Map<String, Object?>>> callContent(String name, Map<String, Object?> input) async {
    try {
      switch (name) {
        case 'gui_run':
          final command = input['command'] as String;
          final given = input['name'] as String?;
          final first = command.trim().split(RegExp(r'\s+')).first.split('/').last;
          final appName = given ?? (first.isEmpty ? 'app' : first);
          return [
            _text(await _gui(['run', '--wait', '--name', _cleanName(appName), if (input['size'] != null) ...['--size', '${input['size']}'],
              '--', '/bin/sh', '-c', 'exec $command'], cwd: workdir, timeout: const Duration(seconds: 60)))
          ];
        case 'gui_list':
          return [_text(await _gui(['list']))];
        case 'gui_screenshot':
          return await _screenshot(input['target'] as String, region: input['region'] as String?);
        case 'gui_input':
          return await _input(input);
        case 'gui_clipboard':
          if (input['action'] == 'set') {
            final r = await sandbox.exec(['gui', 'clipboard', 'set'], stdin: utf8.encode((input['text'] as String?) ?? ''));
            return [_text(r.ok ? 'clipboard set' : 'Error: ${r.stderrText.trim()}')];
          }
          final r = await sandbox.exec(['gui', 'clipboard', 'get']);
          return [_text(r.ok ? r.stdoutText : 'Error: ${r.stderrText.trim()}')];
        case 'gui_window':
          final target = input['target'] as String;
          switch (input['action']) {
            case 'resize':
              return [_text(await _gui(['resize', target, '${input['size']}', '--wait']))];
            case 'logs':
              return [_text(_cut(await _gui(['logs', target.split(':').first, '-n', '200'])))];
            case final String action when ['close', 'kill', 'focus'].contains(action):
              final out = await _gui([action, target]);
              return [_text(out.isEmpty ? 'done' : out)];
            default:
              return [_text('Error: unknown action ${input['action']}')];
          }
        default:
          return [_text('Error: unknown tool "$name"')];
      }
    } on _GuiError catch (e) {
      return [_text('Error: ${e.message}')];
    } catch (e) {
      return [_text('Error: $e')];
    }
  }

  /// [callContent] as text only (an image becomes a line saying it was taken), for clients
  /// that pass tool results as strings.
  Future<String> call(String name, Map<String, Object?> input) async {
    final blocks = await callContent(name, input);
    return blocks.map((b) => b['type'] == 'text' ? b['text'] as String : '[screenshot image]').join('\n');
  }

  Map<String, Object?> _text(String t) => {'type': 'text', 'text': t.isEmpty ? '(no output)' : t};

  static String _cleanName(String raw) {
    var s = raw.replaceAll(RegExp(r'[^A-Za-z0-9._-]'), '');
    if (s.length > 32) s = s.substring(0, 32);
    return RegExp(r'^[A-Za-z]').hasMatch(s) ? s : 'app$s';
  }

  String _cut(String text, [int max = 20000]) =>
      text.length <= max ? text : '… [${text.length - max} characters omitted] …\n${text.substring(text.length - max)}';

  Future<String> _gui(List<String> args, {String? cwd, Duration timeout = const Duration(seconds: 30)}) async {
    final r = await sandbox.exec(['gui', ...args], cwd: cwd, timeout: timeout);
    if (!r.ok) {
      final err = r.stderrText.trim();
      throw _GuiError(err.startsWith('gui: ') ? err.substring(5) : (err.isEmpty ? 'gui ${args.first} failed (${r.exitCode})' : err));
    }
    return r.stdoutText.trimRight();
  }

  Future<List<Map<String, Object?>>> _screenshot(String target, {String? region, Duration? settleFirst}) async {
    final r = await sandbox.exec([
      'gui', 'screenshot', target, '-o', '-', '--max-width', '$maxImageWidth',
      if (region != null) ...['--region', region],
      if (settleFirst != null) ...['--idle', '${settleFirst.inMilliseconds}', '--timeout', '3'],
    ]);
    if (!r.ok) throw _GuiError(r.stderrText.trim().replaceFirst('gui: ', ''));
    // stderr: "NAME:N WxH (BYTES bytes png)"; the full size comes from gui info.
    final m = RegExp(r'^(\S+) (\d+)x(\d+)').firstMatch(r.stderrText.trim());
    var note = r.stderrText.trim();
    if (m != null && region == null) {
      final info = jsonDecode(await _gui(['info', target])) as Map<String, Object?>;
      final canvas = info['canvas'] as Map<String, Object?>?;
      final fullW = (canvas?['width'] as int?) ?? int.parse(m.group(2)!);
      final shownW = int.parse(m.group(2)!);
      final scale = fullW / shownW;
      _scale[target] = scale;
      note = '${m.group(1)}: ${canvas?['width']}x${canvas?['height']}'
          '${scale > 1.001 ? ', shown at ${m.group(2)}x${m.group(3)} (coordinates in this image are scaled back)' : ''}'
          '${canvas?['focused'] == true ? ', focused' : ''}; title: ${canvas?['title']}';
    }
    return [
      {
        'type': 'image',
        'source': {'type': 'base64', 'media_type': 'image/png', 'data': base64.encode(r.stdout)},
      },
      _text(note),
    ];
  }

  Future<List<Map<String, Object?>>> _input(Map<String, Object?> input) async {
    final target = input['target'] as String;
    final scale = _scale[target] ?? 1.0;
    String c(Object? v, String what) {
      if (v is! num) throw _GuiError('$what is needed');
      return '${(v * scale).round()}';
    }

    final at = input['x'] != null ? [c(input['x'], 'x'), c(input['y'], 'y')] : <String>[];
    final args = switch (input['action']) {
      'click' => ['click', target, ...at],
      'double_click' => ['click', target, ...at, '--double'],
      'right_click' => ['click', target, ...at, '--button', 'right'],
      'middle_click' => ['click', target, ...at, '--button', 'middle'],
      'move' => ['move', target, c(input['x'], 'x'), c(input['y'], 'y')],
      'drag' => ['drag', target, c(input['x'], 'x'), c(input['y'], 'y'), c(input['to_x'], 'to_x'), c(input['to_y'], 'to_y')],
      'scroll' => ['scroll', target, if (at.isNotEmpty) ...['--at', at[0], at[1]], '${input['amount'] ?? 3}'],
      'mouse_down' => ['mousedown', target, ...at],
      'mouse_up' => ['mouseup', target, ...at],
      'leave' => ['leave', target],
      'key' || 'key_down' || 'key_up' => [
          (input['action'] as String).replaceAll('_', ''),
          target,
          ...((input['keys'] as String?) ?? '').trim().split(RegExp(r'\s+')).where((k) => k.isNotEmpty)
        ],
      'release' => ['release', target],
      'type' => ['type', target, if (input['ime'] == true) '--ime', '--', (input['text'] as String?) ?? ''],
      _ => throw _GuiError('unknown action ${input['action']}'),
    };
    final mods = input['modifiers'] as String?;
    if (mods != null && mods.isNotEmpty && !['key', 'key_down', 'key_up', 'type', 'release'].contains(input['action'])) {
      args.addAll(['--mods', mods]);
    }
    await _gui(args);
    if (input['screenshot'] == false) return [_text('done')];
    return _screenshot(target, settleFirst: settle);
  }
}

class _GuiError implements Exception {
  _GuiError(this.message);
  final String message;
}
