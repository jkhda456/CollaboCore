/// Configuration of a sandbox. Serialized to the runtime's `start` request
/// (runtime/src/sandbox.mjs `normalizeConfig` validates it again).
class CollaboConfig {
  const CollaboConfig({
    this.cpus,
    this.python = true,
    this.tools = true,
    this.addons = const {},
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

  /// Boot with CPython 3.13 and pip (adds ~62 MB to what is loaded at start).
  final bool python;

  /// Boot with the network tools: curl, ssh (dropbear) and git (adds ~14 MB). busybox's wget,
  /// nc and telnet are always there.
  final bool tools;

  /// Add-ons to boot with (addons/README.md), by name, each with its settings (`{}` for none).
  /// None by default. An `apiKey` setting never reaches the sandbox: the host adds it to the
  /// add-on's https requests, like a [Secret]. For the claude-code add-on see
  /// [ClaudeCodeSettings]:
  ///
  /// ```dart
  /// addons: {'claude-code': ClaudeCodeSettings.openai(baseUrl: 'http://localhost:11434/v1',
  ///     model: 'qwen3-coder').toJson()},
  /// network: NetworkPolicy(allowHostLoopback: true),   // localhost is this computer
  /// ```
  final Map<String, Map<String, Object?>> addons;

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
        'tools': tools,
        if (addons.isNotEmpty) 'addons': addons,
        'mounts': [for (final m in mounts) m.toJson()],
        'network': networkEnabled ? network.toJson() : false,
        'hostExec': hostExec.name,
        'hostFunctions': hostFunctions,
        'permissionTimeoutMs': permissionTimeout.inMilliseconds,
        'quiet': quiet,
        'consoleSize': {'cols': consoleColumns, 'rows': consoleRows},
      };
}

/// Settings of the claude-code add-on: `claude` in the sandbox, a coding agent that talks to
/// the Anthropic Messages API or to an OpenAI-compatible server (a local LLM). Written to
/// /etc/collabo/addons/claude-code.json in the sandbox; `claude --show-config` shows them.
class ClaudeCodeSettings {
  const ClaudeCodeSettings({
    this.provider = 'anthropic',
    this.baseUrl,
    this.model,
    this.apiKey,
    this.maxTokens,
    this.stream,
    this.permissionMode,
    this.appendSystemPrompt,
    this.maxTurns,
  });

  /// Anthropic's API (or a gateway that speaks it).
  const ClaudeCodeSettings.anthropic({String? baseUrl, String? model, String? apiKey, int? maxTokens})
      : this(provider: 'anthropic', baseUrl: baseUrl, model: model, apiKey: apiKey, maxTokens: maxTokens);

  /// An OpenAI-compatible Chat Completions server: Ollama (`http://localhost:11434/v1`),
  /// llama.cpp, vLLM, LM Studio, … The request is made by this computer, so `localhost` is
  /// this computer: that needs [NetworkPolicy.allowHostLoopback]. Without [model], the first
  /// one the server lists is used.
  const ClaudeCodeSettings.openai({required String baseUrl, String? model, String? apiKey, int? maxTokens})
      : this(provider: 'openai', baseUrl: baseUrl, model: model, apiKey: apiKey, maxTokens: maxTokens);

  /// `anthropic` or `openai`.
  final String provider;
  final String? baseUrl;
  final String? model;

  /// Added by the host to https requests for [baseUrl]'s host; never in the sandbox. (A
  /// plain-http [baseUrl] gets it in the sandbox's settings instead: the host adds keys to
  /// https requests only.)
  final String? apiKey;
  final int? maxTokens;
  final bool? stream;

  /// `default` (ask before commands and edits), `acceptEdits`, `plan` or `bypassPermissions`
  /// (the sandbox is the boundary). With `claude -p` nobody can answer, so what needs a yes is
  /// refused unless the mode or the permission rules allow it.
  final String? permissionMode;
  final String? appendSystemPrompt;
  final int? maxTurns;

  Map<String, Object?> toJson() => {
        'provider': provider,
        if (baseUrl != null) 'baseUrl': baseUrl,
        if (model != null) 'model': model,
        if (apiKey != null) 'apiKey': apiKey,
        if (maxTokens != null) 'maxTokens': maxTokens,
        if (stream != null) 'stream': stream,
        if (permissionMode != null) 'permissionMode': permissionMode,
        if (appendSystemPrompt != null) 'appendSystemPrompt': appendSystemPrompt,
        if (maxTurns != null) 'maxTurns': maxTurns,
      };

  @override
  String toString() => 'ClaudeCodeSettings($provider, ${baseUrl ?? 'default URL'}, ${model ?? 'default model'}'
      '${apiKey != null ? ', key ***' : ''})';
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
    this.ask = false,
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

  /// A host that neither [allow] nor [deny] names is not refused but asked about:
  /// [CollaboCore.onPermission] gets a request of kind "network" with target "host:port"
  /// (curl, git, ssh, python sockets and hfetch alike). The answer holds for that host:port
  /// until the policy changes, unless the handler clears [PermissionRequest.remember].
  /// Useful with a narrow [allow] list, e.g. `allow: ['api.openai.com'], ask: true`.
  final bool ask;

  Map<String, Object?> toJson() => {
        'allow': allow,
        'deny': deny,
        'allowHostLoopback': allowHostLoopback,
        'secrets': [for (final s in secrets) s.toJson()],
        'extraAllowedHeaders': extraAllowedHeaders,
        'ask': ask,
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
