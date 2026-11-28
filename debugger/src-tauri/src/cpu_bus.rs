//! CPU/bus panel: IRQ/NMI controls, reset, and cached bus-signal snapshot.

use std::sync::Mutex;

use emma65::emulator::bus::DeviceIdAllocator;
use emma65::emulator::{BusConfigError, IrqSource, Processor};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::CpuState;
use crate::cpu::{DebugCpu, with_processor};
use crate::disassembly::{LiveSnapshotRx, RunStopperState, SkipBreakpointPc};
use crate::registers::{ChangedFlagsState, RegisterSnapshot};

/// IRQ source identifying the debugger UI's own IRQ toggle control.
///
/// Allocated from the session's `DeviceIdAllocator` after all configured
/// devices, so it never collides with a real device's `IrqSource`. `None`
/// until the emulator session finishes loading.
pub struct UiIrqSourceState(pub Mutex<Option<IrqSource>>);

/// IRQ source identifying the debugger UI's own FIRQ toggle control: an interrupt
/// source the session routes to FIRQ, allocated the same way as the IRQ control's.
/// `None` until a session loads, and for a CPU with no FIRQ line (the 6502).
pub struct UiFirqSourceState(pub Mutex<Option<IrqSource>>);

/// For a CPU with a FIRQ line, allocates the FIRQ control's interrupt source
/// (`DEBUGGER_FIRQ`) and adds it to the interrupt controller's FIRQ mask, so asserting
/// it drives FIRQ, the way a device configured with `interrupt = "firq"` does. Returns
/// `None` for a CPU without one.
pub fn install_ui_firq_source(
    cpu: &mut DebugCpu,
    allocator: &mut DeviceIdAllocator,
) -> Result<Option<IrqSource>, BusConfigError> {
    if !cpu.description().interrupt_lines.contains(&"FIRQ") {
        return Ok(None);
    }
    let source = IrqSource::from(allocator.for_irq(crate::DEBUGGER_FIRQ)?);
    let interrupts = cpu.interrupts_mut();
    interrupts.set_firq_mask(interrupts.firq_mask() | (1u64 << crate::DEBUGGER_FIRQ));
    Ok(Some(source))
}

/// Placeholder `effective_speed` shown while the CPU isn't free-running (no
/// live snapshot to compute a rate from).
pub const EFFECTIVE_SPEED_UNKNOWN: &str = "0 MHz";

/// Cached CPU/bus state (IRQ, NMI, cycle count) for use when the CPU is free-running.
///
/// Updated every time the CPU is available: after each step, reset, or run completion.
pub struct CpuBusCache(pub Mutex<CpuBusSnapshot>);

/// Snapshot of CPU/bus signals and cycle counter.
#[derive(Clone, serde::Serialize)]
pub struct CpuBusSnapshot {
    /// True if any device is currently asserting IRQ.
    pub irq_active: bool,
    /// True if any source routed to FIRQ is currently asserting it.
    pub firq_active: bool,
    /// True if an NMI is pending (latched but not yet serviced).
    pub nmi_pending: bool,
    /// Total CPU cycles executed since the last reset.
    pub cycles: u64,
    /// Effective speed of the CPU
    pub effective_speed: String,
    /// True when the CPU executed STP and is halted until reset.
    pub cpu_stopped: bool,
    /// True when the CPU executed WAI and is waiting for an interrupt.
    pub cpu_waiting: bool,
    /// The status the program asked to exit with through the `exit` device,
    /// once the CPU has stopped for it.
    pub exit_status: Option<u8>,
}

/// Combined CPU/bus state returned by `get_cpu_bus_state`.
#[derive(Clone, serde::Serialize)]
pub struct CpuBusState {
    /// True if any device is currently asserting IRQ.
    pub irq_active: bool,
    /// True if any source routed to FIRQ is currently asserting it.
    pub firq_active: bool,
    /// True if an NMI is pending (latched but not yet serviced).
    pub nmi_pending: bool,
    /// Total CPU cycles executed since the last reset.
    pub cycles: u64,
    /// Effective speed of the CPU
    pub effective_speed: String,
    /// True while the CPU is free-running (run_cpu, step_over, or step_return in progress).
    pub is_running: bool,
    /// True when the CPU executed STP and is halted until reset.
    pub cpu_stopped: bool,
    /// True when the CPU executed WAI and is waiting for an interrupt.
    pub cpu_waiting: bool,
    /// The status the program asked to exit with through the `exit` device,
    /// once the CPU has stopped for it.
    pub exit_status: Option<u8>,
}

/// Resets the CPU (reads reset vector, reinitializes registers) and returns the
/// post-reset register snapshot, or `None` if the CPU was free-running.
///
/// While free-running (Run, Step Over, or Step Return in progress), this instead
/// signals a RESET into the running CPU via `RunStopper::trigger_reset` and
/// returns immediately; the run halts on its own and the free-run completion
/// handler emits `debugger-halted`/`debugger-run-stopped`/`debugger-cpu-reset`
/// with the real post-reset snapshot.
///
/// Otherwise resets `ChangedFlagsState` to 0 and emits `debugger-halted` with
/// the new PC, then emits `debugger-cpu-reset` so the frontend can stop
/// auto-step if active.
#[tauri::command]
pub fn reset_cpu(
    app: AppHandle,
    cpu_state: State<CpuState>,
    changed_flags_state: State<ChangedFlagsState>,
    cpu_bus_cache: State<CpuBusCache>,
    skip_breakpoint_pc: State<SkipBreakpointPc>,
    ui_irq_source: State<UiIrqSourceState>,
    run_stopper_state: State<RunStopperState>,
) -> Result<Option<RegisterSnapshot>, String> {
    if let Some(stopper) = run_stopper_state.0.lock().unwrap().as_ref() {
        stopper.trigger_reset();
        return Ok(None);
    }

    let ui_irq_source = ui_irq_source.0.lock().unwrap().ok_or("CPU not ready")?;
    let mut guard = cpu_state.0.lock().unwrap();
    let cpu = guard.as_mut().ok_or("CPU not ready")?;

    cpu.reset().map_err(|e| e.to_string())?;
    // Clear any NMI/IRQ/FIRQ state the debugger UI itself introduced, so the
    // interrupt controls stay in sync with a freshly reset CPU.
    cpu.interrupts_mut().release_irq(ui_irq_source);
    // Read through `app` rather than as another `State` parameter, which would trip
    // clippy's too-many-arguments lint.
    if let Some(ui_firq_source) = *app.state::<UiFirqSourceState>().0.lock().unwrap() {
        cpu.interrupts_mut().release_irq(ui_firq_source);
    }
    cpu.interrupts_mut().take_nmi();
    let regs = cpu.registers();

    *changed_flags_state.0.lock().unwrap() = 0;
    *cpu_bus_cache.0.lock().unwrap() = snapshot_cpu_bus(cpu);
    *skip_breakpoint_pc.0.lock().unwrap() = None;

    let snapshot = RegisterSnapshot::new(&regs, 0, false, false, false);

    let _ = app.emit("debugger-halted", regs.pc());
    let _ = app.emit("debugger-cpu-reset", ());
    Ok(Some(snapshot))
}

/// Latches a pending NMI.
///
/// While free-running, signals the running CPU via `RunStopper::trigger_nmi`
/// instead of locking `CpuState`, and returns the best-known current state.
#[tauri::command]
pub fn trigger_nmi(
    cpu_state: State<CpuState>,
    cpu_bus_cache: State<CpuBusCache>,
    run_stopper_state: State<RunStopperState>,
    live_snapshot_rx: State<LiveSnapshotRx>,
) -> Result<CpuBusState, String> {
    // Scoped so the lock is released before `current_cpu_bus_state` re-locks
    // `run_stopper_state` below; `if let` on a `.lock()` temporary would
    // otherwise hold the guard for the whole block and self-deadlock.
    let is_running = {
        let guard = run_stopper_state.0.lock().unwrap();
        if let Some(stopper) = guard.as_ref() {
            stopper.trigger_nmi();
            true
        } else {
            false
        }
    };
    if is_running {
        return Ok(current_cpu_bus_state(
            &cpu_bus_cache,
            &run_stopper_state,
            &live_snapshot_rx,
        ));
    }
    let mut guard = cpu_state.0.lock().unwrap();
    let cpu = guard.as_mut().ok_or("CPU not ready")?;
    cpu.interrupts_mut().signal_nmi();
    Ok(refresh_cpu_bus_cache(cpu, &cpu_bus_cache))
}

/// Asserts the IRQ line from the debugger UI's own IRQ source.
///
/// While stopped or single/auto-stepping, this is a manual, level-triggered assert:
/// the line stays up until `release_irq` is called, matching the sustained-signal
/// semantics a real device would produce.
///
/// While free-running (Run, Step Over, or Step Return in progress), asserting IRQ
/// instead uses `RunStopper::assert_irq_pulse`: the line is a one-shot pulse that
/// auto-releases as soon as the CPU services the interrupt. Without this, a level
/// left asserted during a free run would re-enter the ISR on every instruction after
/// `RTI` (once `I` clears again) until the user managed to click release — see #261.
#[tauri::command]
pub fn assert_irq(
    cpu_state: State<CpuState>,
    cpu_bus_cache: State<CpuBusCache>,
    ui_irq_source: State<UiIrqSourceState>,
    run_stopper_state: State<RunStopperState>,
    live_snapshot_rx: State<LiveSnapshotRx>,
) -> Result<CpuBusState, String> {
    let source = ui_irq_source.0.lock().unwrap().ok_or("CPU not ready")?;
    drive_ui_line(
        source,
        true,
        &cpu_state,
        &cpu_bus_cache,
        &run_stopper_state,
        &live_snapshot_rx,
    )
}

/// Releases the IRQ line from the debugger UI's own IRQ source.
///
/// While free-running, signals the running CPU via `RunStopper::release_irq`
/// instead of locking `CpuState`, and returns the best-known current state. Not
/// normally needed while free-running, since `assert_irq` pulses in that mode and
/// auto-releases on its own — but harmless to call, e.g. to cancel a pulse that
/// hasn't yet been serviced because `I` is still set.
#[tauri::command]
pub fn release_irq(
    cpu_state: State<CpuState>,
    cpu_bus_cache: State<CpuBusCache>,
    ui_irq_source: State<UiIrqSourceState>,
    run_stopper_state: State<RunStopperState>,
    live_snapshot_rx: State<LiveSnapshotRx>,
) -> Result<CpuBusState, String> {
    let source = ui_irq_source.0.lock().unwrap().ok_or("CPU not ready")?;
    drive_ui_line(
        source,
        false,
        &cpu_state,
        &cpu_bus_cache,
        &run_stopper_state,
        &live_snapshot_rx,
    )
}

/// Asserts FIRQ from the debugger UI's own FIRQ source, the way `assert_irq` asserts
/// IRQ: level-triggered while halted or stepping, a one-shot pulse while
/// free-running. Fails for a CPU with no FIRQ line.
#[tauri::command]
pub fn assert_firq(
    cpu_state: State<CpuState>,
    cpu_bus_cache: State<CpuBusCache>,
    ui_firq_source: State<UiFirqSourceState>,
    run_stopper_state: State<RunStopperState>,
    live_snapshot_rx: State<LiveSnapshotRx>,
) -> Result<CpuBusState, String> {
    let source = ui_firq_source.0.lock().unwrap().ok_or(NO_FIRQ)?;
    drive_ui_line(
        source,
        true,
        &cpu_state,
        &cpu_bus_cache,
        &run_stopper_state,
        &live_snapshot_rx,
    )
}

/// Releases FIRQ from the debugger UI's own FIRQ source, the way `release_irq`
/// releases IRQ. Fails for a CPU with no FIRQ line.
#[tauri::command]
pub fn release_firq(
    cpu_state: State<CpuState>,
    cpu_bus_cache: State<CpuBusCache>,
    ui_firq_source: State<UiFirqSourceState>,
    run_stopper_state: State<RunStopperState>,
    live_snapshot_rx: State<LiveSnapshotRx>,
) -> Result<CpuBusState, String> {
    let source = ui_firq_source.0.lock().unwrap().ok_or(NO_FIRQ)?;
    drive_ui_line(
        source,
        false,
        &cpu_state,
        &cpu_bus_cache,
        &run_stopper_state,
        &live_snapshot_rx,
    )
}

/// The error for a FIRQ command when the session's CPU has no FIRQ line.
const NO_FIRQ: &str = "The CPU has no FIRQ line";

/// Asserts (`assert`) or releases one of the UI's interrupt sources. The interrupt
/// controller's FIRQ mask decides which line the source drives.
fn drive_ui_line(
    source: IrqSource,
    assert: bool,
    cpu_state: &State<CpuState>,
    cpu_bus_cache: &State<CpuBusCache>,
    run_stopper_state: &State<RunStopperState>,
    live_snapshot_rx: &State<LiveSnapshotRx>,
) -> Result<CpuBusState, String> {
    // Scoped so the lock is released before `current_cpu_bus_state` re-locks
    // `run_stopper_state` below; `if let` on a `.lock()` temporary would
    // otherwise hold the guard for the whole block and self-deadlock.
    let is_running = {
        let guard = run_stopper_state.0.lock().unwrap();
        if let Some(stopper) = guard.as_ref() {
            if assert {
                stopper.assert_irq_pulse(source);
            } else {
                stopper.release_irq(source);
            }
            true
        } else {
            false
        }
    };
    if is_running {
        return Ok(current_cpu_bus_state(
            cpu_bus_cache,
            run_stopper_state,
            live_snapshot_rx,
        ));
    }
    let mut guard = cpu_state.0.lock().unwrap();
    let cpu = guard.as_mut().ok_or("CPU not ready")?;
    if assert {
        cpu.interrupts_mut().assert_irq(source);
    } else {
        cpu.interrupts_mut().release_irq(source);
    }
    Ok(refresh_cpu_bus_cache(cpu, cpu_bus_cache))
}

/// Refreshes `CpuBusCache` from `cpu` and returns the corresponding `CpuBusState`.
///
/// `is_running` is always `false` here: this is only called from the direct-lock
/// (not free-running) path of `trigger_nmi`/`assert_irq`/`release_irq`.
fn refresh_cpu_bus_cache(cpu: &DebugCpu, cpu_bus_cache: &State<CpuBusCache>) -> CpuBusState {
    let snap = snapshot_cpu_bus(cpu);
    *cpu_bus_cache.0.lock().unwrap() = snap.clone();
    CpuBusState {
        irq_active: snap.irq_active,
        firq_active: snap.firq_active,
        nmi_pending: snap.nmi_pending,
        cycles: snap.cycles,
        effective_speed: snap.effective_speed,
        is_running: false,
        cpu_stopped: snap.cpu_stopped,
        cpu_waiting: snap.cpu_waiting,
        exit_status: snap.exit_status,
    }
}

/// Snapshots the interrupt controller state and cycle count from a halted CPU.
pub fn snapshot_cpu_bus(cpu: &DebugCpu) -> CpuBusSnapshot {
    with_processor!(cpu, p => snapshot_processor(p))
}

/// Snapshots the interrupt controller state and cycle count from either family's CPU.
pub fn snapshot_processor<P: Processor>(cpu: &P) -> CpuBusSnapshot {
    CpuBusSnapshot {
        irq_active: cpu.interrupts().irq_active(),
        firq_active: cpu.interrupts().firq_active(),
        nmi_pending: cpu.interrupts().nmi_pending(),
        cycles: cpu.cycles(),
        effective_speed: EFFECTIVE_SPEED_UNKNOWN.to_string(),
        cpu_stopped: cpu.is_stopped(),
        cpu_waiting: cpu.is_waiting(),
        exit_status: cpu.machine().exit_status(),
    }
}

/// Computes the current CPU/bus signals and cycle count, plus whether the CPU is free-running.
///
/// All signals come from the cache updated after each step or run completion, except while
/// free-running (Run, Step Over, Step Return), when they're read from the live snapshot channel
/// so the display updates at the tick rate rather than only at halt.
fn current_cpu_bus_state(
    cpu_bus_cache: &State<CpuBusCache>,
    run_stopper_state: &State<RunStopperState>,
    live_snapshot_rx: &State<LiveSnapshotRx>,
) -> CpuBusState {
    let snap = cpu_bus_cache.0.lock().unwrap().clone();
    let is_running = run_stopper_state.0.lock().unwrap().is_some();
    let live = if is_running {
        live_snapshot_rx.latest()
    } else {
        None
    };

    let effective_speed = live.as_ref().map(|snap| {
        let rate = snap.cycles_delta as f64 / snap.elapsed.as_secs_f64() / 1e6;
        format!("{rate:.4} MHz")
    });

    CpuBusState {
        irq_active: live.as_ref().map_or(snap.irq_active, |s| s.irq_active),
        firq_active: live.as_ref().map_or(snap.firq_active, |s| s.firq_active),
        nmi_pending: live.as_ref().map_or(snap.nmi_pending, |s| s.nmi_pending),
        cycles: live.as_ref().map_or(snap.cycles, |s| s.cycles),
        effective_speed: effective_speed.unwrap_or(snap.effective_speed),
        is_running,
        cpu_stopped: live.as_ref().map_or(snap.cpu_stopped, |s| s.cpu_stopped),
        cpu_waiting: live.as_ref().map_or(snap.cpu_waiting, |s| s.cpu_waiting),
        exit_status: live.as_ref().map_or(snap.exit_status, |s| s.exit_status),
    }
}

/// Returns the current CPU/bus signals and cycle count, plus whether the CPU is free-running.
#[tauri::command]
pub fn get_cpu_bus_state(
    cpu_bus_cache: State<CpuBusCache>,
    run_stopper_state: State<RunStopperState>,
    live_snapshot_rx: State<LiveSnapshotRx>,
) -> CpuBusState {
    current_cpu_bus_state(&cpu_bus_cache, &run_stopper_state, &live_snapshot_rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use emma65::emulator::bus::{AddressRange, Bus};
    use emma65::emulator::{Cpu, CpuVariant};

    fn ram() -> Bus {
        Bus::config()
            .ram_with_fill(AddressRange::new(0x0000, 0xffff), 0)
            .unwrap()
            .build()
    }

    #[test]
    fn the_6809_firq_control_drives_firq_not_irq() {
        let mut cpu = DebugCpu::from(emma09::Cpu::builder().bus(ram()).build().unwrap());
        let mut allocator = DeviceIdAllocator::new();
        let source = install_ui_firq_source(&mut cpu, &mut allocator)
            .unwrap()
            .expect("the 6809 has FIRQ");

        cpu.interrupts_mut().assert_irq(source);
        let snap = snapshot_cpu_bus(&cpu);
        assert!(snap.firq_active);
        assert!(!snap.irq_active);

        cpu.interrupts_mut().release_irq(source);
        assert!(!snapshot_cpu_bus(&cpu).firq_active);
    }

    #[test]
    fn the_6502_has_no_firq_control() {
        let mut cpu = DebugCpu::from(
            Cpu::builder(CpuVariant::Wdc65C02)
                .bus(ram())
                .build()
                .unwrap(),
        );
        let mut allocator = DeviceIdAllocator::new();
        assert_eq!(
            install_ui_firq_source(&mut cpu, &mut allocator).unwrap(),
            None
        );
        assert_eq!(cpu.interrupts().firq_mask(), 0);
    }
}
