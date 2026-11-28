//! Stack panel: the stack pointer and the memory window around it.

use emma65::emulator::Processor;
use emma65::emulator::cpu::{CpuLiveSnapshot, Registers};
use tauri::State;

use crate::CpuState;
use crate::cpu::{stack_pointer_address, stack_top_address};
use crate::disassembly::LiveSnapshotRx;

/// Stack snapshot returned to the frontend: the window of memory the CPU reports as its
/// stack (`Processor::stack_window`), and where the stack pointer is within it. Every
/// address is a full bus address. For the 6502 the window is the whole of page 1, so the
/// frontend can render any part of the stack, wrapping around the page.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct StackSnapshot {
    /// Where the stack pointer register points.
    pub pointer: u16,
    /// The address of the byte most recently pushed. The 6502's pointer is one below it.
    pub top: u16,
    /// The address of `bytes[0]`.
    pub base: u16,
    /// The window's contents.
    pub bytes: Vec<u8>,
}

impl StackSnapshot {
    /// Reads `cpu`'s stack window with `Bus::peek_range`, so no device side effects occur.
    fn from_cpu(cpu: &emma65::emulator::Cpu) -> Result<Self, String> {
        let (base, len) = cpu.stack_window();
        let mut bytes = vec![0u8; usize::from(len)];
        cpu.bus()
            .peek_range(base, &mut bytes)
            .map_err(|e| e.to_string())?;
        Ok(Self::new(cpu.registers(), base, bytes))
    }

    /// Takes the window a free-running CPU's live snapshot captured, the same window
    /// `from_cpu` reads.
    fn from_live(live: CpuLiveSnapshot) -> Self {
        Self::new(&live.registers, live.stack_base, live.stack)
    }

    fn new(regs: &Registers, base: u16, bytes: Vec<u8>) -> Self {
        Self {
            pointer: stack_pointer_address(regs),
            top: stack_top_address(regs),
            base,
            bytes,
        }
    }
}

/// Returns the stack pointer and the CPU's stack window.
///
/// Falls back to the live snapshot channel when the CPU is free-running.
#[tauri::command]
pub fn get_stack(
    cpu_state: State<CpuState>,
    live_snapshot_rx: State<LiveSnapshotRx>,
) -> Result<StackSnapshot, String> {
    let guard = cpu_state.0.lock().unwrap();
    if let Some(cpu) = guard.as_ref() {
        return StackSnapshot::from_cpu(cpu);
    }
    // CPU is free-running — read from the live snapshot channel.
    live_snapshot_rx
        .0
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|rx| rx.borrow().clone())
        .map(StackSnapshot::from_live)
        .ok_or_else(|| "CPU not ready".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use emma65::emulator::bus::{AddressRange, Bus};
    use emma65::emulator::{Cpu, CpuVariant};

    fn cpu_with_stack(s: u8) -> Cpu {
        let bus = Bus::config()
            .ram_with_fill(AddressRange::new(0x0000, 0x7fff), 0)
            .unwrap()
            .build();
        let mut cpu = Cpu::builder(CpuVariant::Wdc65C02).bus(bus).build().unwrap();
        for (i, slot) in (0x0100u16..=0x01ff).enumerate() {
            cpu.bus_mut().patch(slot, i as u8);
        }
        cpu.registers_mut().s = s;
        cpu
    }

    #[test]
    fn the_6502_stack_is_page_1_with_full_addresses() {
        let snap = StackSnapshot::from_cpu(&cpu_with_stack(0xfb)).unwrap();
        assert_eq!(snap.pointer, 0x01fb);
        assert_eq!(snap.top, 0x01fc);
        assert_eq!(snap.base, 0x0100);
        assert_eq!(snap.bytes.len(), 256);
        assert_eq!(snap.bytes[0xfc], 0xfc);
    }

    #[test]
    fn the_live_snapshot_reads_the_same_window_as_a_halted_cpu() {
        let cpu = cpu_with_stack(0x42);
        let halted = StackSnapshot::from_cpu(&cpu).unwrap();
        let (base, len) = cpu.stack_window();
        let mut stack = vec![0u8; usize::from(len)];
        cpu.bus().peek_range(base, &mut stack).unwrap();
        let live = StackSnapshot::new(cpu.registers(), base, stack);
        assert_eq!(live, halted);
    }
}
