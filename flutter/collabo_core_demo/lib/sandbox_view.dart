import 'dart:async';
import 'dart:convert';

import 'package:collabo_core/collabo_core.dart';
import 'package:flutter/material.dart';

/// A minimal sandbox console: the guest's root shell, a command line, the network log, and
/// permission prompts for host programs. [start] creates the sandbox (injected, so tests and apps
/// choose the configuration and the runtime location).
class SandboxView extends StatefulWidget {
  const SandboxView({super.key, required this.start});

  final Future<CollaboCore> Function() start;

  @override
  State<SandboxView> createState() => _SandboxViewState();
}

class _SandboxViewState extends State<SandboxView> {
  CollaboCore? _sandbox;
  String? _error;
  final _console = StringBuffer();
  final _network = <NetworkEvent>[];
  final _input = TextEditingController();
  final _scroll = ScrollController();
  final _subscriptions = <StreamSubscription<Object?>>[];

  @override
  void initState() {
    super.initState();
    _boot();
  }

  Future<void> _boot() async {
    try {
      final sandbox = await widget.start();
      if (!mounted) {
        await sandbox.stop();
        return;
      }
      sandbox.onPermission = _askPermission;
      _subscriptions
        ..add(sandbox.console.listen((bytes) => _append(utf8.decode(bytes, allowMalformed: true))))
        ..add(sandbox.networkEvents.listen((e) => setState(() => _network.insert(0, e))));
      setState(() => _sandbox = sandbox);
      await sandbox.writeConsole('\n'); // show a prompt
    } catch (e) {
      if (mounted) setState(() => _error = '$e');
    }
  }

  void _append(String text) {
    // The console speaks VT100; a real app would use a terminal widget. Strip color codes here.
    setState(() => _console.write(text.replaceAll(RegExp(r'\x1b\[[0-9;?]*[A-Za-z]'), '').replaceAll('\r', '')));
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (_scroll.hasClients) _scroll.jumpTo(_scroll.position.maxScrollExtent);
    });
  }

  Future<bool> _askPermission(PermissionRequest request) async {
    if (!mounted) return false;
    final allowed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('The sandbox wants to use this computer'),
        content: Text('$request'),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context, false), child: const Text('Deny')),
          FilledButton(onPressed: () => Navigator.pop(context, true), child: const Text('Allow')),
        ],
      ),
    );
    return allowed ?? false;
  }

  void _send() {
    final line = _input.text;
    _input.clear();
    _sandbox?.writeConsole('$line\n');
  }

  @override
  void dispose() {
    for (final s in _subscriptions) {
      s.cancel();
    }
    _sandbox?.stop();
    _input.dispose();
    _scroll.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    if (_error != null) return Center(child: Text('Could not start the sandbox:\n$_error', key: const Key('error')));
    if (_sandbox == null) return const Center(child: CircularProgressIndicator());
    return Row(
      children: [
        Expanded(
          flex: 3,
          child: Column(
            children: [
              Expanded(
                child: Container(
                  color: Colors.black,
                  padding: const EdgeInsets.all(8),
                  child: SingleChildScrollView(
                    controller: _scroll,
                    child: SelectableText(
                      _console.toString(),
                      key: const Key('console'),
                      style: const TextStyle(fontFamily: 'monospace', color: Colors.white, fontSize: 13),
                    ),
                  ),
                ),
              ),
              TextField(
                key: const Key('command'),
                controller: _input,
                decoration: const InputDecoration(hintText: 'command for the sandbox shell', prefixText: '\$ '),
                onSubmitted: (_) => _send(),
              ),
            ],
          ),
        ),
        SizedBox(
          width: 320,
          child: ListView(
            key: const Key('network'),
            children: [
              const ListTile(title: Text('Network')),
              for (final e in _network)
                ListTile(
                  dense: true,
                  leading: Icon(e.blocked ? Icons.block : Icons.public, color: e.blocked ? Colors.red : null),
                  title: Text(e.url ?? e.host ?? '${e.ip}:${e.port}', maxLines: 2, overflow: TextOverflow.ellipsis),
                  subtitle: Text('${e.via} ${e.kind} ${e.phase ?? ''} ${e.status ?? ''} ${e.reason ?? ''}'),
                ),
            ],
          ),
        ),
      ],
    );
  }
}
