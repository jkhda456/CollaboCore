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
