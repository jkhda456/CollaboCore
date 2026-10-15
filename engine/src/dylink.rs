//! `dlopen` for guest programs: shared libraries as WebAssembly side modules.
//!
//! A shared library here is a WebAssembly module of its own, linked with `wasm-ld -shared
//! --experimental-pic` (its `dylink.0` section says how much memory and how many table slots it
//! needs). Only the host can compile and instantiate a module, so the C library's dlopen
//! (userspace musl, `src/ldso/wasm32/dlopen.c`) reads the file, allocates the library's memory in
//! the program's own heap, and asks for the rest through the dylink calls answered here — syscalls
//! from 0x7d000000 on (`SYSCALL`), which the syscall import passes here first:
//!
//!   load(bytes, len, info) -> id          read it; its needs to `info`
//!   needed(id, i, buf, cap) -> len        the name of the i-th library it needs (DT_NEEDED)
//!   instantiate(id, memory_base, name, len, global) -> handle
//!                                         compile (cached as programs are), link and instantiate
//!                                         it in this thread; data relocations
//!   init(handle)                          its constructors (once what it needs is in and initialized)
//!   sym(handle, name, len, out) -> 0/1/-1 dlsym: an address, or a function's table index (1: a
//!                                         table slot was added, other threads must catch up)
//!   sync(out, cap) -> n                   catch this thread up; the libraries whose thread-local
//!                                         storage it must still set up, as (handle, size, align)
//!   init_tls(handle, block)               that library's `__wasm_init_tls` in this thread
//!   error(buf, cap) -> len                why the last call failed
//!
//! A library links against the program's exports — its functions, its data addresses (exported
//! as globals), the C library, the `__c_longjmp` tag — and those of the libraries loaded before
//! it with RTLD_GLOBAL, then its own: `GOT.mem` imports become the addresses of data, `GOT.func`
//! imports the table indices of functions: the index a function already has in the program's
//! table if it has one (so that function pointers compare equal), else a slot appended to the
//! table. Its own definitions come last, as on Linux, where one an earlier library or the
//! program has wins: wasm-ld imports what a library may define for others too (a weak
//! definition — C++'s inline functions and templates, operator new, its exception tag — even
//! with -Bsymbolic), and exports it. Such an import that nothing before it defines is bound to
//! the library's own definition: its calls, its references and its catch clauses rewritten to
//! name that instead (`rebind`), before it is compiled; the import itself is left, unused.
//!
//! Every thread of a process is an instance of the program in a store of its own, with a table
//! of its own. So a process keeps a log of what was loaded, in order — each library with how its
//! imports were resolved, and every table slot added — and each thread replays it (`sync`)
//! before it runs any code of a library: the table grows the same way in every thread, so a
//! function pointer one thread made is the same function in another. The libraries' data is
//! shared memory, initialized once (wasm-ld's `__wasm_init_memory` runs in every instance but
//! only the first does the work), as are their data relocations and constructors (the loading
//! thread runs them); thread-local storage is per thread (`__wasm_init_tls` with a block the C
//! library allocates). Nothing is ever unloaded.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context as _, Result};
use wasmtime::{
    Caller, Extern, ExternType, Func, Global, GlobalType, Instance, Module, Mutability, Ref, Table, Val, ValType,
};

use crate::machine::HostState;

/// Libraries of at least this size are kept compiled on disk too (program_cache.rs); a few
/// megabytes take the host a noticeable fraction of a second, every machine again.
const LIBRARY_LARGE: usize = 1 << 20;
const TABLE: &str = "__indirect_function_table";

/// What a library's `dylink.0` section says.
#[derive(Clone, Default)]
struct DylinkInfo {
    mem_size: u32,
    mem_align: u32,
    table_size: u32,
    table_align: u32,
    needed: Vec<String>,
    weak_imports: HashSet<(String, String)>,
}

/// Where a library's import comes from — the same in every thread.
#[derive(Clone)]
enum Source {
    Memory,
    Table,
    StackPointer,
    /// `__memory_base`, `__table_base`
    Const(i32),
    /// `GOT.mem` (an address) or `GOT.func` (a table index)
    Got(i32),
    Export(Provider),
    /// A weak function import nothing defines: a function that fails if called
    Missing(String),
    /// An import bound to the library's own definition (`rebind`): never used, any of its type
    Own,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Provider {
    Main(String),
    /// An export of the n-th library loaded
    Library(usize, String),
}

struct Library {
    name: String,
    module: Module,
    memory_base: u32,
    /// The table's size before it
    table_before: u32,
    /// The table's size once it and the slots its imports added are in
    table_after: u32,
    sources: Vec<Source>,
    /// Table slots its `GOT.func` imports added, with what they hold
    slots: Vec<(u32, Provider)>,
    global: bool,
    /// Its thread-local storage: size and alignment
    tls: Option<(u32, u32)>,
}

#[derive(Clone)]
enum Event {
    Load(Arc<Library>),
    /// A slot added later (dlsym of a function that had none)
    Slot(u32, Provider),
}

#[derive(Clone, Default)]
struct Log {
    events: Vec<Event>,
    libraries: Vec<Arc<Library>>,
    /// The table's size after every event (None: never grown, the program's own)
    table_size: Option<u32>,
    /// Functions that were given a slot, so that each keeps one index
    slotted: HashMap<Provider, u32>,
}

/// A process's libraries, shared by its threads (`UserContext`).
#[derive(Default)]
pub struct Process {
    log: Mutex<Log>,
}

impl Process {
    pub fn new() -> Arc<Process> {
        Arc::new(Process::default())
    }

    /// For a copy of the process (fork): the same libraries, at the same places in its copied memory.
    pub fn fork(&self) -> Arc<Process> {
        Arc::new(Process { log: Mutex::new(self.log.lock().unwrap().clone()) })
    }
}

struct Pending {
    bytes: Vec<u8>,
    info: DylinkInfo,
}

/// A thread's side of it (`User`): its instances of the libraries, as far as it has replayed the log.
#[derive(Default)]
pub struct Thread {
    applied: usize,
    instances: Vec<Instance>,
    tls_pending: Vec<usize>,
    loading: HashMap<i32, Pending>,
    next_id: i32,
    error: String,
    /// The program's table, function by function: its index of each (for GOT.func)
    main_index: Option<HashMap<usize, u32>>,
}

// ── dylink.0 ──────────────────────────────────────────────────────────────────────────────────

fn uleb(data: &[u8], pos: &mut usize) -> Result<u32> {
    let (mut result, mut shift) = (0u64, 0);
    loop {
        let byte = *data.get(*pos).context("truncated dylink.0")?;
        *pos += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return u32::try_from(result).map_err(|_| anyhow!("dylink.0: a number too large"));
        }
        shift += 7;
        if shift > 35 {
            bail!("dylink.0: a number too long");
        }
    }
}

fn name(data: &[u8], pos: &mut usize) -> Result<String> {
    let len = uleb(data, pos)? as usize;
    let bytes = data.get(*pos..*pos + len).context("truncated dylink.0")?;
    *pos += len;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn parse_dylink(bytes: &[u8]) -> Result<DylinkInfo> {
    let section = crate::module_info::custom_section(bytes, "dylink.0")
        .map_err(|_| anyhow!("not a shared library: no dylink.0 section (link it with wasm-ld -shared --experimental-pic)"))?;
    let data = section.as_slice();
    let mut info = DylinkInfo::default();
    let mut pos = 0;
    while pos < data.len() {
        let kind = data[pos];
        pos += 1;
        let len = uleb(data, &mut pos)? as usize;
        let end = pos + len;
        let mut p = pos;
        match kind {
            1 => {
                info.mem_size = uleb(data, &mut p)?;
                info.mem_align = 1u32.checked_shl(uleb(data, &mut p)?).unwrap_or(0);
                info.table_size = uleb(data, &mut p)?;
                info.table_align = 1u32.checked_shl(uleb(data, &mut p)?).unwrap_or(1);
            }
            2 => {
                for _ in 0..uleb(data, &mut p)? {
                    info.needed.push(name(data, &mut p)?);
                }
            }
            4 => {
                for _ in 0..uleb(data, &mut p)? {
                    let module = name(data, &mut p)?;
                    let field = name(data, &mut p)?;
                    let flags = uleb(data, &mut p)?;
                    if flags & 1 != 0 {
                        info.weak_imports.insert((module, field));
                    }
                }
            }
            _ => {}
        }
        pos = end;
    }
    Ok(info)
}

// ── a library's own definitions ───────────────────────────────────────────────────────────────

/// What a library's binary imports and exports, read before it is compiled: which of its imports
/// it defines itself.
#[derive(Default)]
struct Shape {
    /// `env` imports of functions and tags: (name, is a tag, its index among functions or tags)
    imports: Vec<(String, bool, u32)>,
    /// The type of every function and tag, imports first
    func_types: Vec<u32>,
    tag_types: Vec<u32>,
    /// Exported functions and tags: name -> (is a tag, index)
    exports: HashMap<String, (bool, u32)>,
}

fn shape(bytes: &[u8]) -> Result<Shape> {
    use wasmparser::{ExternalKind, Payload, TypeRef};
    let mut shape = Shape::default();
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        match payload? {
            Payload::ImportSection(reader) => {
                for import in reader {
                    let import = import?;
                    match import.ty {
                        TypeRef::Func(ty) => {
                            if import.module == "env" {
                                shape.imports.push((import.name.to_string(), false, shape.func_types.len() as u32));
                            }
                            shape.func_types.push(ty);
                        }
                        TypeRef::Tag(tag) => {
                            if import.module == "env" {
                                shape.imports.push((import.name.to_string(), true, shape.tag_types.len() as u32));
                            }
                            shape.tag_types.push(tag.func_type_idx);
                        }
                        _ => {}
                    }
                }
            }
            Payload::FunctionSection(reader) => {
                for ty in reader {
                    shape.func_types.push(ty?);
                }
            }
            Payload::TagSection(reader) => {
                for tag in reader {
                    shape.tag_types.push(tag?.func_type_idx);
                }
            }
            Payload::ExportSection(reader) => {
                for export in reader {
                    let export = export?;
                    match export.kind {
                        ExternalKind::Func => shape.exports.insert(export.name.to_string(), (false, export.index)),
                        ExternalKind::Tag => shape.exports.insert(export.name.to_string(), (true, export.index)),
                        _ => None,
                    };
                }
            }
            Payload::CodeSectionStart { .. } => break,
            _ => {}
        }
    }
    Ok(shape)
}

impl Shape {
    /// The library's own definition of an import of it (a function or tag index of the same
    /// type), if it is not the import itself
    fn own(&self, name: &str, tag: bool, index: u32) -> Option<u32> {
        let &(is_tag, own) = self.exports.get(name)?;
        let types = if tag { &self.tag_types } else { &self.func_types };
        (is_tag == tag && own != index && types.get(own as usize)? == types.get(index as usize)?).then_some(own)
    }
}

/// The library with references to some of its imported functions and tags made to its own
/// definitions instead (calls, `ref.func`, its table's elements, exports, `throw` and `catch`).
/// Nothing moves: the imports stay where they are, unused, so every index but those is the same.
fn rebind(bytes: &[u8], funcs: &HashMap<u32, u32>, tags: &HashMap<u32, u32>) -> Result<Vec<u8>> {
    use wasm_encoder::reencode::{Error, Reencode};
    struct Rebind<'a> {
        funcs: &'a HashMap<u32, u32>,
        tags: &'a HashMap<u32, u32>,
    }
    impl Reencode for Rebind<'_> {
        type Error = std::convert::Infallible;
        fn function_index(&mut self, func: u32) -> Result<u32, Error> {
            Ok(*self.funcs.get(&func).unwrap_or(&func))
        }
        fn tag_index(&mut self, tag: u32) -> Result<u32, Error> {
            Ok(*self.tags.get(&tag).unwrap_or(&tag))
        }
        // custom sections as they are: the names still fit (nothing moved), dylink.0 stays first
        fn parse_custom_section(&mut self, module: &mut wasm_encoder::Module, section: wasmparser::CustomSectionReader<'_>) -> Result<(), Error> {
            module.section(&wasm_encoder::CustomSection { name: section.name().into(), data: section.data().into() });
            Ok(())
        }
    }
    let mut module = wasm_encoder::Module::new();
    Rebind { funcs, tags }
        .parse_core_module(&mut module, wasmparser::Parser::new(0), bytes)
        .map_err(|error| anyhow!("it cannot be rewritten: {error:?}"))?;
    Ok(module.finish())
}

/// Compiles a library (cached by its bytes, as programs are; on disk too from a megabyte).
fn compile(caller: &Caller<'_, HostState>, bytes: &[u8]) -> Result<Module> {
    let engine = caller.data().shared.engine.clone();
    let key = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.len().hash(&mut hasher);
        bytes.hash(&mut hasher);
        hasher.finish()
    };
    if let Some(module) = caller.data().shared.programs.lock().unwrap().get(&key).cloned() {
        return Ok(module);
    }
    let module = crate::program_cache::load_or_compile_at(&engine, bytes, LIBRARY_LARGE)
        .map_err(|error| anyhow!("it does not compile: {error}"))?;
    caller.data().shared.programs.lock().unwrap().insert(key, module.clone());
    Ok(module)
}

// ── the guest's memory ────────────────────────────────────────────────────────────────────────

fn memory(caller: &Caller<'_, HostState>) -> Result<wasmtime::SharedMemory> {
    Ok(caller.data().user.context.as_ref().context("no program is running")?.memory.clone())
}

fn read(caller: &Caller<'_, HostState>, address: u32, len: u32) -> Result<Vec<u8>> {
    crate::machine::guest_bytes(&memory(caller)?, address, len).context("guest memory out of bounds")
}

fn write(caller: &Caller<'_, HostState>, address: u32, bytes: &[u8]) -> Result<()> {
    let memory = memory(caller)?;
    let data = memory.data();
    let end = address as usize + bytes.len();
    if end > data.len() {
        bail!("guest memory out of bounds");
    }
    for (index, byte) in bytes.iter().enumerate() {
        unsafe { *data[address as usize + index].get() = *byte };
    }
    Ok(())
}

fn process(caller: &Caller<'_, HostState>) -> Result<Arc<Process>> {
    Ok(caller.data().user.context.as_ref().context("no program is running")?.dylink.clone())
}

fn main_instance(caller: &Caller<'_, HostState>) -> Result<Instance> {
    caller.data().user.instance.context("no program is running")
}

fn main_table(caller: &mut Caller<'_, HostState>) -> Result<Table> {
    let main = main_instance(caller)?;
    main.get_table(&mut *caller, TABLE)
        .context("the program exports no function table (link it with --export-table --growable-table)")
}

// ── linking ───────────────────────────────────────────────────────────────────────────────────

/// An export of the program or of a library, in this thread.
fn export(caller: &mut Caller<'_, HostState>, provider: &Provider) -> Option<Extern> {
    match provider {
        Provider::Main(name) => main_instance(caller).ok()?.get_export(&mut *caller, name),
        Provider::Library(index, name) => {
            let instance = *caller.data().user.dylink.instances.get(*index)?;
            instance.get_export(&mut *caller, name)
        }
    }
}

/// Who defines `name` for a library being loaded: the program, then the global libraries.
fn find(caller: &mut Caller<'_, HostState>, log: &Log, name: &str, want: fn(&ExternType) -> bool) -> Option<Provider> {
    let main = main_instance(caller).ok()?;
    let module = caller.data().user.context.as_ref()?.module.clone();
    if module.get_export(name).is_some_and(|ty| want(&ty)) && main.get_export(&mut *caller, name).is_some() {
        return Some(Provider::Main(name.to_string()));
    }
    for (index, library) in log.libraries.iter().enumerate() {
        if library.global && library.module.get_export(name).is_some_and(|ty| want(&ty)) {
            return Some(Provider::Library(index, name.to_string()));
        }
    }
    None
}

fn is_func(ty: &ExternType) -> bool {
    matches!(ty, ExternType::Func(_))
}
fn is_global(ty: &ExternType) -> bool {
    matches!(ty, ExternType::Global(_))
}
fn is_tag(ty: &ExternType) -> bool {
    matches!(ty, ExternType::Tag(_))
}

/// The address a data export stands for: the program's are absolute, a library's relative to its memory.
fn data_address(caller: &mut Caller<'_, HostState>, log: &Log, provider: &Provider) -> Option<i32> {
    let global = export(caller, provider)?.into_global()?;
    let value = global.get(&mut *caller).i32()?;
    match provider {
        Provider::Main(_) => Some(value),
        Provider::Library(index, _) => Some(log.libraries[*index].memory_base as i32 + value),
    }
}

/// The table index of a function: the one it has in the program's table, else its own slot.
fn function_index(caller: &mut Caller<'_, HostState>, log: &mut Log, provider: &Provider, added: &mut Vec<(u32, Provider)>) -> Result<i32> {
    if let Some(index) = log.slotted.get(provider) {
        return Ok(*index as i32);
    }
    let func = export(caller, provider).and_then(Extern::into_func).context("not a function")?;
    if let Provider::Main(_) = provider {
        if caller.data().user.dylink.main_index.is_none() {
            let table = main_table(caller)?;
            let module = caller.data().user.context.as_ref().context("no program")?.module.clone();
            // the program's own entries: what its elem segments put there
            let own = module.exports().find(|e| e.name() == TABLE).and_then(|e| e.ty().table().map(|t| t.minimum()));
            let size = own.unwrap_or_else(|| table.size(&*caller)) as u32;
            let mut index = HashMap::new();
            for i in 0..size {
                if let Some(Ref::Func(Some(f))) = table.get(&mut *caller, i as u64) {
                    index.entry(f.to_raw(&mut *caller) as usize).or_insert(i);
                }
            }
            caller.data_mut().user.dylink.main_index = Some(index);
        }
        let key = func.to_raw(&mut *caller) as usize;
        if let Some(index) = caller.data().user.dylink.main_index.as_ref().and_then(|m| m.get(&key)) {
            return Ok(*index as i32);
        }
    }
    let table = main_table(caller)?;
    let index = table.grow(&mut *caller, 1, Ref::Func(Some(func)))? as u32;
    log.slotted.insert(provider.clone(), index);
    added.push((index, provider.clone()));
    Ok(index as i32)
}

/// The externs a library's imports are, in this thread (for its first instance and every replay).
fn externs(caller: &mut Caller<'_, HostState>, module: &Module, sources: &[Source]) -> Result<Vec<Extern>> {
    let main = main_instance(caller)?;
    let mut out = Vec::with_capacity(sources.len());
    for (import, source) in module.imports().zip(sources) {
        let ext = match source {
            Source::Memory => Extern::SharedMemory(memory(caller)?),
            Source::Table => Extern::Table(main_table(caller)?),
            Source::StackPointer => Extern::Global(
                main.get_global(&mut *caller, "__stack_pointer").context("the program exports no __stack_pointer")?,
            ),
            Source::Const(value) => {
                Extern::Global(Global::new(&mut *caller, GlobalType::new(ValType::I32, Mutability::Const), Val::I32(*value))?)
            }
            Source::Got(value) => {
                Extern::Global(Global::new(&mut *caller, GlobalType::new(ValType::I32, Mutability::Var), Val::I32(*value))?)
            }
            Source::Export(provider) => export(caller, provider)
                .with_context(|| format!("{} {} is gone", import.module(), import.name()))?,
            Source::Missing(name) => {
                let ExternType::Func(ty) = import.ty() else { bail!("{name}: a weak import that is no function") };
                let name = name.clone();
                Extern::Func(Func::new(&mut *caller, ty, move |_, _, _| {
                    bail!("{name} was called: a weak symbol no library defines")
                }))
            }
            Source::Own => match import.ty() {
                ExternType::Func(ty) => Extern::Func(Func::new(&mut *caller, ty, |_, _, _| bail!("a rebound import was called"))),
                ExternType::Tag(ty) => Extern::Tag(wasmtime::Tag::new(&mut *caller, &ty)?),
                _ => bail!("{}: a rebound import that is no function or tag", import.name()),
            },
        };
        out.push(ext);
    }
    Ok(out)
}

/// Instantiates `library` in this thread (a replay of its load), its slots filled.
fn replay(caller: &mut Caller<'_, HostState>, library: &Library) -> Result<Instance> {
    let table = main_table(caller)?;
    let size = table.size(&*caller) as u32;
    if size != library.table_before {
        bail!("{}: this thread's table has {size} entries, the process's {}", library.name, library.table_before);
    }
    table.grow(&mut *caller, u64::from(library.table_after - size), Ref::Func(None))?;
    let ext = externs(caller, &library.module, &library.sources)?;
    let instance = Instance::new(&mut *caller, &library.module, &ext).with_context(|| library.name.clone())?;
    caller.data_mut().user.dylink.instances.push(instance);
    for (index, provider) in &library.slots {
        let func = export(caller, provider).and_then(Extern::into_func).context("a slot's function is gone")?;
        table.set(&mut *caller, u64::from(*index), Ref::Func(Some(func)))?;
    }
    Ok(instance)
}

/// Brings this thread's instances up to the process's log.
fn catch_up(caller: &mut Caller<'_, HostState>, log: &Log) -> Result<()> {
    while caller.data().user.dylink.applied < log.events.len() {
        let at = caller.data().user.dylink.applied;
        match &log.events[at] {
            Event::Load(library) => {
                replay(caller, library)?;
                if library.tls.is_some() {
                    let index = caller.data().user.dylink.instances.len() - 1;
                    caller.data_mut().user.dylink.tls_pending.push(index);
                }
            }
            Event::Slot(index, provider) => {
                let table = main_table(caller)?;
                let size = table.size(&*caller) as u32;
                if *index >= size {
                    table.grow(&mut *caller, u64::from(*index + 1 - size), Ref::Func(None))?;
                }
                let func = export(caller, provider).and_then(Extern::into_func).context("a slot's function is gone")?;
                table.set(&mut *caller, u64::from(*index), Ref::Func(Some(func)))?;
            }
        }
        caller.data_mut().user.dylink.applied += 1;
    }
    Ok(())
}

fn instantiate(caller: &mut Caller<'_, HostState>, id: i32, memory_base: u32, name: String, global: bool) -> Result<i32> {
    let Pending { bytes, info } = caller.data_mut().user.dylink.loading.remove(&id).context("no such library loading")?;
    let process = process(caller)?;
    let mut log = process.log.lock().unwrap();
    catch_up(caller, &log)?;
    // what it defines itself that the program and the libraries before it do not: its own, now
    // that those it needs are in (and the binding what it is compiled with)
    let shape = shape(&bytes).with_context(|| name.clone())?;
    let (mut funcs, mut tags) = (HashMap::new(), HashMap::new());
    for (sym, tag, index) in &shape.imports {
        let want = if *tag { is_tag } else { is_func };
        if let Some(own) = shape.own(sym, *tag, *index) {
            if find(caller, &log, sym, want).is_none() {
                if *tag { tags.insert(*index, own) } else { funcs.insert(*index, own) };
            }
        }
    }
    let module = if funcs.is_empty() && tags.is_empty() {
        compile(caller, &bytes)
    } else {
        compile(caller, &rebind(&bytes, &funcs, &tags).with_context(|| name.clone())?)
    }
    .with_context(|| name.clone())?;
    drop(bytes);
    let rebound: HashSet<(bool, String)> = shape
        .imports
        .iter()
        .filter(|(_, tag, index)| if *tag { tags.contains_key(index) } else { funcs.contains_key(index) })
        .map(|(sym, tag, _)| (*tag, sym.clone()))
        .collect();
    let table = main_table(caller)?;
    let before = table.size(&*caller) as u32;
    let align = info.table_align.max(1);
    let table_base = before.div_ceil(align) * align;
    table.grow(&mut *caller, u64::from(table_base - before + info.table_size), Ref::Func(None))?;

    let mut sources = Vec::new();
    let mut slots = Vec::new();
    // its own symbols, which are known once it exists: (import position, export name, is a function)
    let mut own = Vec::new();
    for (position, import) in module.imports().enumerate() {
        let (module_name, field) = (import.module().to_string(), import.name().to_string());
        // (wasm-ld marks the env import of a weak symbol; its GOT entry is as weak)
        let weak = info.weak_imports.contains(&(module_name.clone(), field.clone()))
            || (module_name.starts_with("GOT.") && info.weak_imports.contains(&("env".to_string(), field.clone())));
        let source = match (module_name.as_str(), field.as_str(), import.ty()) {
            ("env", "memory", _) => Source::Memory,
            ("env", TABLE, _) => Source::Table,
            ("env", "__stack_pointer", _) => Source::StackPointer,
            ("env", "__memory_base", _) => Source::Const(memory_base as i32),
            ("env", "__table_base", _) => Source::Const(table_base as i32),
            ("GOT.mem", sym, _) => match find(caller, &log, sym, is_global) {
                Some(provider) => Source::Got(data_address(caller, &log, &provider).with_context(|| format!("{sym}: no address"))?),
                None if module.get_export(sym).is_some_and(|t| is_global(&t)) => {
                    own.push((position, sym.to_string(), false));
                    Source::Got(0)
                }
                // a token only its address matters for (C++'s atexit registrations)
                None if sym == "__dso_handle" => Source::Got(memory_base as i32),
                None if weak => Source::Got(0),
                None => bail!("{name}: undefined symbol: {sym} (data)"),
            },
            ("GOT.func", sym, _) => match find(caller, &log, sym, is_func) {
                Some(provider) => Source::Got(function_index(caller, &mut log, &provider, &mut slots)?),
                None if module.get_export(sym).is_some_and(|t| is_func(&t)) => {
                    own.push((position, sym.to_string(), true));
                    Source::Got(0)
                }
                None if weak => Source::Got(0),
                None => bail!("{name}: undefined symbol: {sym} (function)"),
            },
            ("env", sym, ExternType::Func(_)) if rebound.contains(&(false, sym.to_string())) => Source::Own,
            ("env", sym, ExternType::Tag(_)) if rebound.contains(&(true, sym.to_string())) => Source::Own,
            ("env", sym, ExternType::Func(_)) => match find(caller, &log, sym, is_func) {
                Some(provider) => Source::Export(provider),
                None if weak => Source::Missing(sym.to_string()),
                None => bail!("{name}: undefined symbol: {sym}"),
            },
            ("env", sym, ExternType::Tag(_)) => match find(caller, &log, sym, is_tag) {
                Some(provider) => Source::Export(provider),
                None => bail!("{name}: undefined tag: {sym}"),
            },
            ("env", sym, ExternType::Global(_)) => match find(caller, &log, sym, is_global) {
                Some(provider) => Source::Export(provider),
                None => bail!("{name}: undefined global: {sym}"),
            },
            (m, f, _) => bail!("{name}: unsupported import {m}.{f}"),
        };
        sources.push(source);
    }

    let ext = externs(caller, &module, &sources)?;
    let instance = Instance::new(&mut *caller, &module, &ext).with_context(|| name.clone())?;
    let index = log.libraries.len();
    // its own symbols through its GOT: now that it exists
    for (position, sym, is_function) in own {
        let value = if is_function {
            let provider = Provider::Library(index, sym.clone());
            let func = instance.get_func(&mut *caller, &sym).context("own function")?;
            let at = table.grow(&mut *caller, 1, Ref::Func(Some(func)))? as u32;
            log.slotted.insert(provider.clone(), at);
            slots.push((at, provider));
            at as i32
        } else {
            let global = instance.get_global(&mut *caller, &sym).context("own data")?;
            memory_base as i32 + global.get(&mut *caller).i32().context("own data")?
        };
        if let Extern::Global(got) = &ext[position] {
            got.set(&mut *caller, Val::I32(value))?;
        }
        sources[position] = Source::Got(value);
    }
    if let Some(relocate) = instance.get_func(&mut *caller, "__wasm_apply_data_relocs") {
        relocate.call(&mut *caller, &[], &mut [])?;
    }
    let tls = match (instance.get_global(&mut *caller, "__tls_size"), instance.get_global(&mut *caller, "__tls_align")) {
        (Some(size), Some(align)) => {
            let size = size.get(&mut *caller).i32().unwrap_or(0) as u32;
            let align = align.get(&mut *caller).i32().unwrap_or(1) as u32;
            (size > 0).then_some((size, align.max(1)))
        }
        _ => None,
    };
    let library = Arc::new(Library {
        name,
        module,
        memory_base,
        table_before: before,
        table_after: table.size(&*caller) as u32,
        sources,
        slots,
        global,
        tls,
    });
    // the slots it added for its imports were part of its growth: replays grow to table_after, then fill
    log.libraries.push(library.clone());
    log.events.push(Event::Load(library));
    log.table_size = Some(table.size(&*caller) as u32);
    let thread = &mut caller.data_mut().user.dylink;
    thread.instances.push(instance);
    thread.applied = log.events.len();
    Ok(index as i32 + 1)
}

fn sym(caller: &mut Caller<'_, HostState>, handle: i32, name: &str) -> Result<Option<(i32, bool)>> {
    let process = process(caller)?;
    let mut log = process.log.lock().unwrap();
    catch_up(caller, &log)?;
    let mut candidates = Vec::new();
    if handle == 0 {
        let module = caller.data().user.context.as_ref().context("no program")?.module.clone();
        if module.get_export(name).is_some() {
            candidates.push(Provider::Main(name.to_string()));
        }
        for (index, library) in log.libraries.iter().enumerate() {
            if library.global && library.module.get_export(name).is_some() {
                candidates.push(Provider::Library(index, name.to_string()));
            }
        }
    } else {
        let index = (handle - 1) as usize;
        let library = log.libraries.get(index).context("no such library")?;
        if library.module.get_export(name).is_some() {
            candidates.push(Provider::Library(index, name.to_string()));
        }
    }
    let Some(provider) = candidates.into_iter().next() else { return Ok(None) };
    let ty = match &provider {
        Provider::Main(n) => caller.data().user.context.as_ref().context("no program")?.module.get_export(n),
        Provider::Library(i, n) => log.libraries[*i].module.get_export(n),
    }
    .context("no export")?;
    match ty {
        ExternType::Func(_) => {
            let mut added = Vec::new();
            let index = function_index(caller, &mut log, &provider, &mut added)?;
            let grew = !added.is_empty();
            for (at, provider) in added {
                log.events.push(Event::Slot(at, provider));
            }
            if grew {
                log.table_size = Some(main_table(caller)?.size(&*caller) as u32);
                caller.data_mut().user.dylink.applied = log.events.len();
            }
            Ok(Some((index, grew)))
        }
        ExternType::Global(_) => Ok(data_address(caller, &log, &provider).map(|address| (address, false))),
        _ => Ok(None),
    }
}

fn sync(caller: &mut Caller<'_, HostState>, out: u32, cap: u32) -> Result<i32> {
    let process = process(caller)?;
    let log = process.log.lock().unwrap();
    catch_up(caller, &log)?;
    let pending: Vec<usize> = {
        let thread = &mut caller.data_mut().user.dylink;
        let n = thread.tls_pending.len().min(cap as usize);
        thread.tls_pending.drain(..n).collect()
    };
    let mut bytes = Vec::new();
    for index in &pending {
        let (size, align) = log.libraries[*index].tls.unwrap_or((0, 1));
        for value in [*index as u32 + 1, size, align] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    drop(log);
    write(caller, out, &bytes)?;
    Ok(pending.len() as i32)
}

/// Runs `body`; a failure is this thread's `dylink.error` and -1 for the guest. A trap of the
/// library's own code (its constructors) is the program's: it passes through.
fn guarded(caller: &mut Caller<'_, HostState>, body: impl FnOnce(&mut Caller<'_, HostState>) -> Result<i32>) -> Result<i32> {
    match body(caller) {
        Ok(value) => Ok(value),
        Err(error) if error.downcast_ref::<wasmtime::Trap>().is_some() || error.is::<wasmtime::ThrownException>() => Err(error),
        Err(error) => {
            caller.data_mut().user.dylink.error = format!("{error:#}");
            Ok(-1)
        }
    }
}

/// The C library's dylink calls: syscalls from `SYSCALL` on (`DL_*` in dlopen.c), numbers Linux
/// has none of. Not imports of their own, so that a program with dlopen runs on a host without
/// them too (its kernel answers these numbers with ENOSYS). None: not one of them.
pub const SYSCALL: i32 = 0x7d00_0000;

pub fn syscall(caller: &mut Caller<'_, HostState>, nr: i32, a: [i32; 5]) -> Option<Result<i32>> {
    let op = nr.wrapping_sub(SYSCALL);
    if !(0..8).contains(&op) {
        return None;
    }
    Some(match op {
        // load(bytes, len, info) -> id: read it (compiled when it is instantiated: how it binds to
        // itself depends on what is in by then); its needs to `info`
        0 => guarded(caller, |caller| {
            let bytes = read(caller, a[0] as u32, a[1] as u32)?;
            let dylink = parse_dylink(&bytes)?;
            let mut words = Vec::new();
            for value in [dylink.mem_size, dylink.mem_align.max(1), dylink.table_size, dylink.needed.len() as u32] {
                words.extend_from_slice(&value.to_le_bytes());
            }
            write(caller, a[2] as u32, &words)?;
            let thread = &mut caller.data_mut().user.dylink;
            thread.next_id += 1;
            let id = thread.next_id;
            thread.loading.insert(id, Pending { bytes, info: dylink });
            Ok(id)
        }),
        // needed(id, i, buf, cap) -> len
        1 => guarded(caller, |caller| {
            let (id, i, buf, cap) = (a[0], a[1], a[2], a[3]);
            let name = caller.data().user.dylink.loading.get(&id).and_then(|p| p.info.needed.get(i as usize)).cloned();
            let name = name.context("no such needed library")?;
            let bytes = name.as_bytes();
            write(caller, buf as u32, &bytes[..bytes.len().min(cap as usize)])?;
            Ok(bytes.len() as i32)
        }),
        // instantiate(id, memory_base, name, len, global) -> handle
        2 => guarded(caller, |caller| {
            let name = String::from_utf8_lossy(&read(caller, a[2] as u32, a[3] as u32)?).into_owned();
            instantiate(caller, a[0], a[1] as u32, name, a[4] != 0)
        }),
        // init(handle): its constructors
        3 => guarded(caller, |caller| {
            let instance = *caller.data().user.dylink.instances.get((a[0] - 1) as usize).context("no such library")?;
            if let Some(ctors) = instance.get_func(&mut *caller, "__wasm_call_ctors") {
                ctors.call(&mut *caller, &[], &mut [])?;
            }
            Ok(0)
        }),
        // sym(handle, name, len, out) -> 0, or 1 when a table slot was added
        4 => guarded(caller, |caller| {
            let name = String::from_utf8_lossy(&read(caller, a[1] as u32, a[2] as u32)?).into_owned();
            match sym(caller, a[0], &name)? {
                Some((value, grew)) => {
                    write(caller, a[3] as u32, &value.to_le_bytes())?;
                    Ok(grew as i32)
                }
                None => bail!("{name}: no such symbol"),
            }
        }),
        // sync(out, cap) -> n
        5 => guarded(caller, |caller| sync(caller, a[0] as u32, a[1] as u32)),
        // init_tls(handle, block)
        6 => guarded(caller, |caller| {
            let instance = *caller.data().user.dylink.instances.get((a[0] - 1) as usize).context("no such library")?;
            let init = instance.get_func(&mut *caller, "__wasm_init_tls").context("the library has no __wasm_init_tls")?;
            init.call(&mut *caller, &[Val::I32(a[1])], &mut [])?;
            Ok(0)
        }),
        // error(buf, cap) -> len
        _ => {
            let text = caller.data().user.dylink.error.clone().into_bytes();
            write(caller, a[0] as u32, &text[..text.len().min(a[1] as usize)]).map(|()| text.len() as i32)
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn dylink_section() {
        // MEM_INFO: 20 bytes aligned to 4, 2 table slots aligned to 1; NEEDED: libz.so
        let section = [1u8, 4, 20, 2, 2, 0, 2, 9, 1, 7, b'l', b'i', b'b', b'z', b'.', b's', b'o'];
        let mut module = b"\0asm\x01\0\0\0\0".to_vec();
        module.push((section.len() + 9) as u8);
        module.push(8);
        module.extend_from_slice(b"dylink.0");
        module.extend_from_slice(&section);
        let info = super::parse_dylink(&module).unwrap();
        assert_eq!((info.mem_size, info.mem_align, info.table_size, info.table_align), (20, 4, 2, 1));
        assert_eq!(info.needed, vec!["libz.so".to_string()]);
    }
}
