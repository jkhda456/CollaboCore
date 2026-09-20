//! The flattened device tree the kernel reads at boot (a port of the host JS `devicetree.ts`).
//! Layout: <https://devicetree-specification.readthedocs.io/en/latest/chapter5-flattened-format.html>

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;
const NAME_MAX: usize = 31;

/// A property value. The kernel reads big-endian cells, strings are NUL-terminated.
pub enum Value {
    U32(u32),
    U64(u64),
    Str(String),
    Cells(Vec<u32>),
    Bytes(Vec<u8>),
    Empty,
}

impl Value {
    fn encode(&self) -> Vec<u8> {
        match self {
            Value::U32(v) => v.to_be_bytes().to_vec(),
            Value::U64(v) => v.to_be_bytes().to_vec(),
            Value::Str(s) => {
                let mut out = s.as_bytes().to_vec();
                out.push(0);
                out
            }
            Value::Cells(cells) => cells.iter().flat_map(|c| c.to_be_bytes()).collect(),
            Value::Bytes(b) => b.clone(),
            Value::Empty => Vec::new(),
        }
    }
}

/// A node: properties in insertion order, then child nodes (the kernel does not care, but a
/// stable order keeps generated trees reproducible).
#[derive(Default)]
pub struct Node {
    pub properties: Vec<(String, Value)>,
    pub children: Vec<(String, Node)>,
}

impl Node {
    pub fn prop(&mut self, name: &str, value: Value) -> &mut Self {
        self.properties.push((name.to_string(), value));
        self
    }
    pub fn child(&mut self, name: &str) -> &mut Node {
        self.children.push((name.to_string(), Node::default()));
        &mut self.children.last_mut().unwrap().1
    }
}

struct Writer {
    bytes: Vec<u8>,
    /// name -> offsets of the property headers that refer to it (patched at the end)
    strings: Vec<(String, Vec<usize>)>,
}

impl Writer {
    fn align(&mut self, alignment: usize) {
        while self.bytes.len() % alignment != 0 {
            self.bytes.push(0);
        }
    }
    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }
    fn patch_u32(&mut self, at: usize, value: u32) {
        self.bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
    }
    fn string_ref(&mut self, name: &str, header_at: usize) {
        match self.strings.iter_mut().find(|(s, _)| s == name) {
            Some((_, refs)) => refs.push(header_at),
            None => self.strings.push((name.to_string(), vec![header_at])),
        }
    }

    fn walk(&mut self, node: &Node, name: &str) {
        assert!(name.len() <= NAME_MAX, "device tree node name too long: {name}");
        self.align(4);
        self.u32(FDT_BEGIN_NODE);
        self.bytes.extend_from_slice(name.as_bytes());
        self.bytes.push(0);
        self.align(4);

        for (prop_name, value) in &node.properties {
            assert!(prop_name.len() <= NAME_MAX, "device tree property name too long: {prop_name}");
            self.align(4);
            self.u32(FDT_PROP);
            let encoded = value.encode();
            self.u32(encoded.len() as u32);
            let nameoff_at = self.bytes.len();
            self.u32(0); // patched with the string table offset below
            self.string_ref(prop_name, nameoff_at);
            self.bytes.extend_from_slice(&encoded);
            self.align(4);
        }
        for (child_name, child) in &node.children {
            self.walk(child, child_name);
        }
        self.align(4);
        self.u32(FDT_END_NODE);
    }
}

/// Serializes `root` into a device tree blob, reserving the given memory ranges.
pub fn generate(root: &Node, memory_reservations: &[(u64, u64)], boot_cpu_id: u32) -> Vec<u8> {
    let mut w = Writer { bytes: Vec::with_capacity(4096), strings: Vec::new() };
    w.bytes.resize(40, 0); // the header, filled in at the end
    w.u32(0); // (header is 40 bytes: magic .. size_dt_struct)
    w.bytes.truncate(40);

    w.align(8);
    let off_mem_rsvmap = w.bytes.len();
    for (address, size) in memory_reservations {
        w.bytes.extend_from_slice(&address.to_be_bytes());
        w.bytes.extend_from_slice(&size.to_be_bytes());
    }
    w.bytes.extend_from_slice(&0u64.to_be_bytes());
    w.bytes.extend_from_slice(&0u64.to_be_bytes());

    let off_dt_struct = w.bytes.len();
    w.walk(root, "");
    w.u32(FDT_END);
    let size_dt_struct = w.bytes.len() - off_dt_struct;

    let off_dt_strings = w.bytes.len();
    let strings = std::mem::take(&mut w.strings);
    for (name, refs) in strings {
        let offset = w.bytes.len() - off_dt_strings;
        w.bytes.extend_from_slice(name.as_bytes());
        w.bytes.push(0);
        for at in refs {
            w.patch_u32(at, offset as u32);
        }
    }
    let size_dt_strings = w.bytes.len() - off_dt_strings;

    let total = w.bytes.len();
    for (at, value) in [
        (0, FDT_MAGIC),
        (4, total as u32),
        (8, off_dt_struct as u32),
        (12, off_dt_strings as u32),
        (16, off_mem_rsvmap as u32),
        (20, 17),
        (24, 16),
        (28, boot_cpu_id),
        (32, size_dt_strings as u32),
        (36, size_dt_struct as u32),
    ] {
        w.patch_u32(at, value);
    }
    w.bytes
}
