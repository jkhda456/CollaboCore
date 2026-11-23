//! This build's version and platform, as the engine and the launcher print them (`--version`)
//! and as the runtime folders and release archives are named (scripts/package-runtime.sh).

/// The engine's version (engine/Cargo.toml); the launcher is built with it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The platform in the names package-runtime.sh gives: collabo-core-<platform>.
pub const PLATFORM: &str = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    "linux-x64"
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    "linux-arm64"
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    "darwin-x64"
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    "darwin-arm64"
} else if cfg!(all(windows, target_arch = "x86_64")) {
    "win-x64"
} else if cfg!(all(windows, target_arch = "aarch64")) {
    "win-arm64"
} else {
    "unknown"
};

/// What `--version` prints.
pub fn version_line() -> String {
    format!("collabo-core {VERSION} ({PLATFORM})")
}
