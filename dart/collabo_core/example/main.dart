// A minimal host program: start a sandbox on a local folder, let "an agent" use the tools, watch
// the network, answer a permission request, then download the folder as a zip.
//
//   COLLABO_CORE_RUNTIME=<collaboCore>/dist/runtime dart run example/main.dart [folder]
//
// In a Flutter desktop app the code is the same; ship the runtime folder next to the app
// (readme.detail.md, "Flutter 통합") and CollaboRuntime.locate() finds it.
import 'dart:io';

import 'package:collabo_core/collabo_core.dart';

Future<void> main(List<String> args) async {
  final folder = args.isNotEmpty ? Directory(args.first) : Directory.systemTemp.createTempSync('collabo-example-');
  stdout.writeln('workspace: ${folder.path}');

  final sandbox = await CollaboCore.start(CollaboConfig(
    mounts: [Mount(hostPath: folder.path, guestPath: '/work')],
    network: const NetworkPolicy(allow: ['example.com', 'pypi.org']),
    hostExec: HostExecPolicy.ask,
    hostFunctions: ['app.notify'],
    quiet: true,
  ));
  sandbox.networkEvents.listen((e) => stdout.writeln('  [network] $e'));
  sandbox.onHostCall('app.notify', (args) async {
    stdout.writeln('  [app.notify] ${args['message']}');
    return {'shown': true};
  });
  sandbox.onPermission = (request) async {
    stdout.writeln('  [permission] the sandbox wants to $request -> allowed for "echo" only');
    return request.argv.isNotEmpty && request.argv.first == 'echo';
  };

  // What an LLM would do through tool calls:
  final tools = SandboxTools(sandbox, workdir: '/work');
  stdout.writeln(await tools.call('write_file', {
    'path': 'report.py',
    'content': '''
import collabo_core, platform
r = collabo_core.get("https://example.com/")
print("fetched", r.status, len(r.read()), "bytes on", platform.machine())
collabo_core.host.call("app.notify", message="report finished")
# "echo" is a program on macOS and Linux; on Windows it is a cmd.exe builtin: ["cmd", "/c", "echo", ...]
print(collabo_core.host.exec(["echo", "hello from the host"]).stdout.strip())
''',
  }));
  stdout.writeln(await tools.call('run_command', {'command': 'python3 report.py'}));
  stdout.writeln(await tools.call('run_command', {'command': 'hfetch https://not-allowed.test/'}));
  stdout.writeln(await tools.call('list_directory', {}));

  final zip = '${folder.path}.zip';
  stdout.writeln('zip: ${await sandbox.exportZip('/work', zip)} entries -> $zip');
  stdout.writeln('stopped: ${await sandbox.stop()}');
}
