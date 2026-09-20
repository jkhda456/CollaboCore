// The collabo_core package inside the Flutter engine (flutter_tester): starts a real sandbox,
// types a command into the app's command line, and reads the answer from the app's console.
//   COLLABO_CORE_RUNTIME=<collaboCore>/dist/runtime flutter test
import 'dart:io';

import 'package:collabo_core/collabo_core.dart';
import 'package:collabo_core_demo/main.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets('the demo app runs a sandbox and its shell answers', (tester) async {
    final workspace = Directory.systemTemp.createTempSync('collabo-flutter-');
    File('${workspace.path}/from-host.txt').writeAsStringSync('hello from the host');
    // Process I/O does not advance under the widget test's fake clock, so the sandbox is started
    // in real async time and handed to the app (a real app just calls CollaboCore.start).
    final sandbox = await tester.runAsync(() => CollaboCore.start(CollaboConfig(
          cpus: 2,
          quiet: true,
          mounts: [Mount(hostPath: workspace.path, guestPath: '/work')],
        )));
    await tester.pumpWidget(DemoApp(start: () async => sandbox!));

    // Real process I/O happens outside the test's fake clock.
    Future<void> waitFor(bool Function() condition, String what) async {
      for (var i = 0; i < 600 && !condition(); i++) {
        await tester.runAsync(() => Future<void>.delayed(const Duration(milliseconds: 100)));
        await tester.pump();
      }
      expect(condition(), isTrue, reason: 'timed out waiting for $what');
    }

    String consoleText() {
      final finder = find.byKey(const Key('console'));
      return finder.evaluate().isEmpty ? '' : (tester.widget<SelectableText>(finder).data ?? '');
    }

    await waitFor(() => find.byKey(const Key('console')).evaluate().isNotEmpty, 'the sandbox to start');
    await waitFor(() => consoleText().contains('#'), 'the shell prompt');

    await tester.enterText(find.byKey(const Key('command')), r'cat /work/from-host.txt; echo; python3 -c "print(6*7)"; uname -n');
    await tester.testTextInput.receiveAction(TextInputAction.done);
    await waitFor(() => consoleText().contains('42\ncollabo'), 'the command output');
    expect(consoleText(), contains('hello from the host'));

    // The same sandbox through the API, while the app shows it.
    final r = await tester.runAsync(() => sandbox!.run('echo made-in-guest > /work/out.txt; echo ok'));
    expect(r!.stdoutText, 'ok\n');
    expect(File('${workspace.path}/out.txt').readAsStringSync(), 'made-in-guest\n');

    await tester.runAsync(() => sandbox!.stop());
    workspace.deleteSync(recursive: true);
  });
}
