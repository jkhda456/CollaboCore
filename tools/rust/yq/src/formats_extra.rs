//! The encoders and decoders for each format, configured from the preferences.

use crate::formats::{Decoder, Encoder};
use crate::node::Engine;
use crate::parser::Fmt;

pub fn decoder_for(e: &Engine, f: Fmt) -> Option<Box<dyn Decoder>> {
    match f {
        Fmt::Yaml | Fmt::KYaml => Some(Box::new(crate::formats::yaml::YamlDecoder::new(e.prefs.yaml.clone()))),
        Fmt::Json => Some(Box::new(crate::formats::json::JsonDecoder::new())),
        Fmt::Props => Some(Box::new(crate::formats::simple::PropsDecoder::new(e.prefs.props.clone()))),
        Fmt::Csv => Some(Box::new(crate::formats::simple::CsvDecoder::new(e.prefs.csv.clone()))),
        Fmt::Tsv => Some(Box::new(crate::formats::simple::CsvDecoder::new(e.prefs.tsv.clone()))),
        Fmt::Base64 => Some(Box::new(crate::formats::simple::Base64Decoder::new(false))),
        Fmt::Base64Url => Some(Box::new(crate::formats::simple::Base64Decoder::new(true))),
        Fmt::Uri => Some(Box::new(crate::formats::simple::UriDecoder::new())),
        Fmt::Xml => Some(Box::new(crate::formats::xml::YqXmlDecoder::new(e.prefs.xml.clone()))),
        _ => None,
    }
}

pub fn encoder_for(e: &Engine, f: Fmt) -> Option<Box<dyn Encoder>> {
    match f {
        Fmt::Yaml => Some(Box::new(crate::formats::yaml::YamlEncoder { prefs: e.prefs.yaml.clone() })),
        Fmt::Json => Some(Box::new(crate::formats::json::JsonEncoder {
            indent: e.prefs.json.indent,
            unwrap_scalar: e.prefs.json.unwrap_scalar,
            colors: e.prefs.json.colors_enabled,
        })),
        Fmt::Props => Some(Box::new(crate::formats::simple::PropsEncoder { prefs: e.prefs.props.clone() })),
        Fmt::Csv => Some(Box::new(crate::formats::simple::CsvEncoder { separator: e.prefs.csv.separator })),
        Fmt::Tsv => Some(Box::new(crate::formats::simple::CsvEncoder { separator: e.prefs.tsv.separator })),
        Fmt::Base64 => Some(Box::new(crate::formats::simple::Base64Encoder { url: false })),
        Fmt::Base64Url => Some(Box::new(crate::formats::simple::Base64Encoder { url: true })),
        Fmt::Uri => Some(Box::new(crate::formats::simple::UriEncoder)),
        Fmt::Sh => Some(Box::new(crate::formats::simple::ShEncoder)),
        Fmt::Xml => Some(Box::new(crate::formats::xml::YqXmlEncoder::new(e.prefs.xml.clone()))),
        Fmt::Shell => Some(Box::new(crate::formats::simple::ShellVarsEncoder { prefs: e.prefs.shell.clone() })),
        _ => None,
    }
}

pub fn xml_encoder_with_indent(e: &Engine, indent: i64) -> Option<Box<dyn Encoder>> {
    let mut p = e.prefs.xml.clone();
    p.indent = indent;
    Some(Box::new(crate::formats::xml::YqXmlEncoder::new(p)))
}
