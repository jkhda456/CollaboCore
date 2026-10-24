//! Running guest userspace programs (`binfmt_wasm`).
//!
//! A guest program is itself a wasm module: the kernel streams its bytes to the host
//! (`user.compile_*`), the host compiles it and gives it its own memory, and the kernel then
//! enters it (`user.call` / `user.switch_entry`). The program's only imports are the kernel's
//! syscall entry points, so a syscall is a call back into the kernel instance on this same
//! thread. A port of `user_imports` in the JavaScript host's worker.
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{anyhow, bail, Context as _, Result};
use wasmtime::{AsContextMut, Caller, Func, Instance, Linker, Module, Ref, SharedMemory, StoreContextMut, TypedFunc, UpdateDeadline, Val};

use crate::machine::{guest_bytes, HostState};

/// Errno values the kernel expects back from these imports.
const ENOMEM: i32 = -12;
const EFAULT: i32 = -14;
const EINVAL: i32 = -22;
const EINTR: i32 = -4;
const ENOSYS: i32 = -38;
const ENOEXEC: i32 = -8;

/// The program a thread is running: its module and its memory, and the libraries it loaded
/// (dlopen: dylink.rs), shared by the threads of the process.
#[derive(Clone)]
pub struct UserContext {
    pub module: Module,
    pub memory: SharedMemory,
    pub maximum_pages: u64,
    pub dylink: std::sync::Arc<crate::dylink::Process>,
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
    /// This thread's instances of the process's libraries (dlopen).
    pub dylink: crate::dylink::Thread,
    /// The program's own code is running (not the kernel's): an epoch tick here is an interrupt
    /// from user mode ([`on_epoch`]). Set where the host enters the program, cleared where the
    /// program calls into the kernel, each restored on the way back.
    in_user: bool,
    /// The kernel's `user_interrupt`, once looked up (`Some(None)`: a kernel without it).
    interrupt: Option<Option<TypedFunc<(), ()>>>,
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

/// A trap in a guest program's own code (an out-of-bounds access, `unreachable` — abort paths
/// such as a failed assertion —, a stack overflow): that program ends, the machine goes on.
/// Raised from the signal handler calls, which run the program's code inside a system call, so
/// that the error passes the kernel's frames up to [`call`] as the program's own.
#[derive(Debug)]
pub struct UserTrapped;
impl std::fmt::Display for UserTrapped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the guest program trapped in a signal handler")
    }
}
impl std::error::Error for UserTrapped {}

/// An exception the program threw and nothing caught (a C++ `throw` with no handler left — on
/// WebAssembly no search phase calls std::terminate first —, or a longjmp to no setjmp): it
/// leaves the program's code as wasmtime's `ThrownException`, which ends the program as a trap
/// does. [`uncaught`] makes it this error and takes the exception off the store.
#[derive(Debug)]
pub struct UncaughtException;
impl std::fmt::Display for UncaughtException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "an exception nothing caught")
    }
}
impl std::error::Error for UncaughtException {}

/// A `ThrownException` out of the program's code as [`UncaughtException`], its exception taken
/// off the store (the next call must not find it pending); other errors as they are.
fn uncaught(mut store: impl AsContextMut<Data = HostState>, error: anyhow::Error) -> anyhow::Error {
    if !error.is::<wasmtime::ThrownException>() {
        return error;
    }
    store.as_context_mut().take_pending_exception();
    anyhow!(UncaughtException)
}

/// The kernel failed inside a guest program's system call: not the program's fault, and the
/// machine cannot go on.
#[derive(Debug)]
pub struct KernelFailed;
impl std::fmt::Display for KernelFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the kernel failed in a system call")
    }
}
impl std::error::Error for KernelFailed {}

/// Whether an error out of a guest program's code is that program's own trap.
fn is_user_trap(error: &anyhow::Error) -> bool {
    if error.chain().any(|cause| cause.is::<KernelFailed>()) {
        return false;
    }
    error.chain().any(|cause| cause.is::<UserTrapped>() || cause.is::<UncaughtException>())
        || error.downcast_ref::<wasmtime::Trap>().is_some()
}

/// Says on the host's stderr why a guest program ended (the guest itself sees SIGSEGV): the
/// trap, then the innermost frames of the program, repeats (a runaway recursion) collapsed.
fn report_user_trap(error: &anyhow::Error) {
    if error.chain().any(|cause| cause.is::<UncaughtException>()) {
        eprintln!("[collabo-core] a guest program threw an exception nothing caught and was ended (SIGSEGV)");
        return;
    }
    let causes: Vec<String> = error.chain().skip(1).map(|cause| cause.to_string()).collect();
    let mut text = format!("[collabo-core] a guest program trapped and was ended (SIGSEGV): {}", causes.join(": "));
    if let Some(trace) = error.downcast_ref::<wasmtime::WasmBacktrace>() {
        let frames: Vec<String> = trace
            .frames()
            .iter()
            .map(|frame| match frame.func_name() {
                Some(name) => format!("{name}"),
                None => format!("<wasm function {}>", frame.func_index()),
            })
            .collect();
        let (mut shown, mut index) = (0, 0);
        while index < frames.len() && shown < 16 {
            let mut repeats = 1;
            while index + repeats < frames.len() && frames[index + repeats] == frames[index] {
                repeats += 1;
            }
            text.push_str(&format!("\n    {}{}", frames[index], if repeats > 1 { format!(" (x{repeats})") } else { String::new() }));
            index += repeats;
            shown += 1;
        }
        if index < frames.len() {
            text.push_str("\n    ...");
        }
    }
    eprintln!("{text}");
}

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
pub fn compile_end(caller: &mut Caller<'_, HostState>, rlimit_pages: u32) -> Result<i32> {
    let state = caller.data_mut();
    let Some(bytes) = state.user.pending_bytes.take() else { return Ok(EINVAL) };
    let engine = state.shared.engine.clone();

    let Ok((shared, minimum, maximum)) = crate::module_info::memory_import(&bytes).map(|(info, _)| {
        (info.is_shared, info.minimum_pages, info.maximum_pages.unwrap_or(0))
    }) else {
        return Ok(ENOEXEC);
    };
    if !shared || maximum == 0 {
        eprintln!("[collabo-core] the guest program needs a shared memory with a maximum (shared={shared}, max={maximum})");
        return Ok(ENOEXEC);
    }
    // The same program is spawned over and over (every shell command is busybox again), and
    // compiling it costs about a second, so keep what has been compiled (a large program also on
    // disk, for later machines: program_cache.rs).
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
        None => match compile_waiting(caller, bytes, key)? {
            Some(Ok(module)) => module,
            Some(Err(error)) => {
                eprintln!("[collabo-core] the guest program does not compile: {error:?}");
                return Ok(ENOEXEC);
            }
            None => return Ok(EINTR),
        },
    };
    if !imports_supported(&module) {
        eprintln!("[collabo-core] the guest program imports something the sandbox does not provide");
        return Ok(ENOEXEC);
    }
    let maximum = maximum.min(rlimit_pages as u64);
    if maximum < minimum {
        return Ok(ENOMEM);
    }
    let memory = match SharedMemory::new(&engine, wasmtime::MemoryType::shared(minimum as u32, maximum as u32)) {
        Ok(memory) => memory,
        Err(error) => {
            eprintln!("[collabo-core] the guest program's memory ({minimum}..{maximum} pages) could not be allocated: {error}");
            return Ok(ENOMEM);
        }
    };
    caller.data_mut().user.pending_context =
        Some(UserContext { module, memory, maximum_pages: maximum, dylink: crate::dylink::Process::new() });
    Ok(0)
}

/// Compiles a program this machine has not compiled before, and keeps it. That can take tens of
/// seconds (program_cache.rs), all inside the exec, and the guest CPU this task holds would run
/// nothing else meanwhile: not its other tasks, not its timers (the kernel reported an RCU stall
/// after 21 s). So the program compiles on a thread of its own while the task sleeps in the
/// kernel, a tick at a time (`compile_wait`); with a kernel that does not export it, on this
/// thread as before. `None`: the task got a fatal signal meanwhile; the compile goes on, and its
/// result is kept for the next exec.
fn compile_waiting(caller: &mut Caller<'_, HostState>, bytes: Vec<u8>, key: u64) -> Result<Option<Result<Module>>> {
    let shared = caller.data().shared.clone();
    let compile = move || {
        let compiled = crate::program_cache::load_or_compile(&shared.engine, &bytes);
        if let Ok(module) = &compiled {
            shared.programs.lock().unwrap().insert(key, module.clone());
        }
        compiled
    };
    let kernel = caller.data().kernel_instance;
    let wait = kernel.and_then(|kernel| kernel.get_typed_func::<(), i32>(&mut *caller, "compile_wait").ok());
    let Some(wait) = wait else { return Ok(Some(compile())) };
    let thread = match std::thread::Builder::new().name("compile".into()).spawn(compile) {
        Ok(thread) => thread,
        Err(error) => return Ok(Some(Err(error.into()))),
    };
    while !thread.is_finished() {
        if wait.call(&mut *caller, ())? != 0 {
            return Ok(None);
        }
    }
    Ok(Some(thread.join().unwrap_or_else(|_| Err(anyhow!("the compiler panicked")))))
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
            // dlopen's calls, which the engine answers itself (dylink.rs)
            if let Some(result) = crate::dylink::syscall(&mut caller, nr, [a0, a1, a2, a3, a4]) {
                return result;
            }
            let before = caller.data().user.generation;
            // A trap of the program's own signal handler (UserTrapped) or an exec (HaltUser) passes
            // through; anything else failed in the kernel.
            let was = std::mem::replace(&mut caller.data_mut().user.in_user, false);
            let result = syscall.call(&mut caller, (nr, a0, a1, a2, a3, a4, a5));
            caller.data_mut().user.in_user = was;
            let result = result.map_err(kernel_error)?;
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
            let was = std::mem::replace(&mut caller.data_mut().user.in_user, false);
            let result = get_thread_area.call(&mut caller, ());
            caller.data_mut().user.in_user = was;
            result
        })?;
    }
    linker.func_wrap("linux", "copy_siginfo", move |mut caller: Caller<'_, HostState>, to: i32| -> Result<i32> {
        let was = std::mem::replace(&mut caller.data_mut().user.in_user, false);
        let result = copy_siginfo.call(&mut caller, to);
        caller.data_mut().user.in_user = was;
        let result = result?;
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
    // a new instance has none of the process's libraries yet: it catches up when it first syncs
    state.user.dylink = crate::dylink::Thread::default();
    Ok(())
}

/// What a call into the kernel failed with, as the program's caller should see it: a trap of the
/// program's own signal handler (UserTrapped) or an exec (HaltUser) passes through; anything
/// else failed in the kernel.
fn kernel_error(error: anyhow::Error) -> anyhow::Error {
    if error.chain().any(|cause| cause.is::<HaltUser>() || cause.is::<UserTrapped>()) {
        error
    } else {
        error.context(KernelFailed)
    }
}

/// While the kernel takes a tick: no deadline (about 10^11 ticks). The callback is out of the
/// store while it runs, so a nested one would trap, and the kernel may park this thread a while.
const NO_TICK: u64 = 1 << 40;

/// The epoch tick (machine.rs), on every store. In the program's own code the kernel takes it as
/// an interrupt from user mode (`user_interrupt`): expired timers, pending signals (SIGKILL ends
/// the task right there) and the scheduler, which is how a loop without syscalls still gets
/// signals and gives up its cpu. In the kernel's code, or with a kernel without the export,
/// nothing happens.
pub fn on_epoch(mut store: StoreContextMut<'_, HostState>) -> Result<UpdateDeadline> {
    if store.data().user.in_user {
        if store.data().user.interrupt.is_none() {
            let kernel = store.data().kernel_instance;
            let found = kernel.and_then(|kernel| kernel.get_typed_func::<(), ()>(&mut store, "user_interrupt").ok());
            store.data_mut().user.interrupt = Some(found);
        }
        if let Some(Some(interrupt)) = store.data().user.interrupt.clone() {
            store.set_epoch_deadline(NO_TICK);
            store.data_mut().user.in_user = false;
            let result = interrupt.call(&mut store, ());
            store.data_mut().user.in_user = true;
            result.map_err(kernel_error)?;
        }
    }
    Ok(UpdateDeadline::Continue(1))
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
        let was = std::mem::replace(&mut store.as_context_mut().data_mut().user.in_user, true);
        let outcome = match entry {
            Entry::Start => instance
                .get_typed_func::<(), ()>(store.as_context_mut(), "_start")
                .context("the program has no _start")
                .and_then(|start| start.call(store.as_context_mut(), ()).map_err(Into::into)),
            Entry::Function { function, arg } => table_function(store.as_context_mut(), &instance, function)
                .and_then(|function| call_dynamic(store.as_context_mut(), function, &[Val::I32(arg as i32)])),
        };
        store.as_context_mut().data_mut().user.in_user = was;
        match outcome.map_err(|error| uncaught(store.as_context_mut(), error)) {
            // A program that returns instead of exiting is a bug in the program, not the host.
            Ok(()) => return Ok(()),
            Err(error) if error.chain().any(|cause| cause.is::<HaltUser>()) => continue,
            // The program's own fault: back to the kernel, which ends it with SIGSEGV (as the
            // browser host does), instead of taking the whole machine down.
            Err(error) if is_user_trap(&error) => {
                report_user_trap(&error);
                return Ok(());
            }
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
    linker.func_wrap("user", "compile_end", |mut caller: Caller<'_, HostState>, pages: i32| -> Result<i32> {
        compile_end(&mut caller, pages as u32)
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
            // `to as u32` first: a guest address at or above 2 GiB must not sign-extend
            unsafe { *data[to as u32 as usize + index].get() = 0 };
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
    let was = std::mem::replace(&mut store.as_context_mut().data_mut().user.in_user, true);
    let called = call_dynamic(store.as_context_mut(), handler, &[Val::I32(signal)]);
    store.as_context_mut().data_mut().user.in_user = was;
    called.map_err(|error| mark_user_trap(uncaught(store.as_context_mut(), error)))
}

/// A trap of the program's code called from inside the kernel (a signal handler) is still the
/// program's: marked so that it unwinds to [`call`] instead of failing the kernel. So is an
/// exception the handler did not catch: as a plain error (not wasmtime's `ThrownException`) it
/// unwinds the kernel's frames like a trap, rather than land in code the signal interrupted with
/// the kernel half way through delivering it.
fn mark_user_trap(error: anyhow::Error) -> anyhow::Error {
    let own = error.downcast_ref::<wasmtime::Trap>().is_some() || error.is::<UncaughtException>();
    if own && !error.chain().any(|cause| cause.is::<KernelFailed>()) {
        error.context(UserTrapped)
    } else {
        error
    }
}

pub fn call_siginfo_handler(mut store: impl AsContextMut<Data = HostState>, trampoline: u32, function: i32, signal: i32) -> Result<i32> {
    let instance = store.as_context().data().user.instance.context("no user program is running")?;
    let entry = table_function(store.as_context_mut(), &instance, trampoline)?;
    store.as_context_mut().data_mut().user.siginfo_results.push(None);
    let was = std::mem::replace(&mut store.as_context_mut().data_mut().user.in_user, true);
    let called = call_dynamic(store.as_context_mut(), entry, &[Val::I32(function), Val::I32(signal)]);
    store.as_context_mut().data_mut().user.in_user = was;
    let called = called.map_err(|error| mark_user_trap(uncaught(store.as_context_mut(), error)));
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
    Ok(UserContext {
        module: context.module.clone(),
        memory: copy,
        maximum_pages: context.maximum_pages,
        dylink: context.dylink.fork(),
    })
}
