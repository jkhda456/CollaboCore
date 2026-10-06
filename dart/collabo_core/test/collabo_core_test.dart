// Runs against a real packaged runtime. Point COLLABO_CORE_RUNTIME at dist/runtime (or at a
// collabo-core-<platform> folder):  COLLABO_CORE_RUNTIME=../../dist/runtime dart test
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:collabo_core/collabo_core.dart';
import 'package:test/test.dart';

void main() {
  late Directory tmp;
  late Directory work;
  late CollaboCore sandbox;
  final permissions = <PermissionRequest>[];
  final network = <NetworkEvent>[];

  setUpAll(() async {
    tmp = Directory.systemTemp.createTempSync('collabo-dart-');
    work = Directory('${tmp.path}/work')..createSync();
    File('${work.path}/input.txt').writeAsStringSync('from the host\n');
    sandbox = await CollaboCore.start(CollaboConfig(
      cpus: 2,
      quiet: true,
      mounts: [Mount(hostPath: work.path, guestPath: '/work')],
      network: const NetworkPolicy(allow: ['example.com'], deny: ['evil.example.com']),
      hostExec: HostExecPolicy.ask,
      hostFunctions: ['app.version', 'app.broken'],
    ));
    sandbox.networkEvents.listen(network.add);
    sandbox.onHostCall('app.version', (args) async => {'version': '1.2.3', 'asked': args['who']});
    sandbox.onHostCall('app.broken', (args) async => throw HostCallFailure('not today'));
    // The host program used in the tests is the Dart binary running them: it exists on every
    // platform (there is no `echo` executable on Windows). Allowed only when asked for its
    // version, which is how the two cases below tell themselves apart.
    sandbox.onPermission = (request) async {
      permissions.add(request);
      return request.argv.contains('--version');
    };
  });

  tearDownAll(() async {
    await sandbox.stop();
    tmp.deleteSync(recursive: true);
  });

  test('runtime is located for this platform', () {
    expect(sandbox.runtime.platform, CollaboRuntime.currentPlatform);
    expect(() => CollaboRuntime.locate(directory: '/nonexistent'), throwsA(isA<StateError>()));
  });

  group('exec', () {
    test('run: shell command, exit code, stdout, stderr', () async {
      final r = await sandbox.run('echo hello; echo oops >&2; exit 5');
      expect(r.exitCode, 5);
      expect(r.stdoutText, 'hello\n');
      expect(r.stderrText, 'oops\n');
      expect(r.ok, isFalse);
    });

    test('exec: argv, cwd, env, binary stdin', () async {
      final data = Uint8List.fromList(List.generate(5000, (i) => i % 256));
      final r = await sandbox.exec(['sh', '-c', 'pwd; echo \$GREETING; wc -c'], cwd: '/work', env: {'GREETING': 'hi'}, stdin: data);
      expect(r.stdoutText, '/work\nhi\n5000\n');
    });

    test('streaming output with onOutput', () async {
      final chunks = <String>[];
      final r = await sandbox.run('for i in 1 2 3; do echo tick\$i; sleep 0.2; done',
          onOutput: (stream, data) => chunks.add('$stream:${utf8.decode(data)}'));
      expect(chunks.join(), contains('stdout:tick1'));
      expect(chunks.length, greaterThanOrEqualTo(2));
      expect(r.stdoutText, 'tick1\ntick2\ntick3\n');
    });

    test('timeout kills the command', () async {
      final r = await sandbox.run('sleep 20', timeout: const Duration(milliseconds: 600));
      expect(r.timedOut, isTrue);
      expect(r.exitCode, isNull);
    });

    test('python in the sandbox', () async {
      final r = await sandbox.exec(['python3', '-c', 'import json; print(json.dumps({"n": 6*7}))']);
      expect(jsonDecode(r.stdoutText), {'n': 42});
    });
  });

  group('files', () {
    test('writeFile / readFile / writeText / readText', () async {
      await sandbox.writeText('/tmp/d/notes.txt', 'héllo 한글\n');
      expect(await sandbox.readText('/tmp/d/notes.txt'), 'héllo 한글\n');
      final bin = Uint8List.fromList([0, 255, 7, 10, 13]);
      await sandbox.writeFile('/tmp/d/x.bin', bin, mode: 0x1ed);
      expect(await sandbox.readFile('/tmp/d/x.bin'), bin);
      expect((await sandbox.run('stat -c %a /tmp/d/x.bin')).stdoutText.trim(), '755');
      expect(sandbox.readFile('/missing'), throwsA(isA<CollaboException>()));
    });

    test('the mounted folder is shared both ways', () async {
      expect(await sandbox.readText('/work/input.txt'), 'from the host\n');
      await sandbox.writeText('/work/result/answer.txt', '42\n');
      expect(File('${work.path}/result/answer.txt').readAsStringSync(), '42\n');
    });

    test('exportZip writes the folder as a zip', () async {
      final out = '${tmp.path}/work.zip';
      final entries = await sandbox.exportZip('/work', out);
      expect(entries, greaterThanOrEqualTo(3));
      final bytes = File(out).readAsBytesSync();
      expect(bytes.sublist(0, 4), [0x50, 0x4b, 0x03, 0x04]);
    });
  });

  group('network', () {
    test('refused hosts are reported as events, with the reason', () async {
      final r = await sandbox.run('hfetch https://not-allowed.example.org/');
      expect(r.exitCode, 2);
      expect(r.stderrText, contains('not in the allow list'));
      await Future<void>.delayed(const Duration(milliseconds: 100));
      expect(network.where((e) => e.blocked && e.url == 'https://not-allowed.example.org/'), isNotEmpty);
    });

    test('updatePolicy takes effect for the next request', () async {
      await sandbox.updatePolicy(network: const NetworkPolicy(allow: ['*'], deny: ['not-allowed.example.org']));
      final r = await sandbox.run('hfetch https://not-allowed.example.org/');
      expect(r.stderrText, contains('matches the deny rule'));
      await sandbox.updatePolicy(network: const NetworkPolicy(allow: ['example.com'], deny: ['evil.example.com']));
    });
  });

  group('host access', () {
    test('app functions: result and failure', () async {
      final ok = await sandbox.run("hostcall app.version '{\"who\":\"agent\"}'");
      expect(jsonDecode(ok.stdoutText), {'version': '1.2.3', 'asked': 'agent'});
      final bad = await sandbox.run('hostcall app.broken');
      expect(bad.exitCode, 1);
      expect(bad.stderrText, contains('failed: not today'));
    });

    test('host programs go through onPermission: allowed and refused', () async {
      final program = Platform.resolvedExecutable;
      final ok = await sandbox.exec(['hostcall', '--exec', '--', program, '--version']);
      expect(ok.exitCode, 0);
      expect('${ok.stdoutText}${ok.stderrText}', contains('Dart SDK version'));
      final no = await sandbox.exec(['hostcall', '--exec', '--', program, 'help']);
      expect(no.exitCode, 1);
      expect(no.stderrText, contains('denied'));
      expect(permissions.map((p) => p.argv.first).toSet(), {program});
      expect(permissions.first.kind, 'exec');
    });
  });

  group('agent tools', () {
    test('definitions are Anthropic tool schemas', () {
      final tools = SandboxTools(sandbox);
      final names = tools.definitions.map((t) => t['name']).toList();
      expect(names, ['run_command', 'read_file', 'write_file', 'list_directory']);
      for (final t in tools.definitions) {
        expect(t['description'], isA<String>());
        expect((t['input_schema'] as Map)['type'], 'object');
      }
    });

    test('a small agent loop: write, run, read, list', () async {
      final tools = SandboxTools(sandbox, workdir: '/work');
      expect(await tools.call('write_file', {'path': 'hello.py', 'content': 'print("hi from", __name__)\n'}), contains('wrote'));
      final run = await tools.call('run_command', {'command': 'python3 hello.py > out.txt; cat out.txt'});
      expect(run, contains('exit code: 0'));
      expect(run, contains('hi from __main__'));
      expect(await tools.call('read_file', {'path': 'out.txt'}), 'hi from __main__\n');
      expect(await tools.call('list_directory', {}), contains('hello.py'));
      expect(File('${work.path}/hello.py').existsSync(), isTrue, reason: 'the agent worked in the shared folder');
      expect(await tools.call('read_file', {'path': 'missing.txt'}), startsWith('Error:'));
      expect(await tools.call('nope', {}), startsWith('Error: unknown tool'));
    });

    test('long output is cut in the middle', () async {
      final tools = SandboxTools(sandbox, maxOutputChars: 200);
      final out = await tools.call('run_command', {'command': 'seq 1 5000', 'cwd': '/'});
      expect(out, contains('characters omitted'));
      expect(out, contains('\n1\n'));
      expect(out.trim(), endsWith('5000'));
    });
  });

  group('gui tools', () {
    late CollaboCore gs;
    setUpAll(() async {
      gs = await CollaboCore.start(CollaboConfig(
        cpus: 2,
        quiet: true,
        python: false,
        networkEnabled: false,
        gui: const GuiSettings(maxSize: '800x600'),
      ));
    });
    tearDownAll(() => gs.stop());

    int pngWidth(Map<String, Object?> block) {
      final png = base64.decode((block['source'] as Map)['data'] as String);
      expect(png.sublist(1, 4), 'PNG'.codeUnits);
      return ByteData.sublistView(png, 16, 20).getUint32(0);
    }

    test('start a program, look at it, click and type, clipboard, resize, close', () async {
      final gui = GuiTools(gs, workdir: '/', maxImageWidth: 320);
      expect(gui.definitions.map((t) => t['name']), ['gui_run', 'gui_list', 'gui_screenshot', 'gui_input', 'gui_clipboard', 'gui_window']);
      expect(gui.handles('gui_run'), isTrue);
      expect(gui.handles('run_command'), isFalse);
      expect(await gui.call('gui_run', {'command': 'gui-demo --title Dart', 'name': 'demo', 'size': '640x400'}), contains('demo is up'));
      expect(await gui.call('gui_list', {}), contains('demo:1'));

      final shot = await gui.callContent('gui_screenshot', {'target': 'demo'});
      expect(shot.first['type'], 'image');
      expect(pngWidth(shot.first), 320, reason: 'scaled to maxImageWidth');
      expect(shot.last['text'], allOf(contains('640x400'), contains('shown at 320x200'), contains('title: Dart')));

      // The button is at (100, 60) on the canvas: (50, 30) in the half-size screenshot.
      final after = await gui.callContent('gui_input', {'target': 'demo', 'action': 'click', 'x': 50, 'y': 30});
      expect(after.first['type'], 'image');
      final logs = await gui.call('gui_window', {'target': 'demo', 'action': 'logs'});
      expect(logs, allOf(contains('button 1 down 100,60'), contains('clicked 1')));

      // ctrl+click (the demo counts it as ten), keys held across calls, letting go of them.
      await gui.call('gui_input', {'target': 'demo', 'action': 'click', 'x': 50, 'y': 30, 'modifiers': 'ctrl', 'screenshot': false});
      await gui.call('gui_input', {'target': 'demo', 'action': 'key_down', 'keys': 'shift', 'screenshot': false});
      await gui.call('gui_input', {'target': 'demo', 'action': 'mouse_down', 'x': 40, 'y': 150, 'screenshot': false});
      expect(await gui.call('gui_window', {'target': 'demo', 'action': 'logs'}), allOf(contains('clicked 11'), contains('button 1 down 80,300 mods=0x1')));
      await gui.call('gui_input', {'target': 'demo', 'action': 'release', 'screenshot': false});
      final info = (await gs.exec(['gui', 'info', 'demo'])).stdoutText;
      expect(info, contains('"buttons":0,"modifiers":0'));
      // Hangul through a Korean input method's composition.
      await gui.call('gui_input', {'target': 'demo', 'action': 'type', 'text': '한', 'ime': true, 'screenshot': false});
      expect(await gui.call('gui_window', {'target': 'demo', 'action': 'logs'}), contains('preedit "하" 3 3\ntext "한"'));
      await gui.call('gui_input', {'target': 'demo', 'action': 'key', 'keys': 'ctrl+l', 'screenshot': false});
      expect(await gui.call('gui_input', {'target': 'demo', 'action': 'type', 'text': 'hi 안녕', 'screenshot': false}), 'done');
      await gui.call('gui_input', {'target': 'demo', 'action': 'key', 'keys': 'ctrl+c', 'screenshot': false});
      expect(await gui.call('gui_clipboard', {'action': 'get'}), 'hi 안녕');
      expect(await gui.call('gui_clipboard', {'action': 'set', 'text': 'from dart'}), 'clipboard set');
      // Pasting asks the clipboard and the answer comes later, as in any toolkit: Return after.
      await gui.call('gui_input', {'target': 'demo', 'action': 'key', 'keys': 'ctrl+l ctrl+v', 'screenshot': false});
      await gui.call('gui_input', {'target': 'demo', 'action': 'key', 'keys': 'Return', 'screenshot': false});
      expect(await gui.call('gui_window', {'target': 'demo', 'action': 'logs'}), contains('enter "from dart"'));

      expect(await gui.call('gui_window', {'target': 'demo', 'action': 'resize', 'size': '9999x9999'}), '800x600', reason: 'held to maxSize');
      expect(await gui.call('gui_screenshot', {'target': 'nosuch'}), startsWith('Error: no program named nosuch'));
      expect(await gui.call('gui_input', {'target': 'demo', 'action': 'drag', 'x': 1}), startsWith('Error: y is needed'));
      await gui.call('gui_window', {'target': 'demo', 'action': 'close'});
      expect((await gs.exec(['gui', 'wait', 'demo', '--exit'])).stdoutText.trim(), 'exited(0)');
    });
  });

  group('console and shutdown', () {
    test('console is the guest shell', () async {
      final text = StringBuffer();
      final sub = sandbox.console.listen((d) => text.write(utf8.decode(d, allowMalformed: true)));
      await sandbox.writeConsole('echo from-console-\$((40+2))\n');
      for (var i = 0; i < 50 && !text.toString().contains('from-console-42'); i++) {
        await Future<void>.delayed(const Duration(milliseconds: 100));
      }
      await sub.cancel();
      expect(text.toString(), contains('from-console-42'));
    });

    test('a second sandbox starts and stops independently', () async {
      final other = await CollaboCore.start(const CollaboConfig(cpus: 1, python: false, networkEnabled: false, quiet: true));
      expect((await other.run('uname -n')).stdoutText.trim(), 'collabo');
      expect((await other.run('hfetch https://example.com/')).exitCode, isNot(0), reason: 'no network in this one');
      final exit = await other.stop();
      expect(exit.reason, 'stopped');
      expect(other.run('true'), throwsA(isA<CollaboException>()));
      expect((await sandbox.run('echo still-alive')).stdoutText, 'still-alive\n');
    });

    test('shutdown in the guest ends that sandbox with reason poweroff', () async {
      final other = await CollaboCore.start(const CollaboConfig(cpus: 1, python: false, networkEnabled: false, quiet: true));
      await other.writeConsole('shutdown\n');
      final exit = await other.done.timeout(const Duration(seconds: 30));
      expect(exit.reason, 'poweroff');
      expect(other.run('true'), throwsA(isA<CollaboException>()));
      expect((await sandbox.run('echo still-alive')).stdoutText, 'still-alive\n');
    });
  });
}
