//! Disassembly panel: run/step/stop controls and disassembly listing.

use std::sync::atomic::AtomicU16;
use std::sync::{Arc, Mutex};

use crate::CpuState;
use crate::cpu::{DebugCpu, DebugRegisters, with_processor};
use crate::cpu_bus::{
    CpuBusCache, UiFirqSourceState, UiIrqSourceState, snapshot_cpu_bus, snapshot_processor,
};
use crate::memory::MemoryViewAddr;
use crate::memory_variables::{self, MemoryVariablesState, ResolvedAddrCache};
use crate::registers::{ChangedFlagsState, RegisterSnapshot};
use emma65::emulator::exec::{
    CpuLiveSnapshot, InterruptCommand, RunHandle, StepResult, run_from as exec_run_from,
    step_into as exec_step_into, step_over_breakpoint as exec_step_over_breakpoint,
    step_over_subroutine as exec_step_over_subroutine, step_return as exec_step_return,
};
use emma65::emulator::{InterruptController, Processor, RunStopper};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{mpsc, watch};

/// Interval between `debugger-running-tick` events emitted during free-run.
const RUNNING_TICK_INTERVAL_MS: u64 = 100;

/// The session's disassembler, for the CPU's family.
#[expect(
    clippy::large_enum_variant,
    reason = "there's only ever one, made once per session"
)]
pub enum DebugDisassembler {
    Mos6502(emma65::disassembler::Disassembler),
    Mc6809(emma09::disassembler::Disassembler),
}

impl DebugDisassembler {
    /// A disassembler for `cpu`'s family (and, for the 6502, its variant).
    pub fn for_cpu(cpu: &DebugCpu) -> Self {
        match cpu {
            DebugCpu::Mos6502(cpu) => {
                Self::Mos6502(emma65::disassembler::Disassembler::new(cpu.variant()))
            }
            DebugCpu::Mc6809(_) => Self::Mc6809(emma09::disassembler::Disassembler::new()),
        }
    }
}

/// Holds the disassembler once the session is ready.
pub struct DisassemblerState(pub Mutex<Option<DebugDisassembler>>);

/// Holds the stopper handle while the CPU is free-running; `None` when halted.
pub struct RunStopperState(pub Mutex<Option<RunStopper>>);

/// When the CPU is halted at a breakpoint or watch trigger, holds that PC so
/// the next step command can skip past it. Cleared after each step or reset.
pub struct SkipBreakpointPc(pub Mutex<Option<u16>>);

/// A live snapshot of either family's CPU.
pub type LiveSnapshot = CpuLiveSnapshot<DebugRegisters>;

/// The receiving end of a free-running CPU's live snapshots, for the CPU's family.
pub enum LiveRx {
    Mos6502(watch::Receiver<Option<CpuLiveSnapshot<emma65::emulator::Registers>>>),
    Mc6809(watch::Receiver<Option<CpuLiveSnapshot<emma09::Registers>>>),
}

impl LiveRx {
    /// The most recent snapshot, if the run has published one.
    fn latest(&self) -> Option<LiveSnapshot> {
        match self {
            LiveRx::Mos6502(rx) => rx.borrow().clone().map(neutral_snapshot),
            LiveRx::Mc6809(rx) => rx.borrow().clone().map(neutral_snapshot),
        }
    }
}

/// Re-types a family's live snapshot with [`DebugRegisters`].
fn neutral_snapshot<R: Into<DebugRegisters>>(live: CpuLiveSnapshot<R>) -> LiveSnapshot {
    CpuLiveSnapshot {
        registers: live.registers.into(),
        stack: live.stack,
        stack_base: live.stack_base,
        cycles: live.cycles,
        cycles_delta: live.cycles_delta,
        elapsed: live.elapsed,
        irq_active: live.irq_active,
        firq_active: live.firq_active,
        nmi_pending: live.nmi_pending,
        cpu_stopped: live.cpu_stopped,
        cpu_waiting: live.cpu_waiting,
        exit_status: live.exit_status,
        memory_page_addr: live.memory_page_addr,
        memory_page: live.memory_page,
        watched_values: live.watched_values,
    }
}

/// Live CPU snapshot stream published by the run loop during free-run.
///
/// Set when `run_cpu` starts; cleared when the run completes. Commands that
/// need CPU state while running read from this instead of `CpuState`.
pub struct LiveSnapshotRx(pub Mutex<Option<LiveRx>>);

impl LiveSnapshotRx {
    /// The most recent snapshot of a free-running CPU, if one is running and has
    /// published one.
    pub fn latest(&self) -> Option<LiveSnapshot> {
        self.0.lock().unwrap().as_ref().and_then(LiveRx::latest)
    }
}

/// A family's CPU, as the debugger drives it: a [`Processor`] whose registers and
/// live snapshots it can show, and which goes back into a [`DebugCpu`] after a run.
pub trait DebugProcessor: Processor<Registers: Into<DebugRegisters>> + Into<DebugCpu> {
    /// Wraps this family's live snapshot receiver.
    fn live_rx(rx: watch::Receiver<Option<CpuLiveSnapshot<Self::Registers>>>) -> LiveRx;
}

impl DebugProcessor for emma65::emulator::Cpu {
    fn live_rx(rx: watch::Receiver<Option<CpuLiveSnapshot<Self::Registers>>>) -> LiveRx {
        LiveRx::Mos6502(rx)
    }
}

impl DebugProcessor for emma09::Cpu {
    fn live_rx(rx: watch::Receiver<Option<CpuLiveSnapshot<Self::Registers>>>) -> LiveRx {
        LiveRx::Mc6809(rx)
    }
}

/// How a step or run ended, in the terms the panels need.
#[derive(Default, Clone, Copy)]
struct Outcome {
    stopped: bool,
    waiting: bool,
    breakpoint_hit: bool,
    watch_triggered: bool,
    reset: bool,
}

impl Outcome {
    /// Classifies `result`; `None` (a run interrupted by Stop) is the default.
    fn of<Op>(result: Option<&StepResult<Op>>) -> Self {
        let Some(result) = result else {
            return Self::default();
        };
        Self {
            stopped: matches!(result, StepResult::Stopped),
            waiting: matches!(result, StepResult::Waiting),
            breakpoint_hit: matches!(result, StepResult::Breakpoint(_)),
            watch_triggered: matches!(
                result,
                StepResult::WatchTriggered { .. } | StepResult::WatchError { .. }
            ),
            reset: matches!(result, StepResult::Reset),
        }
    }

    /// The PC the next step must skip the breakpoint and watch check at: `pc`, when
    /// a breakpoint or watchpoint halted there.
    fn skip_pc(self, pc: u16) -> Option<u16> {
        (self.breakpoint_hit || self.watch_triggered).then_some(pc)
    }
}

/// A single disassembled line returned to the frontend.
#[derive(Clone, serde::Serialize)]
pub struct DisassembledRow {
    /// Instruction address.
    pub addr: u16,
    /// Raw bytes as hex strings, e.g. ["4C", "00", "06"].
    pub bytes: Vec<String>,
    /// Symbol names from the symbol table associated with `addr`, without trailing colons.
    pub labels: Vec<String>,
    /// Mnemonic string, e.g. "JMP".
    pub mnemonic: String,
    /// Formatted operand text, e.g. "$0600".
    pub operand: String,
    /// Comment string
    pub comment: String,
    /// False for invalid opcodes under the active variant.
    pub is_valid: bool,
}

/// Executes a single CPU instruction and returns the updated register snapshot.
///
/// Emits `debugger-halted` with the new PC after the step completes.
#[tauri::command]
pub fn step_into(
    app: AppHandle,
    cpu_state: State<CpuState>,
    changed_flags_state: State<ChangedFlagsState>,
    cpu_bus_cache: State<CpuBusCache>,
    skip_breakpoint_pc: State<SkipBreakpointPc>,
) -> Result<RegisterSnapshot, String> {
    let mut guard = cpu_state.0.lock().unwrap();
    let cpu = guard.as_mut().ok_or("CPU not ready")?;

    let flags_before = cpu.registers().flags();
    let pc = cpu.pc();
    // Skip the breakpoint/watch check only if we are halted at that PC because
    // of a prior breakpoint or watch trigger — not on every step.
    let skip_pc = skip_breakpoint_pc.0.lock().unwrap().take();
    let outcome = with_processor!(cpu, p => {
        let result = if skip_pc == Some(pc) {
            exec_step_over_breakpoint(p, pc)
        } else {
            exec_step_into(p)
        };
        Outcome::of(Some(&result))
    });
    let regs = cpu.registers();
    let changed = flags_before ^ regs.flags();

    *changed_flags_state.0.lock().unwrap() = changed;
    *cpu_bus_cache.0.lock().unwrap() = snapshot_cpu_bus(cpu);

    // Record the halted PC if a breakpoint or watch triggered, so the next
    // step_into call knows to skip the check there.
    *skip_breakpoint_pc.0.lock().unwrap() = outcome.skip_pc(regs.pc());

    let snapshot = RegisterSnapshot::new(
        &regs,
        changed,
        outcome.stopped,
        outcome.waiting,
        outcome.breakpoint_hit,
    );

    let _ = app.emit("debugger-halted", regs.pc());
    Ok(snapshot)
}

/// Returns disassembled instructions starting at `addr`, up to `count` rows.
#[tauri::command]
pub fn get_disassembly(
    addr: u16,
    count: usize,
    cpu_state: State<CpuState>,
    disasm_state: State<DisassemblerState>,
) -> Result<Vec<DisassembledRow>, String> {
    let cpu_guard = cpu_state.0.lock().unwrap();
    let cpu = cpu_guard.as_ref().ok_or("CPU not ready")?;
    let disasm_guard = disasm_state.0.lock().unwrap();
    let disasm = disasm_guard.as_ref().ok_or("Disassembler not ready")?;

    // Both families' lines have the same fields, though their mnemonic types differ.
    macro_rules! rows {
        ($lines:expr) => {
            $lines
                .into_iter()
                .map(|line| DisassembledRow {
                    addr: line.addr,
                    bytes: line.raw_bytes.iter().map(|b| format!("{b:02X}")).collect(),
                    labels: line.labels,
                    mnemonic: line.mnemonic.to_string(),
                    operand: line.operand_text,
                    comment: line.comment_text.unwrap_or_default(),
                    is_valid: line.is_valid,
                })
                .collect()
        };
    }
    let rows = match disasm {
        DebugDisassembler::Mos6502(d) => rows!(d.disassemble_range(cpu.bus(), addr, 0, count)),
        DebugDisassembler::Mc6809(d) => rows!(d.disassemble_range(cpu.bus(), addr, 0, count)),
    };

    Ok(rows)
}

/// Spawns a task that emits `debugger-running-tick` every `RUNNING_TICK_INTERVAL_MS`
/// while `RunStopperState` is set, so panels can refresh live state during any
/// free-running mode (Run, Step Over, Step Return).
///
/// Stops ticking once `RunStopperState` is cleared, which every free-run
/// command does via `finish_run` (or, for `run_cpu`, its completion task).
fn spawn_running_tick(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(RUNNING_TICK_INTERVAL_MS)).await;
            if app.state::<RunStopperState>().0.lock().unwrap().is_none() {
                break;
            }
            let _ = app.emit("debugger-running-tick", ());
        }
    });
}

/// Starts free-run execution on a dedicated OS thread.
///
/// Takes the CPU out of `CpuState` and passes it to `exec::run_from`, compiled for
/// the CPU's family. Spawns a background task that awaits the run completing, then
/// restores the CPU to `CpuState`, emits `debugger-halted` with the final PC, and
/// emits `debugger-run-stopped` with a full register snapshot.
///
/// If the run halts because of a mid-run Reset (the Reset control while the
/// CPU is free-running via Run), the run is restarted from the reset vector
/// instead of finishing (#448): the user pressed Run, so a Reset during that
/// run is naturally followed by resuming, not by landing back in the stopped
/// state. `step_over`/`step_return` don't get this treatment — those are
/// point-to-point operations, not open-ended running.
#[tauri::command]
pub fn run_cpu(
    app: AppHandle,
    cpu_state: State<CpuState>,
    skip_breakpoint_pc: State<SkipBreakpointPc>,
    mem_view_addr: State<MemoryViewAddr>,
    memory_variables_state: State<MemoryVariablesState>,
    resolved_addr_cache: State<ResolvedAddrCache>,
) -> Result<(), String> {
    let cpu = cpu_state.0.lock().unwrap().take().ok_or("CPU not ready")?;
    let skip_pc = skip_breakpoint_pc.0.lock().unwrap().take();
    let mem_view_addr = Arc::clone(&mem_view_addr.0);
    // Resolved once, here, rather than left to `get_memory_variables`'s cache:
    // guarantees the run's tracked addresses are current even if the Memory
    // Variables panel was never opened before Run was pressed. Safe to
    // compute once for the whole run — nothing can rebind a name while
    // running, since every command that touches the symbol table requires
    // the CPU to be stopped first.
    let watch_addrs = memory_variables::refresh_addr_cache_for_run(
        &memory_variables_state.0.lock().unwrap(),
        &cpu,
        &resolved_addr_cache.0,
    );
    with_processor!(cpu, p => start_run(app, p, skip_pc, mem_view_addr, watch_addrs));
    Ok(())
}

/// Runs `cpu` free until it halts, as `run_cpu` describes, then restores it.
fn start_run<P: DebugProcessor>(
    app: AppHandle,
    cpu: P,
    skip_pc: Option<u16>,
    mem_view_addr: Arc<AtomicU16>,
    watch_addrs: Vec<u16>,
) {
    let handle = spawn_run(&app, cpu, skip_pc, &mem_view_addr, &watch_addrs);
    spawn_running_tick(app.clone());

    tauri::async_runtime::spawn(async move {
        let mut handle = handle;
        loop {
            let (result, mut cpu) = handle.take_cpu_with_result().await;
            let pc = cpu.pc();
            let outcome = Outcome::of(Some(&result));
            clear_ui_interrupts_on_reset(&app, cpu.interrupts_mut(), outcome);
            if outcome.reset {
                let _ = app.emit("debugger-halted", cpu.pc());
                *app.state::<ChangedFlagsState>().0.lock().unwrap() = 0;
                *app.state::<CpuBusCache>().0.lock().unwrap() = snapshot_processor(&cpu);
                *app.state::<SkipBreakpointPc>().0.lock().unwrap() = None;
                handle = spawn_run(&app, cpu, None, &mem_view_addr, &watch_addrs);
                continue;
            }
            finish_run(&app, cpu.into(), 0, outcome, outcome.skip_pc(pc));
            break;
        }
    });
}

/// Starts `cpu` free-running (parked through WAI/STP, so a later device- or
/// UI-triggered interrupt or reset resumes it without the user pressing Run again),
/// and installs the run's stopper and live-snapshot channel.
///
/// After a mid-run Reset, `start_run` calls this again, having made the state
/// updates `reset_cpu` makes for a stopped CPU (clearing the changed flags and the
/// skip-breakpoint PC).
fn spawn_run<P: DebugProcessor>(
    app: &AppHandle,
    cpu: P,
    skip_pc: Option<u16>,
    mem_view_addr: &Arc<AtomicU16>,
    watch_addrs: &[u16],
) -> RunHandle<P> {
    let handle = exec_run_from(
        cpu,
        skip_pc,
        Arc::clone(mem_view_addr),
        watch_addrs.to_vec(),
        true,
    );
    *app.state::<RunStopperState>().0.lock().unwrap() = Some(handle.stopper());
    *app.state::<LiveSnapshotRx>().0.lock().unwrap() = Some(P::live_rx(handle.subscribe_live()));
    handle
}

/// Signals the free-running CPU thread to stop.
///
/// Non-blocking. The background task spawned by `run_cpu` handles CPU recovery
/// and emits `debugger-run-stopped` when the thread exits.
#[tauri::command]
pub fn stop_cpu(run_stopper_state: State<RunStopperState>) -> Result<(), String> {
    let guard = run_stopper_state.0.lock().unwrap();
    let stopper = guard.as_ref().ok_or("CPU is not running")?;
    stopper.stop();
    Ok(())
}

/// Executes one step treating a subroutine call (the 6502's JSR; the 6809's BSR, LBSR
/// and JSR) as atomic, then emits the result as an event.
///
/// Returns immediately; the blocking work runs on a dedicated thread. The Stop
/// button (via `stop_cpu`) can interrupt the operation mid-subroutine. Emits
/// `debugger-run-stopped` with the final snapshot when done.
#[tauri::command]
pub fn step_over(
    app: AppHandle,
    cpu_state: State<CpuState>,
    skip_breakpoint_pc: State<SkipBreakpointPc>,
    mem_view_addr: State<MemoryViewAddr>,
) -> Result<(), String> {
    // Consume the skip state; exec_step_over handles the skip internally.
    skip_breakpoint_pc.0.lock().unwrap().take();
    let cpu = cpu_state.0.lock().unwrap().take().ok_or("CPU not ready")?;
    let addr = Arc::clone(&mem_view_addr.0);
    with_processor!(cpu, p => spawn_point_run(app, p, addr, |cpu, stop, cmd, live, addr| {
        exec_step_over_subroutine(cpu, stop, cmd, live, addr)
    }));
    Ok(())
}

/// Runs until the current subroutine returns, then emits the result as an event.
///
/// Returns immediately; the blocking work runs on a dedicated thread. The Stop
/// button (via `stop_cpu`) can interrupt the operation before the return. Emits
/// `debugger-run-stopped` with the final snapshot when done.
#[tauri::command]
pub fn step_return(
    app: AppHandle,
    cpu_state: State<CpuState>,
    skip_breakpoint_pc: State<SkipBreakpointPc>,
    mem_view_addr: State<MemoryViewAddr>,
) -> Result<(), String> {
    // Consume the skip state; exec_step_return handles the skip internally.
    skip_breakpoint_pc.0.lock().unwrap().take();
    let cpu = cpu_state.0.lock().unwrap().take().ok_or("CPU not ready")?;
    let addr = Arc::clone(&mem_view_addr.0);
    with_processor!(cpu, p => spawn_point_run(app, p, addr, |cpu, stop, cmd, live, addr| {
        exec_step_return(cpu, stop, cmd, live, addr)
    }));
    Ok(())
}

/// The shape of `exec::step_over_subroutine` and `exec::step_return`.
type PointRun<P> = fn(
    &mut P,
    &watch::Receiver<bool>,
    &mut mpsc::UnboundedReceiver<InterruptCommand>,
    Option<&watch::Sender<Option<CpuLiveSnapshot<<P as Processor>::Registers>>>>,
    &Arc<AtomicU16>,
) -> Option<StepResult<<P as Processor>::Op>>;

/// Runs `run` (step over or step return) on `cpu` on a dedicated thread, with a
/// stopper and a live-snapshot channel installed, then restores the CPU and emits
/// the halt events.
fn spawn_point_run<P: DebugProcessor>(
    app: AppHandle,
    cpu: P,
    mem_view_addr: Arc<AtomicU16>,
    run: PointRun<P>,
) {
    let flags_before = flags_of(&cpu);
    let (stopper, stop_rx, mut cmd_rx) = RunStopper::channel();
    *app.state::<RunStopperState>().0.lock().unwrap() = Some(stopper);

    let (live_tx, live_rx) = watch::channel(None);
    *app.state::<LiveSnapshotRx>().0.lock().unwrap() = Some(P::live_rx(live_rx));
    spawn_running_tick(app.clone());

    std::thread::spawn(move || {
        let mut cpu = cpu;
        let result = run(
            &mut cpu,
            &stop_rx,
            &mut cmd_rx,
            Some(&live_tx),
            &mem_view_addr,
        );
        let pc = cpu.pc();
        let changed = flags_before ^ flags_of(&cpu);
        let outcome = Outcome::of(result.as_ref());
        clear_ui_interrupts_on_reset(&app, cpu.interrupts_mut(), outcome);
        finish_run(&app, cpu.into(), changed, outcome, outcome.skip_pc(pc));
    });
}

/// The flags register of a family's CPU.
fn flags_of<P: DebugProcessor>(cpu: &P) -> u32 {
    let regs: DebugRegisters = (*cpu.registers()).into();
    regs.flags()
}

/// If `outcome` is a mid-run RESET, clears any NMI/IRQ/FIRQ state the debugger UI
/// itself introduced (mirroring the explicit `reset_cpu` command) and emits
/// `debugger-cpu-reset` so the frontend can stop auto-step if active.
fn clear_ui_interrupts_on_reset(
    app: &AppHandle,
    interrupts: &mut InterruptController,
    outcome: Outcome,
) {
    if !outcome.reset {
        return;
    }
    if let Some(ui_irq_source) = *app.state::<UiIrqSourceState>().0.lock().unwrap() {
        interrupts.release_irq(ui_irq_source);
    }
    if let Some(ui_firq_source) = *app.state::<UiFirqSourceState>().0.lock().unwrap() {
        interrupts.release_irq(ui_firq_source);
    }
    interrupts.take_nmi();
    let _ = app.emit("debugger-cpu-reset", ());
}

/// Restores CPU state after a threaded run completes and emits the halt events.
///
/// Writes `changed_flags`, the CPU-bus cache, clears the run-stopper, records the
/// skip-breakpoint PC if applicable, restores the CPU into `CpuState`, then emits
/// `debugger-halted` and `debugger-run-stopped`.
fn finish_run(
    app: &AppHandle,
    cpu: DebugCpu,
    changed_flags: u32,
    outcome: Outcome,
    skip_pc: Option<u16>,
) {
    let regs = cpu.registers();
    *app.state::<ChangedFlagsState>().0.lock().unwrap() = changed_flags;
    *app.state::<CpuBusCache>().0.lock().unwrap() = snapshot_cpu_bus(&cpu);
    *app.state::<RunStopperState>().0.lock().unwrap() = None;
    *app.state::<SkipBreakpointPc>().0.lock().unwrap() = skip_pc;
    *app.state::<LiveSnapshotRx>().0.lock().unwrap() = None;
    *app.state::<CpuState>().0.lock().unwrap() = Some(cpu);

    let snapshot = RegisterSnapshot::new(
        &regs,
        changed_flags,
        outcome.stopped,
        outcome.waiting,
        outcome.breakpoint_hit,
    );
    let _ = app.emit("debugger-halted", regs.pc());
    let _ = app.emit("debugger-run-stopped", snapshot);
}
