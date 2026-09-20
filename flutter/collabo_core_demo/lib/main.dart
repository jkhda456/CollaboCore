import 'dart:io';

import 'package:collabo_core/collabo_core.dart';
import 'package:flutter/material.dart';

import 'sandbox_view.dart';

/// collaboCore demo: a sandbox whose /work is ~/collabo-workspace. The runtime folder ships next
/// to the app (see linux/CMakeLists.txt, windows/CMakeLists.txt, macos Runner build phase);
/// during development set COLLABO_CORE_RUNTIME to `collaboCore/dist/runtime`.
void main() {
  runApp(const DemoApp());
}

Future<CollaboCore> startSandbox() {
  final home = Platform.environment['HOME'] ?? Platform.environment['USERPROFILE'] ?? Directory.systemTemp.path;
  final workspace = Directory('$home${Platform.pathSeparator}collabo-workspace')..createSync(recursive: true);
  return CollaboCore.start(CollaboConfig(
    mounts: [Mount(hostPath: workspace.path, guestPath: '/work')],
    network: const NetworkPolicy(allow: ['*']),
    hostExec: HostExecPolicy.ask,
  ));
}

class DemoApp extends StatelessWidget {
  const DemoApp({super.key, this.start = startSandbox});

  final Future<CollaboCore> Function() start;

  @override
  Widget build(BuildContext context) => MaterialApp(
        title: 'collaboCore',
        theme: ThemeData(colorSchemeSeed: Colors.indigo, useMaterial3: true),
        home: Scaffold(
          appBar: AppBar(title: const Text('collaboCore sandbox')),
          body: SandboxView(start: start),
        ),
      );
}
