/// Configuration of a sandbox. Serialized to the runtime's `start` request
/// (runtime/src/sandbox.mjs `normalizeConfig` validates it again).
class CollaboConfig {
  const CollaboConfig({
    this.cpus,
    this.python = true,
    this.mounts = const [],
    this.network = const NetworkPolicy(),
    this.networkEnabled = true,
    this.hostExec = HostExecPolicy.deny,
    this.hostFunctions = const [],
    this.permissionTimeout = const Duration(minutes: 2),
    this.quiet = false,
    this.consoleColumns = 120,
    this.consoleRows = 40,
  });

  /// Virtual CPUs; one host thread each. Default: min(4, host CPUs).
  final int? cpus;

  /// Boot with CPython 3.13 (adds ~17 MB to what is loaded at start).
  final bool python;

  /// Local folders to share with the sandbox.
  final List<Mount> mounts;

  /// Which hosts the sandbox may reach, and secrets the host adds to its requests.
  final NetworkPolicy network;

  /// false: no network device and no request API at all.
  final bool networkEnabled;

  /// May the sandbox run programs on this computer (shell tools and GUI apps)?
  final HostExecPolicy hostExec;

  /// Names of functions this app offers to the sandbox, handled by
  /// [CollaboCore.onHostCall] (guest: `hostcall NAME '{...}'`, python `collabo_core.host.call`).
  final List<String> hostFunctions;

  /// How long an "ask" permission request waits for [CollaboCore.onPermission] before refusing.
  final Duration permissionTimeout;

  /// No banner on the console.
  final bool quiet;
  final int consoleColumns;
  final int consoleRows;

  Map<String, Object?> toJson() => {
        if (cpus != null) 'cpus': cpus,
        'python': python,
        'mounts': [for (final m in mounts) m.toJson()],
        'network': networkEnabled ? network.toJson() : false,
        'hostExec': hostExec.name,
        'hostFunctions': hostFunctions,
        'permissionTimeoutMs': permissionTimeout.inMilliseconds,
        'quiet': quiet,
        'consoleSize': {'cols': consoleColumns, 'rows': consoleRows},
      };
}

/// A local folder shared with the sandbox (virtio-fs). Changes are live in both directions.
class Mount {
  const Mount({required this.hostPath, required this.guestPath, this.readOnly = false});

  /// A folder on this computer (absolute, or relative to the app's working directory).
  final String hostPath;

  /// Where it appears in the sandbox, e.g. `/work`. Letters, digits, `. _ - /` only, and
  /// not a system directory.
  final String guestPath;
  final bool readOnly;

  Map<String, Object?> toJson() => {'hostPath': hostPath, 'guestPath': guestPath, 'readOnly': readOnly};
}

/// The sandbox's network rules. Patterns: `example.com` (exactly), `*.example.com`
/// (subdomains), `*` (anything). [deny] wins over [allow].
class NetworkPolicy {
  const NetworkPolicy({
    this.allow = const ['*'],
    this.deny = const [],
    this.allowHostLoopback = false,
    this.secrets = const [],
    this.extraAllowedHeaders = const [],
  });

  final List<String> allow;
  final List<String> deny;

  /// May the sandbox reach services on this computer (localhost)? Off by default.
  final bool allowHostLoopback;

  /// Headers the host adds to HTTPS requests from the sandbox (API keys). The sandbox never
  /// sees the values; they are not logged or echoed in events.
  final List<Secret> secrets;

  /// Request headers the sandbox may set itself, beyond accept, content-type, authorization
  /// and x-api-key (for example `anthropic-version`).
  final List<String> extraAllowedHeaders;

  Map<String, Object?> toJson() => {
        'allow': allow,
        'deny': deny,
        'allowHostLoopback': allowHostLoopback,
        'secrets': [for (final s in secrets) s.toJson()],
        'extraAllowedHeaders': extraAllowedHeaders,
      };
}

class Secret {
  const Secret({required this.host, required this.header, required this.value});

  /// Host pattern the header is added for (https only).
  final String host;
  final String header;
  final String value;

  Map<String, Object?> toJson() => {'host': host, 'header': header, 'value': value};

  @override
  String toString() => 'Secret($host, $header, ***)';
}

enum HostExecPolicy {
  /// Refuse every request to run a host program.
  deny,

  /// Ask the app ([CollaboCore.onPermission]) each time.
  ask,

  /// Run whatever the sandbox asks for. Only for trusted agents.
  allow,
}
