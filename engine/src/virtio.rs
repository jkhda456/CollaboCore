//! The virtio transport the kernel uses (a port of the host JS `virtio/core.ts`).
//!
//! The guest does not use MMIO: the kernel calls the host directly (`virtio.setup`,
//! `enable_vring`, `notify`, ...) and the host reads and writes packed virtqueues in guest
//! memory, then raises an interrupt through the kernel's `trigger_irq` export.
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use wasmtime::SharedMemory;

const DESCRIPTOR_SIZE: u64 = 16;
const FLAG_NEXT: u16 = 1;
const FLAG_WRITE: u16 = 1 << 1;
const FLAG_INDIRECT: u16 = 1 << 2;
const FLAG_AVAIL: u16 = 1 << 7;
const FLAG_USED: u16 = 1 << 15;

#[derive(Clone, Copy, Debug)]
struct Descriptor {
    address: u64,
    length: u32,
    id: u16,
    flags: u16,
}

fn read_descriptor(memory: &SharedMemory, at: u64) -> Result<Descriptor> {
    let bytes = crate::machine::guest_bytes(memory, at as u32, DESCRIPTOR_SIZE as u32)
        .ok_or_else(|| anyhow::anyhow!("virtqueue descriptor outside guest memory"))?;
    Ok(Descriptor {
        address: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
        length: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        id: u16::from_le_bytes(bytes[12..14].try_into().unwrap()),
        flags: u16::from_le_bytes(bytes[14..16].try_into().unwrap()),
    })
}

fn write_u16(memory: &SharedMemory, at: u64, value: u16) {
    let data = memory.data();
    for (index, byte) in value.to_le_bytes().iter().enumerate() {
        if let Some(cell) = data.get(at as usize + index) {
            unsafe { *cell.get() = *byte };
        }
    }
}

fn write_u32(memory: &SharedMemory, at: u64, value: u32) {
    let data = memory.data();
    for (index, byte) in value.to_le_bytes().iter().enumerate() {
        if let Some(cell) = data.get(at as usize + index) {
            unsafe { *cell.get() = *byte };
        }
    }
}

/// Copies bytes into guest memory, stopping at its end. Returns how many were written.
pub fn write_bytes(memory: &SharedMemory, at: u64, bytes: &[u8]) -> usize {
    let data = memory.data();
    let mut written = 0;
    for (index, byte) in bytes.iter().enumerate() {
        match data.get(at as usize + index) {
            Some(cell) => unsafe { *cell.get() = *byte },
            None => break,
        }
        written += 1;
    }
    written
}

/// One buffer of a request: a window into guest memory. `writable` means the driver expects the
/// device to fill it in.
pub struct Buffer {
    pub address: u64,
    pub length: u32,
    pub writable: bool,
}

/// One request from the guest: its buffers, and the bookkeeping to complete it.
pub struct Chain {
    pub buffers: Vec<Buffer>,
    id: u16,
    skip: u16,
}

/// A packed virtqueue, as the guest driver and the host share it.
pub struct Queue {
    memory: SharedMemory,
    size: u16,
    descriptors_at: u64,
    irq: u32,
    avail_wrap: bool,
    used_wrap: bool,
    avail_index: u16,
    used_index: u16,
}

impl Queue {
    fn new(memory: SharedMemory, size: u16, descriptors_at: u64, irq: u32) -> Queue {
        Queue { memory, size, descriptors_at, irq, avail_wrap: true, used_wrap: true, avail_index: 0, used_index: 0 }
    }

    fn descriptor_at(&self, index: u16) -> Result<Descriptor> {
        read_descriptor(&self.memory, self.descriptors_at + DESCRIPTOR_SIZE * index as u64)
    }

    /// The next descriptor the driver has made available, if any.
    fn advance(&mut self) -> Result<Option<u16>> {
        let descriptor = self.descriptor_at(self.avail_index)?;
        let avail = descriptor.flags & FLAG_AVAIL != 0;
        let used = descriptor.flags & FLAG_USED != 0;
        if avail == used || avail != self.avail_wrap {
            return Ok(None);
        }
        let index = self.avail_index;
        self.avail_index += 1;
        if self.avail_index >= self.size {
            self.avail_index = 0;
            self.avail_wrap = !self.avail_wrap;
        }
        Ok(Some(index))
    }

    /// Takes the next request from the queue.
    pub fn pop(&mut self) -> Result<Option<Chain>> {
        let Some(mut index) = self.advance()? else { return Ok(None) };
        let mut descriptor = self.descriptor_at(index)?;
        let id = descriptor.id;
        let mut skip = 1u16;
        let mut chain = vec![descriptor];

        if descriptor.flags & FLAG_NEXT != 0 {
            while descriptor.flags & FLAG_NEXT != 0 {
                let Some(next) = self.advance()? else { bail!("virtqueue chain ends without its next descriptor") };
                index = next;
                descriptor = self.descriptor_at(index)?;
                chain.push(descriptor);
                skip += 1;
            }
        } else if descriptor.flags & FLAG_INDIRECT != 0 {
            if descriptor.length as u64 % DESCRIPTOR_SIZE != 0 {
                bail!("malformed indirect descriptor table");
            }
            let count = descriptor.length as u64 / DESCRIPTOR_SIZE;
            chain = (0..count)
                .map(|i| read_descriptor(&self.memory, descriptor.address + DESCRIPTOR_SIZE * i))
                .collect::<Result<_>>()?;
        }

        let buffers = chain
            .iter()
            .map(|descriptor| Buffer {
                address: descriptor.address,
                length: descriptor.length,
                writable: descriptor.flags & FLAG_WRITE != 0,
            })
            .collect();
        Ok(Some(Chain { buffers, id, skip }))
    }

    /// Hands a finished request back to the driver, saying how many bytes the device wrote.
    pub fn release(&mut self, chain: Chain, written: u32) -> Result<u32> {
        let at = self.descriptors_at + DESCRIPTOR_SIZE * self.used_index as u64;
        let descriptor = read_descriptor(&self.memory, at)?;
        let avail = descriptor.flags & FLAG_AVAIL != 0;
        let used = descriptor.flags & FLAG_USED != 0;
        if avail == used || avail != self.used_wrap {
            bail!("virtqueue is full");
        }
        let mut flags = 0u16;
        if self.used_wrap {
            flags |= FLAG_AVAIL | FLAG_USED;
        }
        if written > 0 {
            flags |= FLAG_WRITE;
        }
        // id and length first; the flags make the completion visible, so publish them last.
        write_u32(&self.memory, at + 8, written);
        write_u16(&self.memory, at + 12, chain.id);
        write_u16(&self.memory, at + 14, flags);

        self.used_index += chain.skip;
        if self.used_index >= self.size {
            self.used_index -= self.size;
            self.used_wrap = !self.used_wrap;
        }
        Ok(self.irq)
    }
}

/// What a device does when the guest kicks one of its queues.
pub trait Device: Send {
    /// The virtio device id (3 = console, 4 = entropy, 19 = vsock).
    fn device_id(&self) -> u32;
    fn features(&self) -> u64 {
        0
    }
    /// The device's configuration space, which the guest reads at setup.
    fn config(&self) -> Vec<u8> {
        Vec::new()
    }
    /// Handles one kick on `queue_index`. Return the IRQs to raise.
    fn notify(&mut self, queue_index: u16, queue: &mut Queue, memory: &SharedMemory) -> Result<Vec<u32>>;
    /// Hands the guest whatever the host has queued since the last call, using any queue the
    /// guest has set up. Called after every kick and whenever the device wakes the machine.
    fn poll(&mut self, queues: &mut [Option<Queue>], memory: &SharedMemory) -> Result<Vec<u32>> {
        let _ = (queues, memory);
        Ok(Vec::new())
    }
    /// Receives the handle the device uses to tell the machine it has work for the guest.
    fn attach_waker(&mut self, waker: crate::machine::Waker) {
        let _ = waker;
    }
    /// The guest reset the device: drop whatever state belongs to the old driver.
    fn reset(&mut self) {}
    /// The console device, when this is one: the host pushes typed input into it.
    fn as_any_console(&mut self) -> Option<&mut crate::console::Console> {
        None
    }
}

/// A device plus the queues the guest has set up for it.
pub struct Connected {
    pub device: Box<dyn Device>,
    queues: Vec<Option<Queue>>,
    config_irq: u32,
    config_at: u64,
    config_len: u32,
}

impl Connected {
    pub fn new(device: Box<dyn Device>) -> Connected {
        Connected { device, queues: Vec::new(), config_irq: 0, config_at: 0, config_len: 0 }
    }

    pub fn setup(&mut self, memory: &SharedMemory, config_irq: u32, config_at: u64, config_len: u32) {
        self.config_irq = config_irq;
        self.config_at = config_at;
        self.config_len = config_len;
        let config = self.device.config();
        let length = config.len().min(config_len as usize);
        let data = memory.data();
        for index in 0..length {
            if let Some(cell) = data.get(config_at as usize + index) {
                unsafe { *cell.get() = config[index] };
            }
        }
    }

    pub fn enable_queue(&mut self, memory: &SharedMemory, index: u16, size: u16, descriptors_at: u64, irq: u32) {
        if self.queues.len() <= index as usize {
            self.queues.resize_with(index as usize + 1, || None);
        }
        self.queues[index as usize] = Some(Queue::new(memory.clone(), size, descriptors_at, irq));
    }

    pub fn disable_queue(&mut self, index: u16) {
        if let Some(slot) = self.queues.get_mut(index as usize) {
            *slot = None;
        }
    }

    pub fn reset(&mut self) {
        self.queues.clear();
        self.device.reset();
    }

    pub fn notify(&mut self, memory: &SharedMemory, index: u16) -> Result<Vec<u32>> {
        let Connected { device, queues, .. } = self;
        let mut irqs = match queues.get_mut(index as usize) {
            Some(Some(queue)) => device.notify(index, queue, memory)?,
            _ => Vec::new(),
        };
        // A kick often makes room for what the host has waiting, on another queue.
        irqs.extend(device.poll(queues, memory)?);
        Ok(irqs)
    }

    /// Lets the device deliver what the host queued from another thread.
    pub fn poll(&mut self, memory: &SharedMemory) -> Result<Vec<u32>> {
        let Connected { device, queues, .. } = self;
        device.poll(queues, memory)
    }

    /// Hands typed bytes to this device if it is the console, delivering what it can at once.
    pub fn console_input(&mut self, bytes: &[u8], memory: &SharedMemory) -> Result<Vec<u32>> {
        let Connected { device, queues, .. } = self;
        let Some(console) = device.as_any_console() else { return Ok(Vec::new()) };
        let queue = queues.first_mut().and_then(|slot| slot.as_mut());
        console.write_input(bytes, queue, memory)
    }
}

/// Every device of a machine, addressed by the index the device tree gave them.
#[derive(Clone)]
pub struct Devices(pub Arc<Mutex<Vec<Connected>>>);

impl Devices {
    pub fn new(devices: Vec<Box<dyn Device>>) -> Devices {
        Devices(Arc::new(Mutex::new(devices.into_iter().map(Connected::new).collect())))
    }
}
