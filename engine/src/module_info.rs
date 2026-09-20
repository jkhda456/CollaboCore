//! Facts the host needs from the kernel module before it can run it: the shape of the memory it
//! imports, and the custom sections the build put there (`.linux.sections`, `.linux.initramfs`).
use anyhow::{bail, Context, Result};
use wasmparser::{Parser, Payload, TypeRef};

pub struct MemoryImport {
    pub is_shared: bool,
    pub minimum_pages: u64,
    pub maximum_pages: Option<u64>,
}

/// The single memory the kernel imports as `env.memory`, and its minimum size in pages.
pub fn memory_import(kernel: &[u8]) -> Result<(MemoryImport, u64)> {
    for payload in Parser::new(0).parse_all(kernel) {
        if let Payload::ImportSection(imports) = payload? {
            for import in imports {
                let import = import?;
                if let TypeRef::Memory(memory) = import.ty {
                    if (import.module, import.name) != ("env", "memory") {
                        bail!("the kernel imports an unexpected memory: {}.{}", import.module, import.name);
                    }
                    let info = MemoryImport {
                        is_shared: memory.shared,
                        minimum_pages: memory.initial,
                        maximum_pages: memory.maximum,
                    };
                    let pages = info.minimum_pages;
                    return Ok((info, pages));
                }
            }
        }
    }
    bail!("the kernel imports no memory")
}

/// The contents of a custom section.
pub fn custom_section(kernel: &[u8], name: &str) -> Result<Vec<u8>> {
    for payload in Parser::new(0).parse_all(kernel) {
        if let Payload::CustomSection(section) = payload? {
            if section.name() == name {
                return Ok(section.data().to_vec());
            }
        }
    }
    Err(anyhow::anyhow!("the kernel has no {name} section")).context("this is not a collaboCore kernel")
}

/// The kernel's `.linux.sections` table: `{"<name>": [start, size], ...}`. Only this shape
/// appears there, so a small scanner avoids a JSON dependency (and rejects anything else).
pub fn parse_sections(json: &[u8]) -> Vec<(String, Vec<u32>)> {
    let text = String::from_utf8_lossy(json);
    let mut sections = Vec::new();
    let mut rest = text.trim().trim_start_matches('{').trim_end_matches('}');
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        let name = after[..close].to_string();
        let Some(bracket) = after[close..].find('[') else { break };
        let list_start = close + bracket + 1;
        let Some(list_end) = after[list_start..].find(']') else { break };
        let cells = after[list_start..list_start + list_end]
            .split(',')
            .filter_map(|value| value.trim().parse::<u32>().ok())
            .collect();
        sections.push((name, cells));
        rest = &after[list_start + list_end..];
    }
    sections
}
