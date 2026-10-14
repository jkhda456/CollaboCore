//! Test hook: runs yq's own operator scenarios (extracted from its *_test.go as JSON) the way
//! its testScenario does, and reports the ones whose results differ.

use crate::eval::Ctx;
use crate::formats::Decoder;
use crate::node::*;

fn field(e: &Engine, obj: NodeId, name: &str) -> Option<NodeId> {
    let c = &e.n(obj).content;
    let mut i = 0;
    while i + 1 < c.len() {
        if e.n(c[i]).value == name {
            return Some(c[i + 1]);
        }
        i += 2;
    }
    None
}

fn sfield(e: &Engine, obj: NodeId, name: &str) -> String {
    field(e, obj, name).map(|n| if e.n(n).tag == "!!null" { String::new() } else { e.n(n).value.clone() }).unwrap_or_default()
}

fn path_string(e: &Engine, n: NodeId) -> String {
    let p: Vec<String> = e.get_path(n).iter().map(|x| x.to_go_string()).collect();
    format!("[{}]", p.join(" "))
}

pub fn run(path: &str, only: Option<&str>) -> i32 {
    let data = std::fs::read(path).unwrap();
    let mut meta = Engine::new(crate::prefs::Prefs::default());
    let mut dec = crate::formats::json::JsonDecoder::new();
    dec.init(data).unwrap();
    let root = dec.decode(&mut meta).unwrap().unwrap();
    let scenarios = meta.n(root).content.clone();
    let (mut pass, mut fail) = (0, 0);
    for (idx, s) in scenarios.iter().enumerate() {
        let var = sfield(&meta, *s, "Var");
        if let Some(o) = only {
            if !var.contains(o) && o != idx.to_string() {
                continue;
            }
        }
        if field(&meta, *s, "SkipForGoccy").map(|n| meta.n(n).value == "true").unwrap_or(false) && false {
            continue;
        }
        let desc = sfield(&meta, *s, "Description");
        let doc = sfield(&meta, *s, "Document");
        let doc2 = sfield(&meta, *s, "Document2");
        let exp = sfield(&meta, *s, "Expression");
        let expected_err = sfield(&meta, *s, "ExpectedError");
        let expected: Vec<String> = field(&meta, *s, "Expected").map(|n| meta.n(n).content.iter().map(|c| meta.n(*c).value.clone()).collect()).unwrap_or_default();
        if let Some(env) = field(&meta, *s, "Env") {
            let c = meta.n(env).content.clone();
            let mut i = 0;
            while i + 1 < c.len() {
                std::env::set_var(&meta.n(c[i]).value, &meta.n(c[i + 1]).value);
                i += 2;
            }
        }
        let mut prefs = crate::prefs::Prefs::default();
        if var.starts_with("fixed") {
            prefs.yaml.fix_merge_anchor_to_spec = true;
        }
        if var == "loadOperatorSecurityDisabledScenarios" {
            prefs.security.disable_file_ops = true;
        }
        if var == "envOperatorSecurityDisabledScenarios" {
            prefs.security.disable_env_ops = true;
        }
        let got = run_one(&doc, &doc2, &exp, prefs);
        let ok = match &got {
            Err(e) => !expected_err.is_empty() && *e == expected_err,
            Ok(lines) => expected_err.is_empty() && *lines == expected,
        };
        if ok {
            pass += 1;
        } else {
            fail += 1;
            println!("FAIL #{idx} {var}: {desc}\n  exp: {exp}\n  doc: {doc:?}");
            match &got {
                Err(e) => println!("  got error: {e}"),
                Ok(l) => println!("  got: {l:?}"),
            }
            if expected_err.is_empty() {
                println!("  want: {expected:?}");
            } else {
                println!("  want error: {expected_err}");
            }
        }
    }
    println!("pass={pass} fail={fail}");
    if fail > 0 {
        1
    } else {
        0
    }
}

fn run_one(doc: &str, doc2: &str, exp: &str, prefs: crate::prefs::Prefs) -> Result<Vec<String>, String> {
    let mut e = Engine::new(prefs);
    e.prefs.yaml.colors_enabled = false;
    let node = crate::parser::parse_expression(&mut e, exp).map_err(|err| format!("parse: {err}"))?;
    let mut inputs = Vec::new();
    if !doc.is_empty() {
        let mut d = crate::formats::yaml::YamlDecoder::new(e.prefs.yaml.clone());
        inputs = crate::printer::read_documents(&mut e, doc.as_bytes().to_vec(), "sample.yml", 0, &mut d)?;
        if !doc2.is_empty() {
            let mut d = crate::formats::yaml::YamlDecoder::new(e.prefs.yaml.clone());
            let more = crate::printer::read_documents(&mut e, doc2.as_bytes().to_vec(), "another.yml", 1, &mut d)?;
            inputs.extend(more);
        }
    } else {
        inputs.push(e.scalar("!!null", ""));
    }
    let ctx = e.get_matching_nodes(&Ctx::new(inputs), node.as_ref())?;
    let mut out = Vec::new();
    for n in ctx.nodes.iter() {
        let mut prefs = e.prefs.yaml.clone();
        prefs.print_doc_separators = true;
        prefs.unwrap_scalar = true;
        prefs.indent = 4;
        let enc = Box::new(crate::formats::yaml::YamlEncoder { prefs });
        let mut p = crate::printer::Printer::new(enc, crate::printer::Writer::Buffer(Vec::new()));
        p.print_results(&mut e, &[*n])?;
        let crate::printer::Writer::Buffer(b) = p.writer else { unreachable!() };
        let tag = if e.n(*n).kind == ALIAS { "alias".to_string() } else { e.n(*n).tag.clone() };
        out.push(format!("D{}, P{}, ({})::{}", e.get_document(*n), path_string(&e, *n), tag, String::from_utf8_lossy(&b)));
    }
    Ok(out)
}

// ---- format scenarios (formatScenario tables) ----

use crate::formats::{Encoder as Enc, Decoder as Dec};

fn yaml_dec(p: &crate::prefs::Prefs) -> Box<dyn Dec> {
    Box::new(crate::formats::yaml::YamlDecoder::new(p.yaml.clone()))
}
fn yaml_enc(p: &crate::prefs::Prefs, indent: i64) -> Box<dyn Enc> {
    let mut y = p.yaml.clone();
    y.indent = indent;
    Box::new(crate::formats::yaml::YamlEncoder { prefs: y })
}

/// (decoder, encoder, expects error) as yq's test runner for that scenario sets them up.
fn format_setup(var: &str, typ: &str, indent: i64, p: &mut crate::prefs::Prefs) -> Option<(Box<dyn Dec>, Box<dyn Enc>, bool)> {
    use crate::formats::{json::*, simple::*, xml::*};
    let json_enc = |indent: i64| -> Box<dyn Enc> { Box::new(JsonEncoder { indent, unwrap_scalar: false, colors: false }) };
    let err = typ.ends_with("error");
    Some(match var {
        "jsonScenarios" => match typ {
            "encode" => (yaml_dec(p), json_enc(indent), false),
            "decode" | "roundtrip-ndjson" | "decode-error" => (Box::new(JsonDecoder::new()), json_enc(indent), err),
            "decode-ndjson" => (Box::new(JsonDecoder::new()), yaml_enc(p, 2), false),
            "roundtrip-multi" => (Box::new(JsonDecoder::new()), json_enc(2), false),
            _ => return None,
        },
        "xmlScenarios" => {
            let mut xp = p.xml.clone();
            match typ {
                "" | "decode" => (Box::new(YqXmlDecoder::new(xp)), yaml_enc(p, 4), false),
                "encode" | "encode-error" => (yaml_dec(p), Box::new(YqXmlEncoder::new(xp)), err),
                "roundtrip" => (Box::new(YqXmlDecoder::new(xp.clone())), Box::new(YqXmlEncoder::new(xp)), false),
                "decode-keep-ns" | "decode-raw-token" | "decode-raw-token-off" | "decode-error" => {
                    xp = crate::prefs::XmlPrefs::default();
                    if typ == "decode-keep-ns" {
                        xp.keep_namespace = true;
                    }
                    if typ == "decode-raw-token" {
                        xp.use_raw_token = true;
                    }
                    if typ == "decode-raw-token-off" {
                        xp.use_raw_token = false;
                    }
                    (Box::new(YqXmlDecoder::new(xp)), yaml_enc(p, 2), err)
                }
                "roundtrip-skip-directives" => {
                    xp = crate::prefs::XmlPrefs::default();
                    xp.skip_directives = true;
                    (Box::new(YqXmlDecoder::new(xp.clone())), Box::new(YqXmlEncoder::new(xp)), false)
                }
                _ => return None,
            }
        }
        "csvScenarios" => match typ {
            "encode-csv" => (yaml_dec(p), Box::new(CsvEncoder { separator: ',' }), false),
            "encode-tsv" => (yaml_dec(p), Box::new(CsvEncoder { separator: '\t' }), false),
            "decode-csv" => (Box::new(CsvDecoder::new(p.csv.clone())), yaml_enc(p, 2), false),
            "decode-csv-no-auto" => (Box::new(CsvDecoder::new(crate::prefs::CsvPrefs { separator: ',', auto_parse: false })), yaml_enc(p, 2), false),
            "decode-tsv-object" => (Box::new(CsvDecoder::new(p.tsv.clone())), yaml_enc(p, 2), false),
            "roundtrip-csv" => (Box::new(CsvDecoder::new(p.csv.clone())), Box::new(CsvEncoder { separator: ',' }), false),
            _ => return None,
        },
        "propertyScenarios" => {
            let mut pp = p.props.clone();
            match typ {
                "" => (yaml_dec(p), Box::new(PropsEncoder { prefs: pp }), false),
                "decode" => (Box::new(PropsDecoder::new(pp)), yaml_enc(p, 2), false),
                "decode-array-brackets" => {
                    pp.use_array_brackets = true;
                    (Box::new(PropsDecoder::new(pp)), yaml_enc(p, 2), false)
                }
                "encode-wrapped" => {
                    pp.unwrap_scalar = false;
                    (yaml_dec(p), Box::new(PropsEncoder { prefs: pp }), false)
                }
                "encode-array-brackets" => {
                    pp.key_value_separator = " = ".into();
                    pp.use_array_brackets = true;
                    (yaml_dec(p), Box::new(PropsEncoder { prefs: pp }), false)
                }
                "encode-custom-separator" => {
                    pp.key_value_separator = " :@ ".into();
                    (yaml_dec(p), Box::new(PropsEncoder { prefs: pp }), false)
                }
                "roundtrip" => (Box::new(PropsDecoder::new(pp.clone())), Box::new(PropsEncoder { prefs: pp }), false),
                _ => return None,
            }
        }
        "base64Scenarios" | "base64UrlScenarios" => {
            let url = var == "base64UrlScenarios";
            match typ {
                "" | "decode" => (Box::new(Base64Decoder::new(url)), yaml_enc(p, 4), false),
                "encode" | "encode-error" => (yaml_dec(p), Box::new(Base64Encoder { url }), err),
                "roundtrip" => (Box::new(Base64Decoder::new(url)), Box::new(Base64Encoder { url }), false),
                _ => return None,
            }
        }
        "shellVariablesScenarios" => {
            let mut sp = p.shell.clone();
            if typ == "shell-separator" {
                sp.key_separator = "__".into();
            }
            (yaml_dec(p), Box::new(ShellVarsEncoder { prefs: sp }), false)
        }
        "yamlFormatScenarios" | "formattingExpressionScenarios" => (yaml_dec(p), yaml_enc(p, 2), false),
        _ => return crate::harness_extra::format_setup(var, typ, indent, p),
    })
}

pub fn run_formats(path: &str, only: Option<&str>) -> i32 {
    let data = std::fs::read(path).unwrap();
    let mut meta = Engine::new(crate::prefs::Prefs::default());
    let mut dec = crate::formats::json::JsonDecoder::new();
    dec.init(data).unwrap();
    let root = dec.decode(&mut meta).unwrap().unwrap();
    let scenarios = meta.n(root).content.clone();
    let (mut pass, mut fail, mut skip) = (0, 0, 0);
    for (idx, s) in scenarios.iter().enumerate() {
        let var = sfield(&meta, *s, "Var");
        let typ = sfield(&meta, *s, "Type");
        if let Some(o) = only {
            if !var.contains(o) && o != idx.to_string() {
                continue;
            }
        }
        let indent: i64 = sfield(&meta, *s, "Indent").parse().unwrap_or(0);
        let mut prefs = crate::prefs::Prefs::default();
        prefs.yaml.colors_enabled = false;
        prefs.json.colors_enabled = false;
        let Some((mut d, en, expect_err)) = format_setup(&var, &typ, indent, &mut prefs) else {
            skip += 1;
            continue;
        };
        let input = sfield(&meta, *s, "Input");
        let mut exp = sfield(&meta, *s, "Expression");
        if exp.is_empty() {
            exp = ".".into();
        }
        let expected = sfield(&meta, *s, "Expected");
        let expected_err = sfield(&meta, *s, "ExpectedError");
        let mut e = Engine::new(prefs);
        let got: Result<String, String> = (|| {
            let inputs = crate::printer::read_documents(&mut e, input.as_bytes().to_vec(), "sample.yml", 0, d.as_mut())?;
            let node = crate::parser::parse_expression(&mut e, &exp)?;
            let ctx = e.get_matching_nodes(&Ctx::new(inputs), node.as_ref())?;
            let mut p = crate::printer::Printer::new(en, crate::printer::Writer::Buffer(Vec::new()));
            p.print_results(&mut e, &ctx.nodes)?;
            let crate::printer::Writer::Buffer(b) = p.writer else { unreachable!() };
            Ok(String::from_utf8_lossy(&b).into_owned())
        })();
        let ok = match &got {
            Ok(o) => !expect_err && *o == expected,
            Err(er) => expect_err && *er == expected_err,
        };
        if ok {
            pass += 1;
        } else {
            fail += 1;
            let desc = sfield(&meta, *s, "Description");
            println!("FAIL #{idx} {var} [{typ}] {desc}\n  exp: {exp}\n  input: {input:?}");
            match &got {
                Ok(o) => println!("  got:  {o:?}"),
                Err(er) => println!("  got error: {er}"),
            }
            if expect_err {
                println!("  want error: {expected_err}");
            } else {
                println!("  want: {expected:?}");
            }
        }
    }
    println!("pass={pass} fail={fail} skip={skip}");
    if fail > 0 {
        1
    } else {
        0
    }
}
