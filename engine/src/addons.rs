//! Add-ons: optional cpio overlays beside the base images, one per folder under `addons/` in the
//! source tree, packaged as `app/images/addons/<name>.cpio` with a `<name>.json` that describes
//! it. The app turns them on at start (`start.config.addons`, `--addon` on the command line); the
//! default is none.
//!
//!   addons: {"claude-code": {"provider": "openai", "baseUrl": "http://localhost:11434/v1", ...}}
//!
//! What the engine does for an add-on it turns on:
//!   * appends `<name>.cpio` to the initramfs, after the base, python and tools images
//!   * writes the add-on's settings to /etc/collabo/addons/<name>.json in the guest, less its
//!     `apiKey`, which becomes a network secret for the host of its `baseUrl` instead (https
//!     only: the host adds it to requests and the guest never sees it)
//!   * lets the guest set the request headers the description names (`extraAllowedHeaders`)
//!
//! `<name>.json`:
//!   {"name", "description", "extraAllowedHeaders": [...],
//!    "apiKey": {"selector": "provider", "default": "anthropic",
//!               "variants": {"anthropic": {"baseUrl", "header", "value": "... {key} ..."}, ...}}}
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::http;

/// The add-ons a sandbox boots with, resolved against the add-on folder.
#[derive(Default)]
pub struct Resolved {
    pub names: Vec<String>,
    /// The add-ons' images followed by one overlay with their settings.
    pub initcpio: Vec<u8>,
    pub headers: Vec<String>,
    pub secrets: Vec<http::Secret>,
}

/// Which add-ons exist in `dir`: the names of its `<name>.cpio` files.
pub fn available(dir: Option<&Path>) -> Vec<String> {
    let Some(dir) = dir else { return Vec::new() };
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.strip_suffix(".cpio").map(str::to_string))
        .collect();
    names.sort();
    names
}

/// `requested` is the app's `addons` value: an object of name → settings (`true` or `{}` for
/// none), or a list of names.
pub fn resolve(dir: Option<&Path>, requested: Option<&Value>) -> Result<Resolved, String> {
    let requested: Vec<(String, Map<String, Value>)> = match requested {
        None | Some(Value::Null) => return Ok(Resolved::default()),
        Some(Value::Array(names)) => names
            .iter()
            .map(|name| name.as_str().map(|name| (name.to_string(), Map::new())).ok_or("addons: a list of names"))
            .collect::<Result<_, _>>()?,
        Some(Value::Object(entries)) => entries
            .iter()
            .filter(|(_, settings)| !matches!(settings, Value::Bool(false) | Value::Null))
            .map(|(name, settings)| match settings {
                Value::Bool(true) => Ok((name.clone(), Map::new())),
                Value::Object(fields) => Ok((name.clone(), fields.clone())),
                _ => Err(format!("addons.{name}: true or an object of settings")),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("addons: an object of name → settings, or a list of names".into()),
    };
    if requested.is_empty() {
        return Ok(Resolved::default());
    }
    let dir = dir.ok_or("this runtime has no add-ons (no --addon-dir)")?;

    let mut resolved = Resolved::default();
    let mut settings_files: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, mut settings) in requested {
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || name.is_empty() {
            return Err(format!("addons: \"{name}\" is not an add-on name"));
        }
        let image = dir.join(format!("{name}.cpio"));
        if !image.is_file() {
            let known = available(Some(dir));
            return Err(format!("no add-on \"{name}\" (this runtime has: {})", if known.is_empty() { "none".into() } else { known.join(", ") }));
        }
        let meta = read_meta(&dir.join(format!("{name}.json")))?;
        if let Some(headers) = meta.get("extraAllowedHeaders").and_then(Value::as_array) {
            resolved.headers.extend(headers.iter().filter_map(Value::as_str).map(str::to_lowercase));
        }
        if let Some(key) = settings.remove("apiKey") {
            let key = key.as_str().ok_or_else(|| format!("addons.{name}.apiKey: a string"))?.to_string();
            match api_key_secret(&name, &meta, &settings, &key)? {
                Some(secret) => resolved.secrets.push(secret),
                // A plain-http endpoint (a model server on this computer or the LAN): the host
                // adds secrets to https requests only, so the guest carries this one itself.
                None => {
                    settings.insert("apiKey".into(), json!(key));
                }
            }
        }
        resolved.initcpio.extend(std::fs::read(&image).map_err(|error| format!("reading {}: {error}", image.display()))?);
        let text = serde_json::to_vec_pretty(&Value::Object(settings)).expect("settings serialize");
        settings_files.push((format!("etc/collabo/addons/{name}.json"), text));
        resolved.names.push(name);
    }

    let mut entries: Vec<(&str, Option<&[u8]>)> = vec![("etc", None), ("etc/collabo", None), ("etc/collabo/addons", None)];
    entries.extend(settings_files.iter().map(|(path, data)| (path.as_str(), Some(data.as_slice()))));
    resolved.initcpio.extend(crate::protocol::cpio_overlay(&entries));
    Ok(resolved)
}

fn read_meta(path: &PathBuf) -> Result<Map<String, Value>, String> {
    match std::fs::read(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(format!("reading {}: {error}", path.display())),
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(Value::Object(meta)) => Ok(meta),
            _ => Err(format!("{} is not a JSON object", path.display())),
        },
    }
}

/// The network secret an add-on's `apiKey` becomes: the header its description names for the
/// selected variant, for the host of the settings' `baseUrl` (or the variant's default one).
/// None when that URL is plain http.
fn api_key_secret(name: &str, meta: &Map<String, Value>, settings: &Map<String, Value>, key: &str) -> Result<Option<http::Secret>, String> {
    let spec = meta.get("apiKey").ok_or_else(|| format!("addons.{name}: this add-on takes no apiKey"))?;
    let variant = match spec.get("selector").and_then(Value::as_str) {
        Some(selector) => settings
            .get(selector)
            .and_then(Value::as_str)
            .or_else(|| spec.get("default").and_then(Value::as_str))
            .unwrap_or_default(),
        None => spec.get("default").and_then(Value::as_str).unwrap_or_default(),
    };
    let variant_spec = spec
        .get("variants")
        .and_then(|variants| variants.get(variant))
        .ok_or_else(|| format!("addons.{name}: no API key rule for \"{variant}\""))?;
    let url = settings
        .get("baseUrl")
        .and_then(Value::as_str)
        .or_else(|| variant_spec.get("baseUrl").and_then(Value::as_str))
        .ok_or_else(|| format!("addons.{name}: apiKey needs a baseUrl"))?;
    let (scheme, rest) = url.split_once("://").ok_or_else(|| format!("addons.{name}.baseUrl: not a URL: {url}"))?;
    if scheme.eq_ignore_ascii_case("http") {
        return Ok(None);
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default().to_string(),
        None => authority.split(':').next().unwrap_or_default().to_string(),
    };
    if host.is_empty() {
        return Err(format!("addons.{name}.baseUrl: no host in {url}"));
    }
    let header = variant_spec.get("header").and_then(Value::as_str).unwrap_or("authorization");
    let value = variant_spec.get("value").and_then(Value::as_str).unwrap_or("Bearer {key}").replace("{key}", key);
    Ok(Some(http::Secret { host: host.to_lowercase(), header: header.to_lowercase(), value }))
}

/// Command-line settings: `NAME:KEY=VALUE` → addons.NAME.KEY. Numbers and true/false keep
/// their JSON type.
pub fn parse_setting(spec: &str, into: &mut Map<String, Value>) -> Result<(), String> {
    let (name, assignment) = spec.split_once(':').ok_or("--addon-config needs NAME:KEY=VALUE")?;
    let (key, value) = assignment.split_once('=').ok_or("--addon-config needs NAME:KEY=VALUE")?;
    let value = match serde_json::from_str::<Value>(value) {
        Ok(parsed @ (Value::Number(_) | Value::Bool(_))) => parsed,
        _ => json!(value),
    };
    let settings = into.entry(name.to_string()).or_insert_with(|| json!({}));
    if !settings.is_object() {
        *settings = json!({});
    }
    settings.as_object_mut().unwrap().insert(key.to_string(), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Map<String, Value> {
        serde_json::from_value(json!({
            "apiKey": {"selector": "provider", "default": "anthropic", "variants": {
                "anthropic": {"baseUrl": "https://api.anthropic.com", "header": "x-api-key", "value": "{key}"},
                "openai": {"baseUrl": "https://api.openai.com/v1", "header": "authorization", "value": "Bearer {key}"}
            }}
        }))
        .unwrap()
    }

    #[test]
    fn api_key_becomes_a_secret_for_the_base_url_host() {
        let settings = Map::new();
        let secret = api_key_secret("x", &meta(), &settings, "k1").unwrap().unwrap();
        assert_eq!((secret.host.as_str(), secret.header.as_str(), secret.value.as_str()), ("api.anthropic.com", "x-api-key", "k1"));

        let settings: Map<String, Value> =
            serde_json::from_value(json!({"provider": "openai", "baseUrl": "https://llm.example.com:8443/v1"})).unwrap();
        let secret = api_key_secret("x", &meta(), &settings, "k2").unwrap().unwrap();
        assert_eq!((secret.host.as_str(), secret.header.as_str(), secret.value.as_str()), ("llm.example.com", "authorization", "Bearer k2"));
    }

    #[test]
    fn plain_http_keeps_the_key_in_the_guest() {
        let settings: Map<String, Value> =
            serde_json::from_value(json!({"provider": "openai", "baseUrl": "http://localhost:11434/v1"})).unwrap();
        assert!(api_key_secret("x", &meta(), &settings, "k").unwrap().is_none());
    }

    #[test]
    fn command_line_settings() {
        let mut map = Map::new();
        parse_setting("claude-code:provider=openai", &mut map).unwrap();
        parse_setting("claude-code:maxTokens=4096", &mut map).unwrap();
        parse_setting("claude-code:baseUrl=http://h:1/v1", &mut map).unwrap();
        assert_eq!(Value::Object(map), json!({"claude-code": {"provider": "openai", "maxTokens": 4096, "baseUrl": "http://h:1/v1"}}));
    }
}
