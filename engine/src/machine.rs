//! Boots the WebAssembly Linux kernel with wasmtime.
//!
//! Shape (the same as the JavaScript host it replaces):
//!   * one `Engine` and one compiled `Module`, one shared linear memory (the guest's RAM),
//!   * the main thread instantiates the module, hands over the device tree and initramfs and
//!     calls `boot()`, then serves requests from the CPUs,
//!   * each virtual CPU is an OS thread with its own `Store` and instance over the same memory;
//!     the guest's atomics and futexes work because the memory is a wasm shared memory.
//!
//! Stage 1 (this file): boot, the boot console, CPU threads, and the timer. Virtio devices and
//! user programs (binfmt_wasm) are separate modules and not wired up yet.
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use wasmtime::{Caller, Config, Engine, Instance, Linker, Module, Ref, SharedMemory, Store};

use crate::devicetree::{self, Node, Value};

pub const PAGE_SIZE: u64 = 65536;
const KERNEL_MEMORY_MAXIMUM_PAGES: u64 = 0xffff; // 4 GiB - 64 KiB, as the kernel expects

/// Why the guest stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    Clean,
    Panic,
}

/// What a CPU thread asks the main thread to do.
enum Request {
    /// Bytes typed at the console, from whatever the host is reading.
    ConsoleInput(Vec<u8>),
    SpawnCpu { function: u32, arg: u32, name: String, user: Option<crate::user::UserContext> },
    BootConsole(Vec<u8>),
    BootConsoleClose,
    RunOnMain { function: u32, arg: u32 },
    /// A device has something for the guest (the host wrote to it from another thread).
    Wake,
    Terminate(Termination),
    CpuExited,
}

/// A wasm trap used to unwind a CPU thread that has halted. Not an error.
#[derive(Debug)]
struct Halt;
impl std::fmt::Display for Halt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "kernel thread halted")
    }
}
impl std::error::Error for Halt {}

fn is_halt(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<Halt>())
}

/// Shared by every CPU thread.
pub struct Shared {
    pub engine: Engine,
    /// Every virtio device, in the order the device tree lists them.
    pub devices: crate::virtio::Devices,
    module: Module,
    /// Guest programs already compiled, by the hash of their bytes. Every `posix_spawn` hands
    /// the host the whole program again, and compiling busybox takes about a second, so the
    /// same shell is compiled once and reused for the life of the machine.
    pub programs: Mutex<HashMap<u64, Module>>,
    pub memory: SharedMemory,
    requests: Mutex<Sender<Request>>,
    /// Where the guest's early kernel messages go (before its console device exists).
    boot_console: Mutex<Box<dyn FnMut(&[u8]) + Send>>,
}

/// Lets a device tell the machine, from any thread, that it has work for the guest. Devices
/// receive one when the machine boots; before that, waking is a no-op.
#[derive(Clone, Default)]
pub struct Waker(Arc<Mutex<Option<Sender<Request>>>>);

impl Waker {
    pub fn wake(&self) {
        if let Some(sender) = self.0.lock().unwrap().as_ref() {
            let _ = sender.send(Request::Wake);
        }
    }

    fn attach(&self, sender: Sender<Request>) {
        *self.0.lock().unwrap() = Some(sender);
    }
}

impl Shared {
    fn send(&self, request: Request) {
        let _ = self.requests.lock().unwrap().send(request);
    }
}

/// Per-store (per-thread) state.
pub struct HostState {
    pub shared: Arc<Shared>,
    is_worker: bool,
    /// Only the main thread answers these.
    devicetree: Vec<u8>,
    initramfs: Vec<u8>,
    /// This thread's kernel instance, once it exists (user programs call back into it).
    pub kernel_instance: Option<Instance>,
    /// The userspace program this thread is running, if any.
    pub user: crate::user::User,
}

/// Copies out a range of a guest memory, or `None` if it is out of bounds. The guest may be
/// writing the same bytes concurrently: they are plain data, and every access is checked here.
pub fn guest_bytes(memory: &SharedMemory, address: u32, length: u32) -> Option<Vec<u8>> {
    let data = memory.data();
    let (start, end) = (address as usize, address as usize + length as usize);
    if end > data.len() {
        return None;
    }
    Some(data[start..end].iter().map(|cell| unsafe { *cell.get() }).collect())
}

fn read_memory(memory: &SharedMemory, address: u32, length: u32) -> Result<Vec<u8>> {
    guest_bytes(memory, address, length)
        .ok_or_else(|| anyhow!("guest memory access out of bounds: {address}+{length}"))
}

fn write_memory(memory: &SharedMemory, address: u32, bytes: &[u8]) -> Result<()> {
    let data = memory.data();
    let (start, end) = (address as usize, address as usize + bytes.len());
    if end > data.len() {
        bail!("guest memory write out of bounds: {address}+{}", bytes.len());
    }
    for (cell, byte) in data[start..end].iter().zip(bytes) {
        unsafe { *cell.get() = *byte };
    }
    Ok(())
}

fn now_nsec() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

/// Registers the host functions the kernel imports. `is_worker` selects the few that differ
/// between the main thread and a CPU thread, exactly as the JavaScript host does.
fn link_kernel(linker: &mut Linker<HostState>) -> Result<()> {
    linker.func_wrap("kernel", "breakpoint", || {})?;
    linker.func_wrap("kernel", "return_address", |_level: i32| -> i32 { 0 })?;
    linker.func_wrap("kernel", "get_now_nsec", || -> i64 { now_nsec() })?;
    linker.func_wrap("kernel", "get_stacktrace", |_buf: i32, _size: i32| {})?;

    linker.func_wrap(
        "kernel",
        "boot_console_write",
        |caller: Caller<'_, HostState>, message: i32, length: i32| -> Result<()> {
            let state = caller.data();
            let bytes = read_memory(&state.shared.memory, message as u32, length as u32)?;
            if state.is_worker {
                state.shared.send(Request::BootConsole(bytes));
            } else {
                (state.shared.boot_console.lock().unwrap())(&bytes);
            }
            Ok(())
        },
    )?;
    linker.func_wrap("kernel", "boot_console_close", |caller: Caller<'_, HostState>| {
        caller.data().shared.send(Request::BootConsoleClose);
    })?;

    linker.func_wrap(
        "kernel",
        "terminate_machine",
        |caller: Caller<'_, HostState>, reason: i32| -> Result<()> {
            let termination = if reason == 0 { Termination::Clean } else { Termination::Panic };
            caller.data().shared.send(Request::Terminate(termination));
            Err(anyhow!(Halt))
        },
    )?;
    linker.func_wrap("kernel", "halt_worker", |caller: Caller<'_, HostState>| -> Result<()> {
        if !caller.data().is_worker {
            bail!("the kernel halted the main thread");
        }
        caller.data().shared.send(Request::CpuExited);
        Err(anyhow!(Halt))
    })?;
    linker.func_wrap(
        "kernel",
        "run_on_main",
        |caller: Caller<'_, HostState>, function: i32, arg: i32| {
            caller.data().shared.send(Request::RunOnMain { function: function as u32, arg: arg as u32 });
        },
    )?;
    // A new kernel thread. `user_memory` says what the child does with this thread's program
    // memory: nothing (a kernel thread), share it (clone), or get a copy of it (fork).
    const USER_MEMORY_NONE: i32 = 0;
    const USER_MEMORY_SHARE: i32 = 1;
    const USER_MEMORY_COPY: i32 = 2;
    linker.func_wrap(
        "kernel",
        "spawn_worker",
        |caller: Caller<'_, HostState>, function: i32, arg: i32, comm: i32, comm_len: i32, user_memory: i32| -> Result<i32> {
            let state = caller.data();
            let name = read_memory(&state.shared.memory, comm as u32, comm_len as u32)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            let user = match user_memory {
                USER_MEMORY_NONE => None,
                USER_MEMORY_SHARE | USER_MEMORY_COPY => {
                    let Some(context) = state.user.context.clone() else { return Ok(-22) };
                    if user_memory == USER_MEMORY_COPY {
                        // Synchronous, as the kernel's ABI requires: the child's memory exists
                        // before this call returns.
                        match crate::user::copy_context(&state.shared.engine, &context) {
                            Ok(copy) => Some(copy),
                            Err(_) => return Ok(-12), // out of memory
                        }
                    } else {
                        Some(context)
                    }
                }
                _ => return Ok(-22),
            };
            state.shared.send(Request::SpawnCpu { function: function as u32, arg: arg as u32, name, user });
            Ok(0)
        },
    )?;
    Ok(())
}

/// `boot.*` is answered by the main thread only (a CPU thread never asks).
fn link_boot(linker: &mut Linker<HostState>) -> Result<()> {
    fn hand_over(caller: &Caller<'_, HostState>, blob: &[u8], buffer: i32, size: i32) -> Result<i32> {
        let (address, capacity) = (buffer as u32, size as u32);
        if address == 0 && capacity == 0 {
            return Ok(blob.len() as i32);
        }
        if (capacity as usize) < blob.len() {
            bail!("the kernel's buffer is too small: {capacity} < {}", blob.len());
        }
        write_memory(&caller.data().shared.memory, address, blob)?;
        Ok(blob.len() as i32)
    }
    linker.func_wrap(
        "boot",
        "get_devicetree",
        |caller: Caller<'_, HostState>, buffer: i32, size: i32| -> Result<i32> {
            let blob = caller.data().devicetree.clone();
            hand_over(&caller, &blob, buffer, size)
        },
    )?;
    linker.func_wrap(
        "boot",
        "get_initramfs",
        |caller: Caller<'_, HostState>, buffer: i32, size: i32| -> Result<i32> {
            let blob = caller.data().initramfs.clone();
            hand_over(&caller, &blob, buffer, size)
        },
    )?;
    Ok(())
}

/// The imports the kernel needs. The memory is defined against `store` because wasmtime ties
/// externals to a store context; the memory itself is shared by every CPU thread.
fn make_linker(shared: &Arc<Shared>, store: &Store<HostState>) -> Result<Linker<HostState>> {
    let mut linker = Linker::new(&shared.engine);
    linker.define(store, "env", "memory", shared.memory.clone())?;
    link_kernel(&mut linker)?;
    link_boot(&mut linker)?;
    crate::user::link(&mut linker)?;
    link_user_entries(&mut linker)?;
    link_virtio(&mut linker)?;
    Ok(linker)
}

/// `virtio.*`. The kernel only calls these from the main thread (it moves device work there
/// with `run_on_main`), so they run with the main instance and can raise interrupts directly.
fn link_virtio(linker: &mut Linker<HostState>) -> Result<()> {
    linker.func_wrap(
        "virtio",
        "setup",
        |caller: Caller<'_, HostState>, device: i32, config_irq: i32, config_at: i32, config_len: i32| {
            let state = caller.data();
            let mut devices = state.shared.devices.0.lock().unwrap();
            if let Some(connected) = devices.get_mut(device as usize) {
                connected.setup(&state.shared.memory, config_irq as u32, config_at as u32 as u64, config_len as u32);
            }
        },
    )?;
    linker.func_wrap("virtio", "reset", |caller: Caller<'_, HostState>, device: i32| {
        let mut devices = caller.data().shared.devices.0.lock().unwrap();
        if let Some(connected) = devices.get_mut(device as usize) {
            connected.reset();
        }
    })?;
    linker.func_wrap(
        "virtio",
        "enable_vring",
        |caller: Caller<'_, HostState>, device: i32, queue: i32, size: i32, descriptors_at: i32, irq: i32| {
            let state = caller.data();
            let mut devices = state.shared.devices.0.lock().unwrap();
            if let Some(connected) = devices.get_mut(device as usize) {
                connected.enable_queue(
                    &state.shared.memory,
                    queue as u16,
                    size as u16,
                    descriptors_at as u32 as u64,
                    irq as u32,
                );
            }
        },
    )?;
    linker.func_wrap("virtio", "disable_vring", |caller: Caller<'_, HostState>, device: i32, queue: i32| {
        let mut devices = caller.data().shared.devices.0.lock().unwrap();
        if let Some(connected) = devices.get_mut(device as usize) {
            connected.disable_queue(queue as u16);
        }
    })?;
    linker.func_wrap("virtio", "set_features", |_caller: Caller<'_, HostState>, _device: i32, _features: i64| {})?;
    linker.func_wrap(
        "virtio",
        "notify",
        |mut caller: Caller<'_, HostState>, device: i32, queue: i32| -> Result<()> {
            let memory = caller.data().shared.memory.clone();
            let irqs = {
                let devices = caller.data().shared.devices.clone();
                let mut devices = devices.0.lock().unwrap();
                match devices.get_mut(device as usize) {
                    Some(connected) => connected.notify(&memory, queue as u16)?,
                    None => Vec::new(),
                }
            };
            raise_interrupts(&mut caller, &irqs)
        },
    )?;
    Ok(())
}

/// Tells the kernel a device finished something. Re-entering the kernel from a host call it made
/// is what the JavaScript host does too.
fn raise_interrupts(caller: &mut Caller<'_, HostState>, irqs: &[u32]) -> Result<()> {
    if irqs.is_empty() {
        return Ok(());
    }
    let trigger = caller
        .get_export("trigger_irq")
        .and_then(|export| export.into_func())
        .context("the kernel does not export trigger_irq")?
        .typed::<i32, ()>(&*caller)?;
    let mut seen = Vec::new();
    for irq in irqs {
        if seen.contains(irq) {
            continue;
        }
        seen.push(*irq);
        trigger.call(&mut *caller, *irq as i32)?;
    }
    Ok(())
}

/// The `user.*` imports that run guest code, so they need this thread's kernel instance.
fn link_user_entries(linker: &mut Linker<HostState>) -> Result<()> {
    linker.func_wrap(
        "user",
        "instantiate",
        |mut caller: Caller<'_, HostState>, fresh_memory: i32| -> Result<()> {
            let kernel = caller.data().kernel_instance.context("the kernel instance is not ready")?;
            crate::user::instantiate(&mut caller, &kernel, fresh_memory != 0)
        },
    )?;
    linker.func_wrap("user", "call", |mut caller: Caller<'_, HostState>| -> Result<()> {
        crate::user::call(&mut caller)
    })?;
    linker.func_wrap(
        "user",
        "call_signal_handler",
        |mut caller: Caller<'_, HostState>, function: i32, signal: i32| -> Result<()> {
            crate::user::call_signal_handler(&mut caller, function as u32, signal)
        },
    )?;
    linker.func_wrap(
        "user",
        "call_siginfo_handler",
        |mut caller: Caller<'_, HostState>, trampoline: i32, function: i32, signal: i32| -> Result<i32> {
            crate::user::call_siginfo_handler(&mut caller, trampoline as u32, function, signal)
        },
    )?;
    Ok(())
}

/// Calls `__indirect_function_table[function](arg)`, the kernel's entry point for a CPU thread
/// and for work the main thread must run.
fn call_indirect(store: &mut Store<HostState>, instance: &Instance, function: u32, arg: u32) -> Result<()> {
    let table = instance
        .get_table(&mut *store, "__indirect_function_table")
        .context("the kernel does not export __indirect_function_table")?;
    let entry = table.get(&mut *store, function as u64).context("function index out of range")?;
    let func = match entry {
        Ref::Func(Some(func)) => func,
        _ => bail!("no function at table index {function}"),
    };
    let results = func.ty(&*store).results().len();
    let mut out = vec![wasmtime::Val::I32(0); results];
    func.call(&mut *store, &[wasmtime::Val::I32(arg as i32)], &mut out)?;
    Ok(())
}

/// One virtual CPU: its own store and instance over the shared memory.
fn run_cpu(shared: Arc<Shared>, function: u32, arg: u32, user: Option<crate::user::UserContext>) -> Result<()> {
    let mut store = Store::new(
        &shared.engine,
        HostState {
            shared: shared.clone(),
            is_worker: true,
            devicetree: Vec::new(),
            initramfs: Vec::new(),
            kernel_instance: None,
            user: Default::default(),
        },
    );
    store.data_mut().user.context = user.clone();
    let linker = make_linker(&shared, &store)?;
    let instance = linker.instantiate(&mut store, &shared.module)?;
    store.data_mut().kernel_instance = Some(instance);
    // A thread that inherits a program runs in it from its first instruction.
    if user.is_some() {
        crate::user::instantiate(&mut store, &instance, false)?;
    }
    match call_indirect(&mut store, &instance, function, arg) {
        Ok(()) => Ok(()),
        Err(error) if is_halt(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Stops a running machine from another thread.
#[derive(Clone)]
pub struct Stopper(Sender<Request>);

impl Stopper {
    pub fn stop(&self) {
        let _ = self.0.send(Request::Terminate(Termination::Clean));
    }
}

/// A booted machine. Dropping it stops the guest.
pub struct Machine {
    shared: Arc<Shared>,
    requests: Receiver<Request>,
    store: Store<HostState>,
    instance: Instance,
    pub termination: Option<Termination>,
}

pub struct BootOptions {
    pub kernel: Vec<u8>,
    /// Virtio devices, in device tree order.
    pub devices: Vec<Box<dyn crate::virtio::Device>>,
    /// Extra kernel command line arguments (after `console=hvc0`).
    pub args: Vec<String>,
    pub cpus: u32,
    /// The cpio archive the kernel unpacks as its root filesystem.
    pub initcpio: Vec<u8>,
    /// Receives the guest's early kernel messages.
    pub boot_console: Box<dyn FnMut(&[u8]) + Send>,
}

impl Machine {
    pub fn boot(options: BootOptions) -> Result<Machine> {
        let mut config = Config::new();
        // Guest programs are compiled with LLVM's setjmp/longjmp lowering, which uses the
        // exception-handling instructions.
        config.wasm_threads(true).wasm_simd(true).wasm_bulk_memory(true).wasm_exceptions(true);
        let engine = Engine::new(&config)?;
        let module = Module::new(&engine, &options.kernel).context("compiling the kernel")?;

        let (memory_type, kernel_pages) = crate::module_info::memory_import(&options.kernel)?;
        let initcpio_address = kernel_pages * PAGE_SIZE;
        let initcpio_pages = (options.initcpio.len() as u64).div_ceil(PAGE_SIZE);
        let initial_pages = kernel_pages + initcpio_pages;
        if !memory_type.is_shared {
            bail!("the kernel must import a shared memory");
        }
        let memory = SharedMemory::new(
            &engine,
            wasmtime::MemoryType::shared(initial_pages as u32, KERNEL_MEMORY_MAXIMUM_PAGES as u32),
        )
        .context("allocating the guest's memory")?;

        let sections = crate::module_info::custom_section(&options.kernel, ".linux.sections")?;
        let initramfs = crate::module_info::custom_section(&options.kernel, ".linux.initramfs")?;

        // Built before the console closure and the devices are moved into the machine.
        // Transport features the host always offers: virtio 1.0, packed rings (what the host
        // transport implements), and indirect descriptors.
        const TRANSPORT_FEATURES: u64 = (1 << 32) | (1 << 34) | (1 << 28);
        let device_descriptions: Vec<(u32, u64, Vec<u8>)> = options
            .devices
            .iter()
            .map(|device| (device.device_id(), device.features() | TRANSPORT_FEATURES, device.config()))
            .collect();
        let devicetree = build_devicetree(
            &options,
            &sections,
            initcpio_address,
            KERNEL_MEMORY_MAXIMUM_PAGES * PAGE_SIZE,
            &device_descriptions,
        );

        let (sender, requests) = channel();
        let mut devices = options.devices;
        let waker = Waker::default();
        waker.attach(sender.clone());
        for device in devices.iter_mut() {
            device.attach_waker(waker.clone());
        }
        let devices = crate::virtio::Devices::new(devices);
        let shared = Arc::new(Shared {
            engine,
            devices,
            module,
            programs: Mutex::new(HashMap::new()),
            memory,
            requests: Mutex::new(sender),
            boot_console: Mutex::new(options.boot_console),
        });

        // The initramfs the caller supplies is copied straight after the kernel image, and the
        // device tree tells the kernel where to find it.
        if !options.initcpio.is_empty() {
            write_memory(&shared.memory, initcpio_address as u32, &options.initcpio)?;
        }
        let mut store = Store::new(
            &shared.engine,
            HostState {
                shared: shared.clone(),
                is_worker: false,
                devicetree,
                initramfs,
                kernel_instance: None,
                user: Default::default(),
            },
        );
        let linker = make_linker(&shared, &store)?;
        let instance = linker.instantiate(&mut store, &shared.module)?;
        store.data_mut().kernel_instance = Some(instance);
        instance.get_typed_func::<(), ()>(&mut store, "boot")?.call(&mut store, ())?;

        Ok(Machine { shared, requests, store, instance, termination: None })
    }

    /// Serves the guest until it stops. CPU threads do the work; this thread answers them.
    pub fn run(&mut self) -> Result<Termination> {
        let mut cpus = 0usize;
        loop {
            let request = match self.requests.recv() {
                Ok(request) => request,
                Err(_) => return Ok(self.termination.unwrap_or(Termination::Clean)),
            };
            match request {
                Request::SpawnCpu { function, arg, name, user } => {
                    let shared = self.shared.clone();
                    cpus += 1;
                    std::thread::Builder::new().name(name).spawn(move || {
                        if let Err(error) = run_cpu(shared.clone(), function, arg, user) {
                            eprintln!("collabo-core: cpu thread failed: {error:?}");
                            shared.send(Request::Terminate(Termination::Panic));
                        }
                    })?;
                }
                Request::BootConsole(bytes) => (self.shared.boot_console.lock().unwrap())(&bytes),
                Request::ConsoleInput(bytes) => self.deliver_console_input(&bytes)?,
                Request::Wake => self.poll_devices()?,
                Request::BootConsoleClose => {}
                Request::RunOnMain { function, arg } => {
                    call_indirect(&mut self.store, &self.instance, function, arg)?;
                }
                Request::CpuExited => {
                    cpus = cpus.saturating_sub(1);
                }
                Request::Terminate(reason) => {
                    self.termination = Some(reason);
                    return Ok(reason);
                }
            }
            let _ = cpus;
        }
    }

    /// A handle that stops the machine from another thread.
    pub fn stopper(&self) -> Stopper {
        Stopper(self.shared.requests.lock().unwrap().clone())
    }

    /// A handle the host can use to type into the guest console from another thread.
    pub fn console_input(&self) -> Sender<Vec<u8>> {
        let requests = self.shared.requests.lock().unwrap().clone();
        let (sender, receiver) = channel::<Vec<u8>>();
        std::thread::spawn(move || {
            while let Ok(bytes) = receiver.recv() {
                if requests.send(Request::ConsoleInput(bytes)).is_err() {
                    break;
                }
            }
        });
        sender
    }

    /// Gives every device a chance to hand the guest what the host has queued for it.
    fn poll_devices(&mut self) -> Result<()> {
        let memory = self.shared.memory.clone();
        let devices = self.shared.devices.clone();
        let mut irqs = Vec::new();
        {
            let mut devices = devices.0.lock().unwrap();
            for connected in devices.iter_mut() {
                irqs.extend(connected.poll(&memory)?);
            }
        }
        self.raise(irqs)
    }

    /// Hands typed bytes to the console device and tells the kernel about them.
    fn deliver_console_input(&mut self, bytes: &[u8]) -> Result<()> {
        let memory = self.shared.memory.clone();
        let devices = self.shared.devices.clone();
        let mut irqs = Vec::new();
        {
            let mut devices = devices.0.lock().unwrap();
            for connected in devices.iter_mut() {
                irqs.extend(connected.console_input(bytes, &memory)?);
            }
        }
        self.raise(irqs)
    }

    /// Tells the kernel that these interrupt lines have fired, each one once.
    fn raise(&mut self, irqs: Vec<u32>) -> Result<()> {
        if irqs.is_empty() {
            return Ok(());
        }
        let trigger = self.instance.get_typed_func::<i32, ()>(&mut self.store, "trigger_irq")?;
        let mut seen = Vec::new();
        for irq in irqs {
            if seen.contains(&irq) {
                continue;
            }
            seen.push(irq);
            trigger.call(&mut self.store, irq as i32)?;
        }
        Ok(())
    }
}

fn build_devicetree(
    options: &BootOptions,
    sections: &[u8],
    initcpio_address: u64,
    memory_size: u64,
    device_descriptions: &[(u32, u64, Vec<u8>)],
) -> Vec<u8> {
    let mut root = Node::default();
    root.prop("#address-cells", Value::U32(1));
    root.prop("#size-cells", Value::U32(1));
    {
        let chosen = root.child("chosen");
        let mut seed = [0u8; 64];
        getrandom(&mut seed);
        chosen.prop("rng-seed", Value::Bytes(seed.to_vec()));
        let args = if options.args.is_empty() { String::new() } else { format!(" {}", options.args.join(" ")) };
        chosen.prop("bootargs", Value::Str(format!("console=hvc0{args}")));
        chosen.prop("ncpus", Value::U32(options.cpus));
        if !options.initcpio.is_empty() {
            chosen.prop("linux,initrd-start", Value::U32(initcpio_address as u32));
            chosen.prop("linux,initrd-end", Value::U32(initcpio_address as u32 + options.initcpio.len() as u32));
        }
        // The kernel reads its section table as a node of name -> [start, size] cells.
        let table = chosen.child("sections");
        for (name, cells) in crate::module_info::parse_sections(sections) {
            table.prop(&name, Value::Cells(cells));
        }
    }
    root.child("aliases");
    {
        let memory = root.child("memory");
        memory.prop("device_type", Value::Str("memory".into()));
        memory.prop("reg", Value::Cells(vec![0, memory_size as u32]));
    }
    {
        let reserved = root.child("reserved-memory");
        reserved.prop("#address-cells", Value::U32(1));
        reserved.prop("#size-cells", Value::U32(1));
        reserved.prop("ranges", Value::Empty);
    }
    for (index, device) in device_descriptions.iter().enumerate() {
        let node = root.child(&format!("virtio{index}"));
        node.prop("compatible", Value::Str("virtio,wasm".into()));
        node.prop("host-id", Value::U32(index as u32));
        node.prop("virtio-device-id", Value::U32(device.0));
        node.prop("features", Value::U64(device.1));
        node.prop("config", Value::Bytes(device.2.clone()));
    }
    let reservations: Vec<(u64, u64)> = if options.initcpio.is_empty() {
        Vec::new()
    } else {
        vec![(initcpio_address, options.initcpio.len() as u64)]
    };
    devicetree::generate(&root, &reservations, 0)
}

fn getrandom(buffer: &mut [u8]) {
    use rand::RngCore;
    rand::rng().fill_bytes(buffer);
}
