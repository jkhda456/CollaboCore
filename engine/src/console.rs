//! The guest's terminal: a virtio-console device (device id 3).
//!
//! Queue 0 carries what the guest reads (the host fills the buffers it offers), queue 1 what the
//! guest writes. Input that arrives while the guest has no buffer waiting is held here until it
//! offers one, as the JavaScript `virtio/console.ts` does.
use anyhow::Result;
use wasmtime::SharedMemory;

use crate::virtio::{Device, Queue};

const FEATURE_SIZE: u64 = 1; // VIRTIO_CONSOLE_F_SIZE: the config carries the window size

pub struct Console {
    columns: u16,
    rows: u16,
    /// Bytes from the host waiting for a buffer the guest offers.
    pending_input: Vec<u8>,
    /// Where the guest's output goes.
    output: Box<dyn FnMut(&[u8]) + Send>,
}

impl Console {
    pub fn new(columns: u16, rows: u16, output: Box<dyn FnMut(&[u8]) + Send>) -> Console {
        Console { columns, rows, pending_input: Vec::new(), output }
    }

    /// Queues input for the guest. Returns the IRQs to raise if it could be delivered at once.
    pub fn write_input(&mut self, bytes: &[u8], queue: Option<&mut Queue>, memory: &SharedMemory) -> Result<Vec<u32>> {
        self.pending_input.extend_from_slice(bytes);
        match queue {
            Some(queue) => self.deliver(queue, memory),
            None => Ok(Vec::new()),
        }
    }

    /// Fills as many of the guest's receive buffers as there is input for.
    fn deliver(&mut self, queue: &mut Queue, memory: &SharedMemory) -> Result<Vec<u32>> {
        let mut irqs = Vec::new();
        while !self.pending_input.is_empty() {
            let Some(chain) = queue.pop()? else { break };
            let mut written = 0u32;
            for buffer in &chain.buffers {
                if !buffer.writable {
                    continue;
                }
                let take = (buffer.length as usize).min(self.pending_input.len());
                if take == 0 {
                    break;
                }
                let data = memory.data();
                for (index, byte) in self.pending_input[..take].iter().enumerate() {
                    if let Some(cell) = data.get(buffer.address as usize + index) {
                        unsafe { *cell.get() = *byte };
                    }
                }
                self.pending_input.drain(..take);
                written += take as u32;
            }
            irqs.push(queue.release(chain, written)?);
        }
        Ok(irqs)
    }
}

impl Device for Console {
    fn device_id(&self) -> u32 {
        3
    }
    fn features(&self) -> u64 {
        FEATURE_SIZE
    }
    fn config(&self) -> Vec<u8> {
        // struct virtio_console_config { u16 cols; u16 rows; u32 max_nr_ports; u32 emerg_wr; }
        let mut config = Vec::with_capacity(12);
        config.extend_from_slice(&self.columns.to_le_bytes());
        config.extend_from_slice(&self.rows.to_le_bytes());
        config.extend_from_slice(&1u32.to_le_bytes());
        config.extend_from_slice(&0u32.to_le_bytes());
        config
    }

    fn as_any_console(&mut self) -> Option<&mut Console> {
        Some(self)
    }

    fn notify(&mut self, queue_index: u16, queue: &mut Queue, memory: &SharedMemory) -> Result<Vec<u32>> {
        if queue_index == 0 {
            // The guest offered buffers to read into.
            return self.deliver(queue, memory);
        }
        // The guest wrote something.
        let mut irqs = Vec::new();
        while let Some(chain) = queue.pop()? {
            for buffer in &chain.buffers {
                if buffer.writable {
                    continue;
                }
                if let Some(bytes) = crate::machine::guest_bytes(memory, buffer.address as u32, buffer.length) {
                    (self.output)(&bytes);
                }
            }
            irqs.push(queue.release(chain, 0)?);
        }
        Ok(irqs)
    }
}
