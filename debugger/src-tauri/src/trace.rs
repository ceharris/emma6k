//! Trace panel: live-recorded execution trace and windowed reads.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tauri::State;

use emma65::emulator::SymbolTable;
use emma65::emulator::text::immediate_comment;
use emma65::emulator::trace::{
    BinaryTraceReader, BinaryTraceWriter, ChannelTraceCallback, OverflowPolicy, TraceCallback,
    TraceKind, TraceRecord, TraceRegisters, spawn_trace_writer,
};

use crate::CpuState;
use crate::cpu::{DebugCpu, DebugRegisters};

/// Capacity of the bounded channel between the CPU thread and the trace writer thread.
const TRACE_CHANNEL_CAPACITY: usize = 4096;

/// Recording state and bookkeeping for the trace window, managed by Tauri.
pub struct TraceState(pub Mutex<TraceData>);

/// The data behind [`TraceState`].
pub struct TraceData {
    /// Path of the trace file currently being recorded, or last recorded.
    path: Option<PathBuf>,
    /// Record ordinal where each row's `Registers` record begins, in row order.
    /// Shared with the [`RowIndexingTraceCallback`] installed on the CPU while
    /// recording, so it can grow independently of this struct's own lock.
    row_index: Arc<Mutex<Vec<u64>>>,
    /// True while a recording is in progress.
    recording: bool,
    /// Join handle for the writer thread, awaited by `stop_trace` so the file
    /// is guaranteed flushed before the command returns.
    writer_handle: Option<JoinHandle<()>>,
}

impl TraceData {
    /// Creates an empty, not-yet-recording state.
    pub fn new() -> Self {
        Self {
            path: None,
            row_index: Arc::new(Mutex::new(Vec::new())),
            recording: false,
            writer_handle: None,
        }
    }
}

impl Default for TraceData {
    fn default() -> Self {
        Self::new()
    }
}

/// Wraps a [`ChannelTraceCallback`], counting records and indexing the start
/// of each row (its `Registers` record) into a shared `row_index`, the same
/// way `DisassemblingTraceCallback` wraps a callback in the library.
struct RowIndexingTraceCallback<R> {
    inner: ChannelTraceCallback<R>,
    row_index: Arc<Mutex<Vec<u64>>>,
    record_count: u64,
}

impl<R: Send> TraceCallback<R> for RowIndexingTraceCallback<R> {
    fn record(&mut self, rec: TraceRecord<R>) {
        if matches!(rec.kind, TraceKind::Registers(_)) {
            self.row_index.lock().unwrap().push(self.record_count);
        }
        self.inner.record(rec);
        self.record_count += 1;
    }

    fn flush(&mut self) {
        self.inner.flush();
    }
}

/// One bus operation in a [`TraceRowDto`]'s detail pane.
#[derive(Clone, serde::Serialize)]
pub struct TraceBusOpDto {
    /// The bus address accessed.
    pub addr: u16,
    /// `"Read"` or `"Write"`.
    pub op: String,
    /// The byte value read or written.
    pub value: u8,
    /// Alternate representations of `value` (decimal, ASCII/signed), the same
    /// text `emma65-tracer` shows alongside a bus op's hex value.
    pub comment: String,
}

/// One reconstructed trace row returned to the frontend.
#[derive(Clone, serde::Serialize)]
pub struct TraceRowDto {
    /// 1-based sequence number (`instr_id + 1`), matching the `emma6k-tracer` CLI's numbering.
    pub seq: u64,
    /// Instruction address (for a serviced interrupt, the PC it interrupted).
    pub addr: u16,
    /// Raw bytes as hex strings, e.g. ["4C", "00", "06"]; empty for an interrupt row.
    pub bytes: Vec<String>,
    /// Symbol names from the symbol table associated with `addr`, without trailing colons.
    pub labels: Vec<String>,
    /// Mnemonic string, e.g. "JMP", or for a serviced interrupt its name in angle
    /// brackets, e.g. "<IRQ>", as `emma6k-tracer` shows it.
    pub mnemonic: String,
    /// Formatted operand text, e.g. "$0600"; for an interrupt row, its vector, e.g.
    /// "[$FFF8]".
    pub operand: String,
    /// Comment string.
    pub comment: String,
    /// False for invalid opcodes under the active variant.
    pub is_valid: bool,
    /// The instruction's (or interrupt's) total cycle count, if known.
    pub cycles: Option<u8>,
    /// Every register in the CPU description, keyed by its name, as it stood immediately
    /// before this instruction executed.
    pub registers: BTreeMap<&'static str, u32>,
    /// Bus reads and writes observed while this instruction executed, in stream order.
    pub bus_ops: Vec<TraceBusOpDto>,
}

/// A windowed page of trace rows, returned by `get_trace_window`.
#[derive(Clone, serde::Serialize)]
pub struct TraceWindowPage {
    /// The rows in this page, in row order.
    pub rows: Vec<TraceRowDto>,
    /// Total rows recorded so far, letting the frontend compute the tail window as
    /// `start_row = total_rows.saturating_sub(viewport_rows)` on each poll.
    pub total_rows: usize,
}

/// Recording/browsing status, returned by `get_trace_status`.
#[derive(Clone, serde::Serialize)]
pub struct TraceStatus {
    /// True while a recording is in progress.
    pub recording: bool,
    /// The trace file currently or most recently recorded, if any.
    pub path: Option<String>,
}

/// Starts recording an execution trace to `path`, spooling records to a
/// dedicated writer thread. Only callable while the CPU is halted.
#[tauri::command]
pub fn record_trace(
    path: String,
    cpu_state: State<CpuState>,
    trace_state: State<TraceState>,
) -> Result<(), String> {
    let mut cpu_guard = cpu_state.0.lock().unwrap();
    let cpu = cpu_guard.as_mut().ok_or("CPU not ready")?;

    let file = File::create(&path).map_err(|e| e.to_string())?;
    let row_index = Arc::new(Mutex::new(Vec::new()));
    let writer_handle = match cpu {
        DebugCpu::Mos6502(cpu) => {
            let (callback, handle) = start_writer(file, cpu.variant(), &row_index);
            cpu.set_trace_callback(Some(callback));
            handle
        }
        DebugCpu::Mc6809(cpu) => {
            let (callback, handle) = start_writer(file, cpu.variant(), &row_index);
            cpu.set_trace_callback(Some(callback));
            handle
        }
    };

    let mut state = trace_state.0.lock().unwrap();
    state.path = Some(PathBuf::from(&path));
    state.row_index = row_index;
    state.recording = true;
    state.writer_handle = Some(writer_handle);

    Ok(())
}

/// Starts a writer thread recording `R`'s trace to `file`, and returns the callback
/// to install on the CPU, which also indexes each row's start into `row_index`.
fn start_writer<R: TraceRegisters>(
    file: File,
    variant: R::Variant,
    row_index: &Arc<Mutex<Vec<u64>>>,
) -> (Box<dyn TraceCallback<R>>, JoinHandle<()>) {
    let writer = BinaryTraceWriter::<File, R>::new(file, variant);
    // BlockOnFull, unlike the CLI capture use case's DropOnFull: a debugger
    // trace that silently drops instructions would be misleading. `flush_trace`
    // is called once per debugger step/run command (see `exec::step_into` and
    // friends), not per record (which cratered free-run throughput, issue #273),
    // so get_trace_window's independent file handle sees every row from a command
    // by the time its `debugger-halted`/`debugger-run-stopped` event fires.
    let (callback, writer_handle, _dropped) =
        spawn_trace_writer(writer, TRACE_CHANNEL_CAPACITY, OverflowPolicy::BlockOnFull);
    let indexing_callback = RowIndexingTraceCallback {
        inner: callback,
        row_index: Arc::clone(row_index),
        record_count: 0,
    };
    (Box::new(indexing_callback), writer_handle)
}

/// Stops the current recording: detaches the trace callback and waits for the
/// writer thread to flush and exit. The recorded file stays browsable via
/// `get_trace_window`; the next `record_trace` call requires a new path.
#[tauri::command]
pub fn stop_trace(
    cpu_state: State<CpuState>,
    trace_state: State<TraceState>,
) -> Result<(), String> {
    let mut cpu_guard = cpu_state.0.lock().unwrap();
    match cpu_guard.as_mut().ok_or("CPU not ready")? {
        DebugCpu::Mos6502(cpu) => cpu.set_trace_callback(None),
        DebugCpu::Mc6809(cpu) => cpu.set_trace_callback(None),
    }
    drop(cpu_guard);

    let mut state = trace_state.0.lock().unwrap();
    if let Some(handle) = state.writer_handle.take() {
        let _ = handle.join();
    }
    state.recording = false;

    Ok(())
}

/// Returns up to `count` reconstructed trace rows starting at `start_row`
/// (clamped into the recorded range), plus the total row count recorded so far.
#[tauri::command]
pub fn get_trace_window(
    start_row: usize,
    count: usize,
    cpu_state: State<CpuState>,
    trace_state: State<TraceState>,
) -> Result<TraceWindowPage, String> {
    let cpu_guard = cpu_state.0.lock().unwrap();
    let cpu = cpu_guard.as_ref().ok_or("CPU not ready")?;

    let state = trace_state.0.lock().unwrap();
    let path = state.path.as_ref().ok_or("No trace recorded yet")?;
    // Only the length and one offset are ever needed here, so read them out
    // without cloning the whole index — with a long recording, `row_index`
    // can hold millions of entries, and cloning it on every scroll/poll made
    // the window sluggish even though the DOM itself stays small (issue #272).
    let (total_rows, start_row, seek_record) = {
        let row_offsets = state.row_index.lock().unwrap();
        let total_rows = row_offsets.len();
        if total_rows == 0 {
            return Ok(TraceWindowPage {
                rows: Vec::new(),
                total_rows: 0,
            });
        }
        let start_row = start_row.min(total_rows - 1);
        (total_rows, start_row, row_offsets[start_row])
    };

    // Opened fresh per call: simplest option, and an extra open/close per
    // viewport-sized fetch is cheap relative to a UI-paced scroll/poll action.
    let file = File::open(path).map_err(|e| e.to_string())?;
    let count = count.min(total_rows - start_row);
    let symbols = cpu.bus().symbol_table().clone();
    let rows = match cpu {
        DebugCpu::Mos6502(cpu) => read_rows_6502(file, seek_record, count, cpu.variant(), symbols),
        DebugCpu::Mc6809(_) => read_rows_6809(file, seek_record, count, symbols),
    }?;

    Ok(TraceWindowPage { rows, total_rows })
}

/// Opens `file` as `R`'s trace, positioned at record `seek_record`.
fn open_reader<R: TraceRegisters>(
    file: File,
    seek_record: u64,
) -> Result<BinaryTraceReader<File, R>, String> {
    let mut reader = BinaryTraceReader::new(file).map_err(|e| e.to_string())?;
    reader
        .seek_to_record(seek_record)
        .map_err(|e| e.to_string())?;
    Ok(reader)
}

/// Converts a family's `TraceBusOp`s; both families' have the same shape.
macro_rules! bus_op_dtos {
    ($ops:expr, $op:path) => {{
        use $op as Op;
        $ops.map(|op| {
            let (addr, kind, value) = match *op {
                Op::Read { addr, value } => (addr, "Read", value),
                Op::Write { addr, value } => (addr, "Write", value),
            };
            TraceBusOpDto {
                addr,
                op: kind.to_string(),
                value,
                comment: immediate_comment(value),
            }
        })
        .collect()
    }};
}

/// Reconstructs up to `count` 6502 rows from record `seek_record` on.
fn read_rows_6502(
    file: File,
    seek_record: u64,
    count: usize,
    variant: emma65::emulator::CpuVariant,
    symbols: SymbolTable,
) -> Result<Vec<TraceRowDto>, String> {
    use emma65::disassembler::{TraceBusOp, TraceRowAssembler};
    let reader = open_reader::<emma65::emulator::Registers>(file, seek_record)?;
    let mut assembler = TraceRowAssembler::new(variant, symbols);
    let mut rows = Vec::with_capacity(count);
    for item in reader {
        if rows.len() >= count {
            break;
        }
        let rec = item.map_err(|e| e.to_string())?;
        if let Some(row) = assembler.feed(&rec) {
            // Computed up front: it borrows `row` as a whole via `non_fetch_bus_ops(&self)`,
            // which the field-by-field moves out of `row.line` below would otherwise block.
            let bus_ops = bus_op_dtos!(row.non_fetch_bus_ops(), TraceBusOp);
            rows.push(TraceRowDto {
                seq: row.instr_id + 1,
                addr: row.line.addr,
                bytes: hex_bytes(&row.line.raw_bytes),
                labels: row.line.labels,
                mnemonic: row.line.mnemonic.to_string(),
                operand: row.line.operand_text,
                comment: row.line.comment_text.unwrap_or_default(),
                is_valid: row.line.is_valid,
                cycles: row.cycles,
                registers: DebugRegisters::from(row.regs).values(),
                bus_ops,
            });
        }
    }
    Ok(rows)
}

/// Reconstructs up to `count` 6809 rows from record `seek_record` on. A serviced
/// interrupt, which fetches no instruction, becomes a row named from its vector, as
/// `emma6k-tracer` shows it: `<IRQ>` and `[$FFF8]`, at the PC it interrupted.
fn read_rows_6809(
    file: File,
    seek_record: u64,
    count: usize,
    symbols: SymbolTable,
) -> Result<Vec<TraceRowDto>, String> {
    use emma09::disassembler::trace::vector_name;
    use emma09::disassembler::{TraceBusOp, TraceRowAssembler};
    let reader = open_reader::<emma09::Registers>(file, seek_record)?;
    let mut assembler = TraceRowAssembler::new(symbols);
    let mut rows = Vec::with_capacity(count);
    for item in reader {
        if rows.len() >= count {
            break;
        }
        let rec = item.map_err(|e| e.to_string())?;
        let Some(row) = assembler.feed(&rec) else {
            continue;
        };
        let bus_ops = bus_op_dtos!(row.non_fetch_bus_ops(), TraceBusOp);
        let registers = DebugRegisters::from(row.regs).values();
        let dto = match row.line {
            Some(line) => TraceRowDto {
                seq: row.instr_id + 1,
                addr: line.addr,
                bytes: hex_bytes(&line.raw_bytes),
                labels: line.labels,
                mnemonic: line.mnemonic.to_string(),
                operand: line.operand_text,
                comment: line.comment_text.unwrap_or_default(),
                is_valid: line.is_valid,
                cycles: row.cycles,
                registers,
                bus_ops,
            },
            None => {
                let vector = row.vector();
                let name = vector.and_then(vector_name).unwrap_or("INT");
                TraceRowDto {
                    seq: row.instr_id + 1,
                    addr: row.regs.pc,
                    bytes: Vec::new(),
                    labels: Vec::new(),
                    mnemonic: format!("<{name}>"),
                    operand: vector.map(|v| format!("[${v:04X}]")).unwrap_or_default(),
                    comment: String::new(),
                    is_valid: true,
                    cycles: row.cycles,
                    registers,
                    bus_ops,
                }
            }
        };
        rows.push(dto);
    }
    Ok(rows)
}

fn hex_bytes(bytes: &[u8]) -> Vec<String> {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// Returns the current recording status, letting the toolbar restore
/// Record/Stop button state on panel mount.
#[tauri::command]
pub fn get_trace_status(trace_state: State<TraceState>) -> TraceStatus {
    let state = trace_state.0.lock().unwrap();
    TraceStatus {
        recording: state.recording,
        path: state.path.as_ref().map(|p| p.display().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emma65::emulator::bus::{AddressRange, Bus};
    use emma65::emulator::{IrqSource, Processor};

    /// Records `steps` steps of a 6809 that takes an IRQ before its first instruction,
    /// then reads the rows back.
    fn record_6809_irq(steps: usize) -> Vec<TraceRowDto> {
        let mut bus = Bus::config()
            .ram_with_fill(AddressRange::new(0x0000, 0xffff), 0)
            .unwrap()
            .build();
        bus.patch(0xfff8, 0x20); // IRQ vector: $2000
        bus.patch(0xfff9, 0x00);
        bus.patch(0x2000, 0x3b); // RTI
        bus.patch(0x1000, 0x12); // NOP
        let mut cpu = emma09::Cpu::builder().bus(bus).build().unwrap();
        let regs = cpu.registers_mut();
        regs.pc = 0x1000;
        regs.s = 0x8000;
        regs.cc = emma09::cpu::cc::ConditionCodes::empty();

        let dir = std::env::temp_dir().join(format!(
            "emma6k-trace-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("irq.e65t");
        let row_index = Arc::new(Mutex::new(Vec::new()));
        let (callback, handle) =
            start_writer(File::create(&path).unwrap(), cpu.variant(), &row_index);
        cpu.set_trace_callback(Some(callback));

        let source = IrqSource(1);
        cpu.interrupts_mut().assert_irq(source);
        for step in 0..steps {
            cpu.step(None, false);
            if step == 0 {
                cpu.interrupts_mut().release_irq(source);
            }
        }
        cpu.set_trace_callback(None);
        handle.join().unwrap();
        assert_eq!(row_index.lock().unwrap().len(), steps);

        let rows = read_rows_6809(File::open(&path).unwrap(), 0, 10, SymbolTable::new()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        rows
    }

    #[test]
    fn a_serviced_6809_interrupt_is_a_row_named_from_its_vector() {
        let rows = record_6809_irq(3);
        let summary: Vec<_> = rows
            .iter()
            .map(|r| (r.addr, r.mnemonic.as_str(), r.operand.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                (0x1000, "<IRQ>", "[$FFF8]"),
                (0x2000, "RTI", ""),
                (0x1000, "NOP", ""),
            ]
        );
        let irq = &rows[0];
        assert!(irq.bytes.is_empty());
        assert_eq!(irq.cycles, Some(19));
        assert_eq!(irq.registers["PC"], 0x1000);
        assert_eq!(irq.registers["S"], 0x8000);
        // The entire state is stacked: twelve bytes below $8000.
        let writes = irq.bus_ops.iter().filter(|op| op.op == "Write").count();
        assert_eq!(writes, 12);
        assert_eq!(rows[1].registers["S"], 0x8000 - 12);
    }
}
