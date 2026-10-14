//! yq's formats: the Encoder and Decoder interfaces and the factory by format name.

pub mod json;
pub mod simple;
pub mod xml;
pub mod yaml;

use crate::eval::Res;
use crate::node::{Engine, NodeId};
use crate::parser::Fmt;

pub trait Decoder {
    fn init(&mut self, input: Vec<u8>) -> Res<()>;
    /// The next document, None at the end.
    fn decode(&mut self, e: &mut Engine) -> Res<Option<NodeId>>;
}

pub trait Encoder {
    fn encode(&mut self, e: &mut Engine, out: &mut Vec<u8>, node: NodeId) -> Res<()>;
    fn print_document_separator(&mut self, out: &mut Vec<u8>) -> Res<()>;
    fn print_leading_content(&mut self, out: &mut Vec<u8>, content: &str) -> Res<()>;
    fn can_handle_aliases(&self) -> bool;
}

pub struct FormatInfo {
    pub fmt: Fmt,
    pub formal_name: &'static str,
    pub names: &'static [&'static str],
    pub has_encoder: bool,
    pub has_decoder: bool,
}

pub const FORMATS: &[FormatInfo] = &[
    FormatInfo { fmt: Fmt::Yaml, formal_name: "yaml", names: &["y", "yml"], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::KYaml, formal_name: "kyaml", names: &["ky"], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::Json, formal_name: "json", names: &["j"], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::Props, formal_name: "props", names: &["p", "properties"], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::Csv, formal_name: "csv", names: &["c"], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::Tsv, formal_name: "tsv", names: &["t"], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::Xml, formal_name: "xml", names: &["x"], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::Base64, formal_name: "base64", names: &[], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::Base64Url, formal_name: "base64url", names: &[], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::Uri, formal_name: "uri", names: &[], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::Sh, formal_name: "", names: &[], has_encoder: true, has_decoder: false },
    FormatInfo { fmt: Fmt::Toml, formal_name: "toml", names: &[], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::Hcl, formal_name: "hcl", names: &["h", "tf"], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::Shell, formal_name: "shell", names: &["s", "sh"], has_encoder: true, has_decoder: false },
    FormatInfo { fmt: Fmt::Lua, formal_name: "lua", names: &["l"], has_encoder: true, has_decoder: true },
    FormatInfo { fmt: Fmt::Ini, formal_name: "ini", names: &["i"], has_encoder: true, has_decoder: true },
];

pub fn info(f: Fmt) -> &'static FormatInfo {
    FORMATS.iter().find(|i| i.fmt == f).unwrap()
}

fn available(encoders: bool) -> String {
    let mut out = Vec::new();
    for f in FORMATS {
        if (encoders && f.has_encoder) || (!encoders && f.has_decoder) {
            if !f.formal_name.is_empty() {
                out.push(f.formal_name);
            }
            if let Some(n) = f.names.first() {
                out.push(n);
            }
        }
    }
    out.join("|")
}

pub fn available_output_formats() -> String {
    available(true)
}

pub fn available_input_formats() -> String {
    available(false)
}

/// FormatFromString
pub fn format_from_string(name: &str) -> Res<Fmt> {
    if !name.is_empty() {
        for f in FORMATS {
            if f.formal_name == name || f.names.contains(&name) {
                return Ok(f.fmt);
            }
        }
    }
    Err(format!("unknown format '{name}' please use [{}]", available_output_formats()))
}

/// FormatStringFromFilename
pub fn format_string_from_filename(filename: &str) -> String {
    if !filename.is_empty() {
        let base = filename.rsplit('/').next().unwrap_or(filename);
        if let Some(dot) = base.rfind('.') {
            let ext = &base[dot..];
            if ext.len() >= 2 {
                let f = ext[1..].to_lowercase();
                if format_from_string(&f).is_ok() {
                    return f;
                }
            }
        }
    }
    "yaml".into()
}

pub fn decoder_for(e: &Engine, f: Fmt) -> Option<Box<dyn Decoder>> {
    crate::formats_extra::decoder_for(e, f)
}

pub fn encoder_for(e: &Engine, f: Fmt) -> Option<Box<dyn Encoder>> {
    crate::formats_extra::encoder_for(e, f)
}
