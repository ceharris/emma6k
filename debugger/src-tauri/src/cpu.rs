//! The session's CPU: [`DebugCpu`], which holds either family's CPU, what it looks like
//! to the panels ([`CpuDescription`]), and the register access the Register and Trace
//! panels build on.
//!
//! The frontend renders the Register panel, the Trace panel's register columns and the
//! status bar's interrupt controls from the description, rather than from CPU constants
//! of its own.
//!
//! Code that only needs what [`Processor`] provides goes through `DebugCpu`'s
//! delegating methods, which match the enum and call the same trait method on either
//! arm. Free-running matches once and runs the family's own monomorphic loop (see
//! `disassembly::run_cpu`), so no `dyn Processor` is ever on the hot path.

use std::collections::BTreeMap;
use std::sync::Mutex;

use emma6k::family::Family;
use emma09::cpu::cc::ConditionCodes;
use emma65::emulator::bus::{Bus, InterruptController};
use emma65::emulator::error::ExecError;
use emma65::emulator::watch::{Operand, WatchError, WatchEvaluator};
use emma65::emulator::{CpuVariant, LogSender, Processor, StatusRegister};
use tauri::State;

/// The 6502's CPU and registers.
type Cpu65 = emma65::emulator::Cpu;
type Registers65 = emma65::emulator::Registers;
/// The 6809's CPU and registers.
type Cpu09 = emma09::Cpu;
type Registers09 = emma09::Registers;

/// The session's CPU, of either family.
#[expect(
    clippy::large_enum_variant,
    reason = "there's only ever one, and the run loop takes the CPU by value, unboxed"
)]
pub enum DebugCpu {
    Mos6502(Cpu65),
    Mc6809(Cpu09),
}

impl From<Cpu65> for DebugCpu {
    fn from(cpu: Cpu65) -> Self {
        DebugCpu::Mos6502(cpu)
    }
}

impl From<Cpu09> for DebugCpu {
    fn from(cpu: Cpu09) -> Self {
        DebugCpu::Mc6809(cpu)
    }
}

/// Runs `$body` with `$p` bound to the CPU in either arm of a [`DebugCpu`]. Each arm
/// is compiled for its own concrete type, so `$body` may call `Processor` methods and
/// generic functions.
macro_rules! with_processor {
    ($cpu:expr, $p:ident => $body:expr) => {
        match $cpu {
            $crate::cpu::DebugCpu::Mos6502($p) => $body,
            $crate::cpu::DebugCpu::Mc6809($p) => $body,
        }
    };
}
pub(crate) use with_processor;

impl DebugCpu {
    /// The CPU's family.
    pub fn family(&self) -> Family {
        match self {
            DebugCpu::Mos6502(_) => Family::Mos6502,
            DebugCpu::Mc6809(_) => Family::Mc6809,
        }
    }

    /// What the CPU looks like to the panels.
    pub fn description(&self) -> CpuDescription {
        match self {
            DebugCpu::Mos6502(cpu) => describe_6502(cpu.variant()),
            DebugCpu::Mc6809(_) => describe_6809(),
        }
    }

    pub fn bus(&self) -> &Bus {
        with_processor!(self, p => Processor::bus(p))
    }

    pub fn bus_mut(&mut self) -> &mut Bus {
        with_processor!(self, p => Processor::bus_mut(p))
    }

    pub fn interrupts(&self) -> &InterruptController {
        with_processor!(self, p => Processor::interrupts(p))
    }

    pub fn interrupts_mut(&mut self) -> &mut InterruptController {
        with_processor!(self, p => Processor::interrupts_mut(p))
    }

    pub fn evaluator_mut(&mut self) -> &mut WatchEvaluator {
        with_processor!(self, p => Processor::evaluator_mut(p))
    }

    #[cfg(test)]
    pub fn evaluator(&self) -> &WatchEvaluator {
        with_processor!(self, p => Processor::evaluator(p))
    }

    pub fn add_breakpoint(&mut self, addr: u16) {
        with_processor!(self, p => Processor::add_breakpoint(p, addr))
    }

    pub fn remove_breakpoint(&mut self, addr: u16) -> bool {
        with_processor!(self, p => Processor::remove_breakpoint(p, addr))
    }

    #[cfg(test)]
    pub fn breakpoints(&self) -> &std::collections::HashSet<u16> {
        with_processor!(self, p => Processor::breakpoints(p))
    }

    pub fn cycles(&self) -> u64 {
        with_processor!(self, p => Processor::cycles(p))
    }

    pub fn is_stopped(&self) -> bool {
        with_processor!(self, p => Processor::is_stopped(p))
    }

    pub fn is_waiting(&self) -> bool {
        with_processor!(self, p => Processor::is_waiting(p))
    }

    /// The status a program asked to exit with through the `exit` device, once the CPU
    /// has stopped for it.
    pub fn exit_status(&self) -> Option<u8> {
        with_processor!(self, p => p.machine().exit_status())
    }

    pub fn reset(&mut self) -> Result<(), ExecError> {
        with_processor!(self, p => Processor::reset(p))
    }

    /// The memory window the CPU reports as its stack: `(base, len)`.
    pub fn stack_window(&self) -> (u16, u16) {
        with_processor!(self, p => Processor::stack_window(p))
    }

    pub fn set_log_sender(&mut self, sender: LogSender) {
        with_processor!(self, p => p.machine_mut().set_log_sender(sender))
    }

    /// A copy of the registers.
    pub fn registers(&self) -> DebugRegisters {
        match self {
            DebugCpu::Mos6502(cpu) => DebugRegisters::Mos6502(*cpu.registers()),
            DebugCpu::Mc6809(cpu) => DebugRegisters::Mc6809(*Processor::registers(cpu)),
        }
    }

    /// The program counter.
    pub fn pc(&self) -> u16 {
        with_processor!(self, p => Processor::pc(p))
    }

    /// Sets the register `name` (as the description names it) to `value`, which must
    /// fit the register's width.
    pub fn set_register(&mut self, name: &str, value: u32) -> Result<(), String> {
        match self {
            DebugCpu::Mos6502(cpu) => set_register_6502(cpu.registers_mut(), name, value),
            DebugCpu::Mc6809(cpu) => set_register_6809(cpu.registers_mut(), name, value),
        }
    }

    /// Evaluates every watchpoint in `evaluator` against the current state, one result
    /// per watchpoint, reading memory in the CPU's byte order.
    pub fn evaluate_watchpoints(
        &self,
        evaluator: &mut WatchEvaluator,
    ) -> Vec<Result<Operand, WatchError>> {
        match self {
            DebugCpu::Mos6502(cpu) => cpu.evaluate_watchpoints(evaluator),
            DebugCpu::Mc6809(cpu) => cpu.evaluate_watchpoints(evaluator),
        }
    }

    /// The register and flag names watchpoint expressions may use on this CPU.
    pub fn watch_names(&self) -> WatchNames {
        WatchNames::of(self.family())
    }
}

/// The register and flag names a family's watchpoint expressions may use.
#[derive(Copy, Clone)]
pub struct WatchNames {
    pub register: fn(&str) -> Option<Operand>,
    pub flag: fn(&str) -> Option<Operand>,
}

impl WatchNames {
    pub fn of(family: Family) -> Self {
        match family {
            Family::Mos6502 => Self {
                register: emma65::emulator::map_register_name,
                flag: emma65::emulator::map_flag_name,
            },
            Family::Mc6809 => Self {
                register: emma09::cpu::map_register_name,
                flag: emma09::cpu::map_flag_name,
            },
        }
    }
}

/// A copy of either family's registers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DebugRegisters {
    Mos6502(Registers65),
    Mc6809(Registers09),
}

impl From<Registers65> for DebugRegisters {
    fn from(regs: Registers65) -> Self {
        DebugRegisters::Mos6502(regs)
    }
}

impl From<Registers09> for DebugRegisters {
    fn from(regs: Registers09) -> Self {
        DebugRegisters::Mc6809(regs)
    }
}

impl DebugRegisters {
    /// Every register's value, keyed by the description's names.
    pub fn values(&self) -> BTreeMap<&'static str, u32> {
        match self {
            DebugRegisters::Mos6502(regs) => register_values_6502(regs),
            DebugRegisters::Mc6809(regs) => register_values_6809(regs),
        }
    }

    /// The program counter.
    pub fn pc(&self) -> u16 {
        match self {
            DebugRegisters::Mos6502(regs) => regs.pc,
            DebugRegisters::Mc6809(regs) => regs.pc,
        }
    }

    /// The flags register's value, for working out which flags a step changed.
    pub fn flags(&self) -> u32 {
        match self {
            DebugRegisters::Mos6502(regs) => u32::from(regs.p.to_byte()),
            DebugRegisters::Mc6809(regs) => u32::from(regs.cc.to_byte()),
        }
    }

    /// Where the stack pointer points, as a bus address. The 6502's S is an offset
    /// into page 1; the 6809's S is a full address.
    pub fn stack_pointer_address(&self) -> u16 {
        match self {
            DebugRegisters::Mos6502(regs) => STACK_PAGE | u16::from(regs.s),
            DebugRegisters::Mc6809(regs) => regs.s,
        }
    }

    /// The address of the byte most recently pushed. The 6502 stores and then
    /// decrements S, so S points at the next free slot, one below that byte, wrapping
    /// within page 1. The 6809 decrements and then stores, so S points at that byte.
    pub fn stack_top_address(&self) -> u16 {
        match self {
            DebugRegisters::Mos6502(regs) => STACK_PAGE | u16::from(regs.s.wrapping_add(1)),
            DebugRegisters::Mc6809(regs) => regs.s,
        }
    }
}

/// What a register is for, which decides where the panels show it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisterRole {
    Data,
    Index,
    Stack,
    Pc,
    Flags,
}

/// One register, in the order the Register panel lists it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RegisterDescription {
    /// The register's name, which is also its key in a snapshot's `values` and the name
    /// `set_register` takes.
    pub name: &'static str,
    pub width_bits: u8,
    pub role: RegisterRole,
}

/// One bit of the flags register.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct FlagDescription {
    /// The character shown when the flag is set. An unused bit's label is `-`, and the
    /// panels never mark it changed or let it be toggled.
    pub label: &'static str,
    /// The bit's position in the flags register, 0 for the least significant.
    pub bit: u8,
    pub name: &'static str,
}

/// The CPU's hardware stack.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct StackDescription {
    /// The name of the stack pointer register.
    pub pointer: &'static str,
    /// True when the stack is confined to one page (the 6502's page 1).
    pub page_fixed: bool,
}

/// The byte order of a word in memory.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ByteOrder {
    /// Least significant byte first: the 6502.
    Little,
    /// Most significant byte first: the 6809.
    Big,
}

impl ByteOrder {
    /// Assembles `bytes`, in address order, into an unsigned value.
    pub fn assemble(self, bytes: impl DoubleEndedIterator<Item = u8>) -> u32 {
        let push = |value: u32, byte: u8| (value << 8) | u32::from(byte);
        match self {
            ByteOrder::Little => bytes.rev().fold(0, push),
            ByteOrder::Big => bytes.fold(0, push),
        }
    }
}

/// What the session's CPU looks like, sent to the frontend by `get_cpu_description` and
/// as the payload of the `session-loaded` event.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CpuDescription {
    pub family: &'static str,
    pub variant: String,
    pub registers: Vec<RegisterDescription>,
    /// The flags register's bits, high bit first.
    pub flags: Vec<FlagDescription>,
    /// The name of the register `flags` describes.
    pub flags_register: &'static str,
    pub interrupt_lines: Vec<&'static str>,
    pub stack: StackDescription,
    pub byte_order: ByteOrder,
    pub max_instruction_len: u8,
    /// True when the Assembler panel can assemble for this CPU.
    pub assembler: bool,
}

/// The loaded session's CPU description, `None` before the first session loads.
pub struct CpuDescriptionState(pub Mutex<Option<CpuDescription>>);

/// Returns the loaded session's CPU description.
#[tauri::command]
pub fn get_cpu_description(
    description_state: State<CpuDescriptionState>,
) -> Result<CpuDescription, String> {
    description_state
        .0
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "CPU not ready".to_string())
}

/// The 6502's registers, in the Register panel's order.
const MOS6502_REGISTERS: [RegisterDescription; 6] = [
    register("A", 8, RegisterRole::Data),
    register("X", 8, RegisterRole::Index),
    register("Y", 8, RegisterRole::Index),
    register("PC", 16, RegisterRole::Pc),
    register("S", 8, RegisterRole::Stack),
    register("P", 8, RegisterRole::Flags),
];

const fn register(name: &'static str, width_bits: u8, role: RegisterRole) -> RegisterDescription {
    RegisterDescription {
        name,
        width_bits,
        role,
    }
}

/// Describes the 6502 `variant`.
pub fn describe_6502(variant: CpuVariant) -> CpuDescription {
    let flag = |label, bit, name| FlagDescription { label, bit, name };
    CpuDescription {
        family: "6502",
        variant: match variant {
            CpuVariant::Cmos65C02 => "65C02",
            CpuVariant::Wdc65C02 => "WDC65C02",
        }
        .to_string(),
        registers: MOS6502_REGISTERS.to_vec(),
        flags: vec![
            flag("N", 7, "Negative"),
            flag("V", 6, "Overflow"),
            flag("-", 5, "Unused"),
            flag("B", 4, "Break"),
            flag("D", 3, "Decimal"),
            flag("I", 2, "Interrupt disable"),
            flag("Z", 1, "Zero"),
            flag("C", 0, "Carry"),
        ],
        flags_register: "P",
        interrupt_lines: vec!["IRQ", "NMI"],
        stack: StackDescription {
            pointer: "S",
            page_fixed: true,
        },
        byte_order: ByteOrder::Little,
        max_instruction_len: 3,
        assembler: true,
    }
}

/// The 6502's register values, keyed by the description's names.
fn register_values_6502(regs: &Registers65) -> BTreeMap<&'static str, u32> {
    BTreeMap::from([
        ("A", u32::from(regs.a)),
        ("X", u32::from(regs.x)),
        ("Y", u32::from(regs.y)),
        ("PC", u32::from(regs.pc)),
        ("S", u32::from(regs.s)),
        ("P", u32::from(regs.p.to_byte())),
    ])
}

/// The 6502's stack page.
const STACK_PAGE: u16 = 0x0100;

/// Checks that `name` is one of `registers` and `value` fits its width.
fn check_register(registers: &[RegisterDescription], name: &str, value: u32) -> Result<(), String> {
    let description = registers
        .iter()
        .find(|r| r.name == name)
        .ok_or_else(|| format!("Unknown register: {name}"))?;
    let max = (1u32 << description.width_bits) - 1;
    if value > max {
        return Err(format!("{name} value out of range: must be 0-{max}"));
    }
    Ok(())
}

/// Sets the 6502 register `name` to `value`, which must fit the register's width.
fn set_register_6502(regs: &mut Registers65, name: &str, value: u32) -> Result<(), String> {
    check_register(&MOS6502_REGISTERS, name, value)?;
    // Both casts are exact: the width check above bounds `value`.
    let byte = value as u8;
    match name {
        "A" => regs.a = byte,
        "X" => regs.x = byte,
        "Y" => regs.y = byte,
        "S" => regs.s = byte,
        "P" => regs.p = StatusRegister::from_byte(byte) | StatusRegister::UNUSED,
        "PC" => regs.pc = value as u16,
        _ => unreachable!("every described register is handled"),
    }
    Ok(())
}

/// The 6809's registers, in the Register panel's order. D is A and B together, so
/// editing it writes both. U is a stack pointer, so it sits beside S.
const MC6809_REGISTERS: [RegisterDescription; 10] = [
    register("A", 8, RegisterRole::Data),
    register("B", 8, RegisterRole::Data),
    register("D", 16, RegisterRole::Data),
    register("DP", 8, RegisterRole::Data),
    register("X", 16, RegisterRole::Index),
    register("Y", 16, RegisterRole::Index),
    register("U", 16, RegisterRole::Stack),
    register("S", 16, RegisterRole::Stack),
    register("PC", 16, RegisterRole::Pc),
    register("CC", 8, RegisterRole::Flags),
];

/// Describes the 6809.
pub fn describe_6809() -> CpuDescription {
    let flag = |label, bit, name| FlagDescription { label, bit, name };
    CpuDescription {
        family: "6809",
        variant: "6809".to_string(),
        registers: MC6809_REGISTERS.to_vec(),
        flags: vec![
            flag("E", 7, "Entire state stacked"),
            flag("F", 6, "FIRQ mask"),
            flag("H", 5, "Half carry"),
            flag("I", 4, "IRQ mask"),
            flag("N", 3, "Negative"),
            flag("Z", 2, "Zero"),
            flag("V", 1, "Overflow"),
            flag("C", 0, "Carry"),
        ],
        flags_register: "CC",
        interrupt_lines: vec!["IRQ", "FIRQ", "NMI"],
        stack: StackDescription {
            pointer: "S",
            page_fixed: false,
        },
        byte_order: ByteOrder::Big,
        max_instruction_len: 5,
        // The Assembler panel has no 6809 assembler yet.
        assembler: false,
    }
}

/// The 6809's register values, keyed by the description's names.
fn register_values_6809(regs: &Registers09) -> BTreeMap<&'static str, u32> {
    BTreeMap::from([
        ("A", u32::from(regs.a)),
        ("B", u32::from(regs.b)),
        ("D", u32::from(regs.d())),
        ("DP", u32::from(regs.dp)),
        ("X", u32::from(regs.x)),
        ("Y", u32::from(regs.y)),
        ("U", u32::from(regs.u)),
        ("S", u32::from(regs.s)),
        ("PC", u32::from(regs.pc)),
        ("CC", u32::from(regs.cc.to_byte())),
    ])
}

/// Sets the 6809 register `name` to `value`, which must fit the register's width.
/// Setting D sets A and B. Setting S here doesn't arm NMI: only a program's own load
/// of S does that.
fn set_register_6809(regs: &mut Registers09, name: &str, value: u32) -> Result<(), String> {
    check_register(&MC6809_REGISTERS, name, value)?;
    // Both casts are exact: the width check above bounds `value`.
    let byte = value as u8;
    let word = value as u16;
    match name {
        "A" => regs.a = byte,
        "B" => regs.b = byte,
        "D" => regs.set_d(word),
        "DP" => regs.dp = byte,
        "X" => regs.x = word,
        "Y" => regs.y = word,
        "U" => regs.u = word,
        "S" => regs.s = word,
        "PC" => regs.pc = word,
        "CC" => regs.cc = ConditionCodes::from_byte(byte),
        _ => unreachable!("every described register is handled"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zeroed() -> Registers65 {
        Registers65 {
            a: 0,
            x: 0,
            y: 0,
            s: 0,
            pc: 0,
            p: StatusRegister::UNUSED,
        }
    }

    fn zeroed_6809() -> Registers09 {
        Registers09 {
            a: 0,
            b: 0,
            dp: 0,
            cc: ConditionCodes::empty(),
            x: 0,
            y: 0,
            u: 0,
            s: 0,
            pc: 0,
        }
    }

    fn values(regs: impl Into<DebugRegisters>) -> BTreeMap<&'static str, u32> {
        regs.into().values()
    }

    fn sorted_names(description: &CpuDescription) -> Vec<&'static str> {
        let mut names: Vec<_> = description.registers.iter().map(|r| r.name).collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn the_6502_description_names_every_register_a_snapshot_has() {
        let description = describe_6502(CpuVariant::Wdc65C02);
        assert_eq!(
            sorted_names(&description),
            values(zeroed()).keys().copied().collect::<Vec<_>>()
        );
        assert_eq!(description.variant, "WDC65C02");
        assert_eq!(describe_6502(CpuVariant::Cmos65C02).variant, "65C02");
    }

    #[test]
    fn the_6809_description_names_every_register_a_snapshot_has() {
        let description = describe_6809();
        assert_eq!(
            sorted_names(&description),
            values(zeroed_6809()).keys().copied().collect::<Vec<_>>()
        );
        let order: Vec<_> = description.registers.iter().map(|r| r.name).collect();
        assert_eq!(order, ["A", "B", "D", "DP", "X", "Y", "U", "S", "PC", "CC"]);
        assert_eq!(description.interrupt_lines, ["IRQ", "FIRQ", "NMI"]);
        assert_eq!(description.byte_order, ByteOrder::Big);
        assert!(!description.stack.page_fixed);
        assert_eq!(description.max_instruction_len, 5);
    }

    #[test]
    fn the_6502_flags_run_high_bit_first() {
        let flags = describe_6502(CpuVariant::Cmos65C02).flags;
        let labels: String = flags.iter().map(|f| f.label).collect();
        assert_eq!(labels, "NV-BDIZC");
        let bits: Vec<u8> = flags.iter().map(|f| f.bit).collect();
        assert_eq!(bits, [7, 6, 5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn the_6809_flags_match_the_condition_code_bits() {
        let flags = describe_6809().flags;
        let labels: String = flags.iter().map(|f| f.label).collect();
        assert_eq!(labels, "EFHINZVC");
        for flag in &flags {
            let mask = emma09::cpu::map_flag_name(flag.label).unwrap();
            assert_eq!(mask, 1 << flag.bit, "{}", flag.label);
        }
    }

    #[test]
    fn set_register_writes_each_6502_register() {
        let mut regs = zeroed();
        for (name, value) in [
            ("A", 0x12),
            ("X", 0x34),
            ("Y", 0x56),
            ("S", 0x78),
            ("PC", 0x9abc),
        ] {
            set_register_6502(&mut regs, name, value).unwrap();
            assert_eq!(values(regs)[name], value, "{name}");
        }
    }

    #[test]
    fn set_register_keeps_the_6502_p_unused_bit_set() {
        let mut regs = zeroed();
        set_register_6502(&mut regs, "P", 0x00).unwrap();
        assert_eq!(
            DebugRegisters::from(regs).flags(),
            u32::from(StatusRegister::UNUSED.to_byte())
        );
    }

    #[test]
    fn set_register_writes_each_6809_register() {
        let mut regs = zeroed_6809();
        for (name, value) in [
            ("A", 0x12),
            ("B", 0x34),
            ("DP", 0x56),
            ("X", 0x789a),
            ("Y", 0xbcde),
            ("U", 0x1357),
            ("S", 0x2468),
            ("PC", 0xf00d),
            ("CC", 0xa5),
        ] {
            set_register_6809(&mut regs, name, value).unwrap();
            assert_eq!(values(regs)[name], value, "{name}");
        }
        assert_eq!(values(regs)["D"], 0x1234);
    }

    #[test]
    fn setting_the_6809_d_sets_a_and_b() {
        let mut regs = zeroed_6809();
        set_register_6809(&mut regs, "D", 0xbeef).unwrap();
        assert_eq!((regs.a, regs.b), (0xbe, 0xef));
        assert!(set_register_6809(&mut regs, "D", 0x1_0000).is_err());
        assert!(set_register_6809(&mut regs, "A", 0x100).is_err());
        assert!(set_register_6809(&mut regs, "P", 0).is_err());
    }

    #[test]
    fn byte_order_assembles_in_its_own_order() {
        let bytes = [0x12, 0x34, 0x56, 0x78];
        assert_eq!(ByteOrder::Little.assemble(bytes.into_iter()), 0x7856_3412);
        assert_eq!(ByteOrder::Big.assemble(bytes.into_iter()), 0x1234_5678);
        assert_eq!(ByteOrder::Little.assemble([0xab].into_iter()), 0xab);
        assert_eq!(ByteOrder::Big.assemble([0xab].into_iter()), 0xab);
    }

    #[test]
    fn the_6502_stack_top_is_one_above_s_within_page_1() {
        let mut regs = zeroed();
        regs.s = 0xfd;
        let r = DebugRegisters::from(regs);
        assert_eq!(r.stack_pointer_address(), 0x01fd);
        assert_eq!(r.stack_top_address(), 0x01fe);
        regs.s = 0xff;
        let r = DebugRegisters::from(regs);
        assert_eq!(r.stack_pointer_address(), 0x01ff);
        assert_eq!(r.stack_top_address(), 0x0100);
    }

    #[test]
    fn the_6809_stack_top_is_s_itself() {
        let mut regs = zeroed_6809();
        regs.s = 0x7ff0;
        let r = DebugRegisters::from(regs);
        assert_eq!(r.stack_pointer_address(), 0x7ff0);
        assert_eq!(r.stack_top_address(), 0x7ff0);
    }

    #[test]
    fn set_register_rejects_out_of_range_and_unknown_names() {
        let mut regs = zeroed();
        assert!(set_register_6502(&mut regs, "A", 0x100).is_err());
        assert!(set_register_6502(&mut regs, "PC", 0x1_0000).is_err());
        assert!(set_register_6502(&mut regs, "PC", 0xffff).is_ok());
        assert!(set_register_6502(&mut regs, "B", 0).is_err());
        assert!(set_register_6502(&mut regs, "a", 0).is_err());
    }
}
