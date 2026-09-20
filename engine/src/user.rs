//! Running guest userspace programs (`binfmt_wasm`).
//!
//! A guest program is itself a wasm module: the kernel streams its bytes to the host
//! (`user.compile_*`), the host compiles it and gives it its own memory, and the kernel then
//! enters it (`user.call` / `user.switch_entry`). The program's only imports are the kernel's
//! syscall entry points, so a syscall is a call back into the kernel instance on this same
//! thread. A port of `user_imports` in the JavaScript host's worker.
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{anyhow, bail, Context as _, Result};
use wasmtime::{AsContextMut, Caller, Func, Instance, Linker, Module, Ref, SharedMemory, Val};

use crate::machine::{guest_bytes, HostState};

/// Errno values the kernel expects back from these imports.
const ENOMEM: i32 = -12;
const EFAULT: i32 = -14;
const EINVAL: i32 = -22;
const ENOSYS: i32 = -38;
const ENOEXEC: i32 = -8;

/// The program a thread is running: its module and its memory.
#[derive(Clone)]
pub struct UserContext {
    pub module: Module,
    pub memory: SharedMemory,
    pub maximum_pages: u64,
}

/// Per-thread userspace state.
#[derive(Default)]
pub struct User {
    pub context: Option<UserContext>,
    pub instance: Option<Instance>,
    pending_bytes: Option<Vec<u8>>,
    pending_context: Option<UserContext>,
    /// Entry point for the next `user.call`: the program's `_start`, or a thread function.
    pub entry: Entry,
    /// Bumped by every `instantiate`, so a syscall can tell that execve replaced the program.
    generation: u64,
    /// One slot per nested SA_SIGINFO handler; the trampoline's `copy_siginfo` result.
    siginfo_results: Vec<Option<i32>>,
}

#[derive(Default, Clone, Copy)]
pub enum Entry {
    #[default]
    Start,
    Function {
        function: u32,
        arg: u32,
    },
}

/// Thrown to unwind out of a user program whose instance was replaced (execve) — not an error.
#[derive(Debug)]
pub struct HaltUser;
impl std::fmt::Display for HaltUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "user program replaced")
    }
}
impl std::error::Error for HaltUser {}

fn copy_bytes(to: &SharedMemory, to_at: u32, from: &SharedMemory, from_at: u32, length: u32) -> i32 {
    let (Some(source), Some(_)) = (guest_bytes(from, from_at, length), guest_bytes(to, to_at, length)) else {
        return length as i32;
    };
    let destination = to.data();
    let start = to_at as usize;
    for (index, byte) in source.iter().enumerate() {
        unsafe { *destination[start + index].get() = *byte };
    }
    0
}

/// A 4-byte aligned word in the program's memory, as an atomic.
fn user_word<'a>(context: &'a UserContext, address: u32) -> Option<&'a AtomicI32> {
    if address % 4 != 0 {
        return None;
    }
    let data = context.memory.data();
    let end = address as usize + 4;
    if end > data.len() {
        return None;
    }
    // Shared memory is by definition concurrently accessed; the guest's own atomics use the
    // same words, so this pointer is only ever read and written atomically here.
    Some(unsafe { &*(data[address as usize].get() as *const AtomicI32) })
}

fn write_kernel_u32(memory: &SharedMemory, address: u32, value: u32) -> bool {
    let data = memory.data();
    let end = address as usize + 4;
    if end > data.len() {
        return false;
    }
    for (index, byte) in value.to_le_bytes().iter().enumerate() {
        unsafe { *data[address as usize + index].get() = *byte };
    }
    true
}

/// Only these imports may appear in a guest program; anything else is not a valid program here.
fn imports_supported(module: &Module) -> bool {
    module.imports().all(|import| {
        matches!(
            (import.module(), import.name()),
            ("env", "memory") | ("linux", "syscall") | ("linux", "get_thread_area") | ("linux", "copy_siginfo")
        )
    })
}

/// Compiles the streamed program and gives it a memory of its own.
pub fn compile_end(state: &mut HostState, rlimit_pages: u32) -> i32 {
    let Some(bytes) = state.user.pending_bytes.take() else { return EINVAL };
    let engine = state.shared.engine.clone();

    let Ok((shared, minimum, maximum)) = crate::module_info::memory_import(&bytes).map(|(info, _)| {
        (info.is_shared, info.minimum_pages, info.maximum_pages.unwrap_or(0))
    }) else {
        return ENOEXEC;
    };
    if !shared || maximum == 0 {
        eprintln!("[collabo-core] the guest program needs a shared memory with a maximum (shared={shared}, max={maximum})");
        return ENOEXEC;
    }
    // The same program is spawned over and over (every shell command is busybox again), and
    // compiling it costs about a second, so keep what has been compiled.
    let key = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.len().hash(&mut hasher);
        bytes.hash(&mut hasher);
        hasher.finish()
    };
    let cached = state.shared.programs.lock().unwrap().get(&key).cloned();
    let module = match cached {
        Some(module) => module,
        None => match Module::new(&engine, &bytes) {
            Ok(module) => {
                state.shared.programs.lock().unwrap().insert(key, module.clone());
                module
            }
            Err(error) => {
                eprintln!("[collabo-core] the guest program does not compile: {error:?}");
                return ENOEXEC;
            }
        },
    };
    if !imports_supported(&module) {
        eprintln!("[collabo-core] the guest program imports something the sandbox does not provide");
        return ENOEXEC;
    }
    let maximum = maximum.min(rlimit_pages as u64);
    if maximum < minimum {
        return ENOMEM;
    }
    let memory = match SharedMemory::new(&engine, wasmtime::MemoryType::shared(minimum as u32, maximum as u32)) {
        Ok(memory) => memory,
        Err(error) => {
            eprintln!("[collabo-core] the guest program's memory ({minimum}..{maximum} pages) could not be allocated: {error}");
            return ENOMEM;
        }
    };
    state.user.pending_context = Some(UserContext { module, memory, maximum_pages: maximum });
    0
}

pub fn compile_begin(state: &mut HostState, size: u32) -> i32 {
    state.user.pending_bytes = None;
    state.user.pending_context = None;
    // A program larger than the guest's own memory cannot be real; refuse before allocating.
    if size as u64 > 512 * 1024 * 1024 {
        return ENOMEM;
    }
    state.user.pending_bytes = Some(vec![0; size as usize]);
    0
}

pub fn compile_write(state: &mut HostState, source: u32, offset: u32, length: u32) -> i32 {
    let kernel_memory = state.shared.memory.clone();
    let Some(buffer) = state.user.pending_bytes.as_mut() else { return EINVAL };
    let Some(bytes) = guest_bytes(&kernel_memory, source, length) else { return EINVAL };
    let (start, end) = (offset as usize, offset as usize + length as usize);
    if end > buffer.len() {
        return EINVAL;
    }
    buffer[start..end].copy_from_slice(&bytes);
    0
}

pub fn compile_abort(state: &mut HostState) {
    state.user.pending_bytes = None;
    state.user.pending_context = None;
}

/// Creates the program's instance. Its syscall import calls back into the kernel instance.
pub fn instantiate(mut store: impl AsContextMut<Data = HostState>, kernel: &Instance, fresh_memory: bool) -> Result<()> {
    if fresh_memory {
        let pending = store.as_context_mut().data_mut().user.pending_context.take().context("no program was compiled")?;
        store.as_context_mut().data_mut().user.context = Some(pending);
    }
    let context = store.as_context().data().user.context.clone().context("no program to instantiate")?;
    let engine = store.as_context().data().shared.engine.clone();
    let mut linker: Linker<HostState> = Linker::new(&engine);
    linker.define(store.as_context(), "env", "memory", context.memory.clone())?;

    let syscall = kernel.get_typed_func::<(i32, i32, i32, i32, i32, i32, i32), i32>(store.as_context_mut(), "syscall")?;
    let get_thread_area = kernel.get_typed_func::<(), i32>(store.as_context_mut(), "get_thread_area").ok();
    let copy_siginfo = kernel.get_typed_func::<i32, i32>(store.as_context_mut(), "copy_siginfo")?;

    linker.func_wrap(
        "linux",
        "syscall",
        move |mut caller: Caller<'_, HostState>, nr: i32, a0: i32, a1: i32, a2: i32, a3: i32, a4: i32, a5: i32| -> Result<i32> {
            let before = caller.data().user.generation;
            let result = syscall.call(&mut caller, (nr, a0, a1, a2, a3, a4, a5))?;
            // execve replaced the program under us: leave the old instance's frames.
            if caller.data().user.generation != before {
                caller.data_mut().user.entry = Entry::Start;
                return Err(anyhow!(HaltUser));
            }
            Ok(result)
        },
    )?;
    if let Some(get_thread_area) = get_thread_area {
        linker.func_wrap("linux", "get_thread_area", move |mut caller: Caller<'_, HostState>| -> Result<i32> {
            get_thread_area.call(&mut caller, ())
        })?;
    }
    linker.func_wrap("linux", "copy_siginfo", move |mut caller: Caller<'_, HostState>, to: i32| -> Result<i32> {
        let result = copy_siginfo.call(&mut caller, to)?;
        if let Some(slot) = caller.data_mut().user.siginfo_results.last_mut() {
            *slot = Some(result);
        }
        Ok(result)
    })?;

    let instance = linker.instantiate(store.as_context_mut(), &context.module)?;
    let mut context = store.as_context_mut();
    let state = context.data_mut();
    state.user.instance = Some(instance);
    state.user.generation += 1;
    Ok(())
}

/// Calls a guest function whatever its result type is: a thread entry point or a signal handler
/// may or may not return a value, and the kernel does not care either way.
fn call_dynamic(mut store: impl AsContextMut<Data = HostState>, function: Func, args: &[Val]) -> Result<()> {
    let results = function.ty(store.as_context()).results().len();
    let mut out = vec![Val::I32(0); results];
    function.call(store.as_context_mut(), args, &mut out)?;
    Ok(())
}

fn table_function(mut store: impl AsContextMut<Data = HostState>, instance: &Instance, index: u32) -> Result<wasmtime::Func> {
    let table = instance
        .get_table(store.as_context_mut(), "__indirect_function_table")
        .context("the program has no function table")?;
    match table.get(store.as_context_mut(), index as u64) {
        Some(Ref::Func(Some(function))) => Ok(function),
        _ => bail!("no function at the program's table index {index}"),
    }
}

/// Runs the program until it exits or is replaced. Returns when the kernel thread should stop.
pub fn call(mut store: impl AsContextMut<Data = HostState>) -> Result<()> {
    loop {
        let instance = store.as_context().data().user.instance.context("no user program is running")?;
        let entry = store.as_context().data().user.entry;
        let outcome = match entry {
            Entry::Start => instance
                .get_typed_func::<(), ()>(store.as_context_mut(), "_start")
                .context("the program has no _start")
                .and_then(|start| start.call(store.as_context_mut(), ()).map_err(Into::into)),
            Entry::Function { function, arg } => table_function(store.as_context_mut(), &instance, function)
                .and_then(|function| call_dynamic(store.as_context_mut(), function, &[Val::I32(arg as i32)])),
        };
        match outcome {
            // A program that returns instead of exiting is a bug in the program, not the host.
            Ok(()) => return Ok(()),
            Err(error) if error.chain().any(|cause| cause.is::<HaltUser>()) => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Registers `user.*`. `store_kernel` hands back the kernel instance of this thread, which is
/// only known after instantiation.
pub fn link(linker: &mut Linker<HostState>) -> Result<()> {
    linker.func_wrap("user", "compile_begin", |mut caller: Caller<'_, HostState>, size: i32| -> i32 {
        compile_begin(caller.data_mut(), size as u32)
    })?;
    linker.func_wrap(
        "user",
        "compile_write",
        |mut caller: Caller<'_, HostState>, source: i32, offset: i32, length: i32| -> i32 {
            compile_write(caller.data_mut(), source as u32, offset as u32, length as u32)
        },
    )?;
    linker.func_wrap("user", "compile_end", |mut caller: Caller<'_, HostState>, pages: i32| -> i32 {
        compile_end(caller.data_mut(), pages as u32)
    })?;
    linker.func_wrap("user", "compile_abort", |mut caller: Caller<'_, HostState>| {
        compile_abort(caller.data_mut());
    })?;
    linker.func_wrap("user", "switch_entry", |mut caller: Caller<'_, HostState>, function: i32, arg: i32| {
        caller.data_mut().user.entry = Entry::Function { function: function as u32, arg: arg as u32 };
    })?;

    linker.func_wrap("user", "read", |caller: Caller<'_, HostState>, to: i32, from: i32, n: i32| -> i32 {
        let state = caller.data();
        let Some(context) = state.user.context.as_ref() else { return n };
        copy_bytes(&state.shared.memory, to as u32, &context.memory, from as u32, n as u32)
    })?;
    linker.func_wrap("user", "write", |caller: Caller<'_, HostState>, to: i32, from: i32, n: i32| -> i32 {
        let state = caller.data();
        let Some(context) = state.user.context.as_ref() else { return n };
        copy_bytes(&context.memory, to as u32, &state.shared.memory, from as u32, n as u32)
    })?;
    linker.func_wrap("user", "write_zeroes", |caller: Caller<'_, HostState>, to: i32, n: i32| -> i32 {
        let state = caller.data();
        let Some(context) = state.user.context.as_ref() else { return n };
        let length = n as u32;
        if guest_bytes(&context.memory, to as u32, length).is_none() {
            return n;
        }
        let data = context.memory.data();
        for index in 0..length as usize {
            unsafe { *data[to as usize + index].get() = 0 };
        }
        0
    })?;

    linker.func_wrap(
        "user",
        "futex_atomic_op",
        |caller: Caller<'_, HostState>, oldval: i32, uaddr: i32, op: i32, oparg: i32| -> i32 {
            let state = caller.data();
            let Some(context) = state.user.context.as_ref() else { return EFAULT };
            let Some(word) = user_word(context, uaddr as u32) else { return EFAULT };
            let old = match op {
                0 => word.swap(oparg, Ordering::SeqCst),                  // FUTEX_OP_SET
                1 => word.fetch_add(oparg, Ordering::SeqCst),             // FUTEX_OP_ADD
                2 => word.fetch_or(oparg, Ordering::SeqCst),              // FUTEX_OP_OR
                3 => word.fetch_and(!oparg, Ordering::SeqCst),            // FUTEX_OP_ANDN
                4 => word.fetch_xor(oparg, Ordering::SeqCst),             // FUTEX_OP_XOR
                _ => return ENOSYS,
            };
            if write_kernel_u32(&state.shared.memory, oldval as u32, old as u32) { 0 } else { EFAULT }
        },
    )?;
    linker.func_wrap(
        "user",
        "futex_atomic_cmpxchg",
        |caller: Caller<'_, HostState>, oldval: i32, uaddr: i32, expected: i32, replacement: i32| -> i32 {
            let state = caller.data();
            let Some(context) = state.user.context.as_ref() else { return EFAULT };
            let Some(word) = user_word(context, uaddr as u32) else { return EFAULT };
            let old = match word.compare_exchange(expected, replacement, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(previous) | Err(previous) => previous,
            };
            if write_kernel_u32(&state.shared.memory, oldval as u32, old as u32) { 0 } else { EFAULT }
        },
    )?;
    Ok(())
}

/// The three imports that must run the program's own code, so they need the store.
pub fn call_signal_handler(mut store: impl AsContextMut<Data = HostState>, function: u32, signal: i32) -> Result<()> {
    let instance = store.as_context().data().user.instance.context("no user program is running")?;
    let handler = table_function(store.as_context_mut(), &instance, function)?;
    call_dynamic(store.as_context_mut(), handler, &[Val::I32(signal)])
}

pub fn call_siginfo_handler(mut store: impl AsContextMut<Data = HostState>, trampoline: u32, function: i32, signal: i32) -> Result<i32> {
    let instance = store.as_context().data().user.instance.context("no user program is running")?;
    let entry = table_function(store.as_context_mut(), &instance, trampoline)?;
    store.as_context_mut().data_mut().user.siginfo_results.push(None);
    let called = call_dynamic(store.as_context_mut(), entry, &[Val::I32(function), Val::I32(signal)]);
    let result = store.as_context().data().user.siginfo_results.last().copied().flatten().unwrap_or(EINVAL);
    // The kernel's own cleanup must run even if the handler unwound.
    let kernel = store.as_context().data().kernel_instance.context("the kernel instance is gone")?;
    let clear = kernel.get_typed_func::<(), ()>(store.as_context_mut(), "clear_siginfo")?;
    let cleared = clear.call(store.as_context_mut(), ());
    store.as_context_mut().data_mut().user.siginfo_results.pop();
    called?;
    cleared?;
    Ok(result)
}

/// A private copy of a program's memory, for `fork()`. The parent is inside a host call, so
/// its memory is not changing under us.
pub fn copy_context(engine: &wasmtime::Engine, context: &UserContext) -> Result<UserContext> {
    let source = context.memory.data();
    let pages = (source.len() / 65536) as u32;
    let copy = SharedMemory::new(engine, wasmtime::MemoryType::shared(pages, context.maximum_pages as u32))?;
    let destination = copy.data();
    for (index, cell) in source.iter().enumerate() {
        unsafe { *destination[index].get() = *cell.get() };
    }
    Ok(UserContext { module: context.module.clone(), memory: copy, maximum_pages: context.maximum_pages })
}
