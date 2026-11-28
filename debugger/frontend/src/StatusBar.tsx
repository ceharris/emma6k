import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { ExecState } from "./RunControlsContext";
import { useCpuDescription } from "./CpuDescriptionContext";
import { useExecutionContext } from "./ExecutionContext";
import type { RegisterSnapshot } from "./RegisterPanel";
import { stoppedLabel, waitingLabel } from "./statusLabels";
import "./styles/status-bar.scss";

/** Minimum time in ms that the Step indicator remains visible so users can perceive it. */
const STEP_INDICATOR_MIN_MS = 75;

interface CpuBusState {
  irq_active: boolean;
  /** True while any source routed to FIRQ asserts it (always false on a CPU without FIRQ). */
  firq_active: boolean;
  nmi_pending: boolean;
  cycles: number;
  effective_speed: string;
  is_running: boolean;
  cpu_stopped: boolean;
  cpu_waiting: boolean;
  /** The status the program asked to exit with through the `exit` device, once stopped for it. */
  exit_status: number | null;
}

/** Splits "1.8432 MHz" into ["1.8432", "MHz"] so the value and unit can be styled separately. */
export function splitSpeed(speed: string): [string, string] {
  const idx = speed.lastIndexOf(" ");
  return idx === -1 ? [speed, ""] : [speed.slice(0, idx), speed.slice(idx + 1)];
}

/** Formats a number with comma thousands separators. */
export function formatCycles(n: number): string {
  return n.toLocaleString();
}

/**
 * Fixed status bar docked to the bottom of the main window (issue #400),
 * replacing the old CPU/Bus dock panel so this information stays visible
 * regardless of which panels are docked. Logic ported from the retired
 * `CpuBusPanel`: same commands, same events, same Run/Stop/Step flash
 * behavior — only the layout and visual weight change.
 */
export default function StatusBar() {
  const { execState, onReset } = useExecutionContext();
  // The interrupt controls shown are the CPU's interrupt lines.
  const description = useCpuDescription();
  const family = description?.family;
  const interruptLines = description?.interrupt_lines ?? [];
  const hasNmi = interruptLines.includes("NMI");
  const hasIrq = interruptLines.includes("IRQ");
  const hasFirq = interruptLines.includes("FIRQ");
  const [cpuBus, setCpuBus] = useState<CpuBusState | null>(null);
  // Local toggle state for the IRQ and FIRQ controls, independent of the aggregate
  // cpuBus.irq_active/firq_active indicators (which reflect every source, including devices).
  const [irqAsserted, setIrqAsserted] = useState(false);
  const [firqAsserted, setFirqAsserted] = useState(false);

  // A new session starts with neither UI line asserted.
  useEffect(() => {
    setIrqAsserted(false);
    setFirqAsserted(false);
  }, [description]);

  // Display state for the Run/Stop/Step indicator. When stepping, hold the
  // "stepping" state for at least STEP_INDICATOR_MIN_MS so the transition is
  // perceptible before snapping back to "stopped".
  const [displayExecState, setDisplayExecState] = useState<ExecState>(execState);
  const stepTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const execStateRef = useRef<ExecState>(execState);
  execStateRef.current = execState;

  // Restart the step flash: show "stepping" immediately and schedule a return
  // to "stopped" after the hold duration. Called on each debugger-halted event
  // while in stepping mode so every auto-step tick produces a visible flash.
  const triggerStepFlash = useCallback(() => {
    setDisplayExecState("stepping");
    if (stepTimerRef.current !== null) clearTimeout(stepTimerRef.current);
    stepTimerRef.current = setTimeout(() => {
      stepTimerRef.current = null;
      // Only snap back to stopped if we're not still in a stepping/running state.
      if (execStateRef.current !== "running") {
        setDisplayExecState("stopped");
      }
    }, STEP_INDICATOR_MIN_MS);
  }, []);

  useEffect(() => {
    if (execState === "stepping") {
      // Each transition into "stepping" triggers a flash. During auto-step the
      // debugger-halted listener (below) drives a fresh flash on every tick.
      triggerStepFlash();
    } else if (execState === "running") {
      // Cancel any pending step hold and go to "running" immediately.
      if (stepTimerRef.current !== null) {
        clearTimeout(stepTimerRef.current);
        stepTimerRef.current = null;
      }
      setDisplayExecState("running");
    } else {
      // Transition to stopped: if a step-hold timer is still pending, let it
      // expire naturally so the blue flash remains visible for the full hold duration.
      if (stepTimerRef.current === null) {
        setDisplayExecState("stopped");
      }
    }
  }, [execState, triggerStepFlash]);

  // Cancel any pending step-hold timer on unmount.
  useEffect(() => {
    return () => {
      if (stepTimerRef.current !== null) clearTimeout(stepTimerRef.current);
    };
  }, []);

  const fetchCpuBus = useCallback(async () => {
    try {
      const result = await invoke<CpuBusState>("get_cpu_bus_state");
      setCpuBus(result);
    } catch (e) {
      console.error("get_cpu_bus_state failed:", e);
    }
  }, []);

  useEffect(() => {
    fetchCpuBus();
  }, [fetchCpuBus]);

  // Re-fetch on halt/run-stopped events; also re-trigger the step flash on
  // each halt so auto-step produces a visible indicator pulse per tick.
  useEffect(() => {
    const unlistenHalted = listen("debugger-halted", () => {
      fetchCpuBus();
      if (execStateRef.current === "stepping") {
        triggerStepFlash();
      }
    });
    const unlistenRunStopped = listen("debugger-run-stopped", () => {
      fetchCpuBus();
    });
    const unlistenTick = listen("debugger-running-tick", () => {
      fetchCpuBus();
    });
    return () => {
      unlistenHalted.then((f) => f());
      unlistenRunStopped.then((f) => f());
      unlistenTick.then((f) => f());
    };
  }, [fetchCpuBus, triggerStepFlash]);

  const handleReset = useCallback(async () => {
    try {
      // While free-running, reset_cpu returns null: the run halts asynchronously
      // and the register update instead arrives via the debugger-run-stopped event.
      const snap = await invoke<RegisterSnapshot | null>("reset_cpu");
      setIrqAsserted(false);
      setFirqAsserted(false);
      if (snap !== null) {
        onReset(snap);
      }
    } catch (e) {
      console.error("reset_cpu failed:", e);
    }
  }, [onReset]);

  const handleTriggerNmi = useCallback(async () => {
    try {
      const result = await invoke<CpuBusState>("trigger_nmi");
      setCpuBus(result);
    } catch (e) {
      console.error("trigger_nmi failed:", e);
    }
  }, []);

  /**
   * Toggles one of the UI's own interrupt lines (`irq` or `firq`): assert or release
   * while halted or stepping. While free-running (or step-over/step-return in progress),
   * the backend treats assert as a one-shot pulse that auto-releases once the CPU
   * services it (see #261) — so there's no sticky "asserted" state to track then, and
   * the control behaves like the momentary NMI trigger instead of a toggle.
   */
  const toggleLine = useCallback(
    async (line: "irq" | "firq", asserted: boolean, setAsserted: (v: boolean) => void) => {
      try {
        if (cpuBus?.is_running) {
          setCpuBus(await invoke<CpuBusState>(`assert_${line}`));
          return;
        }
        setCpuBus(await invoke<CpuBusState>(asserted ? `release_${line}` : `assert_${line}`));
        setAsserted(!asserted);
      } catch (e) {
        console.error(`assert_${line}/release_${line} failed:`, e);
      }
    },
    [cpuBus?.is_running],
  );
  const handleToggleIrq = useCallback(
    () => toggleLine("irq", irqAsserted, setIrqAsserted),
    [toggleLine, irqAsserted],
  );
  const handleToggleFirq = useCallback(
    () => toggleLine("firq", firqAsserted, setFirqAsserted),
    [toggleLine, firqAsserted],
  );

  // Determine Run/Stop/Step/STP/WAI indicator label and color class.
  // STP and WAI override the normal "Stop" state when the CPU has halted
  // due to a STP or WAI instruction. They also override "Run": with
  // park_on_stall enabled, Run keeps the CPU thread alive (polling for a
  // device- or UI-triggered interrupt/reset) instead of stopping when the
  // CPU executes STP or WAI, so cpuBus.cpu_stopped/cpu_waiting can go true
  // while displayExecState is still "running".
  let runStopLabel: string;
  let runStopClass: string;
  if (displayExecState === "running") {
    if (cpuBus?.cpu_stopped) {
      runStopLabel = stoppedLabel(cpuBus.exit_status, family);
      runStopClass = "indicator-stp";
    } else if (cpuBus?.cpu_waiting) {
      runStopLabel = waitingLabel(family);
      runStopClass = "indicator-wai";
    } else {
      runStopLabel = "Run";
      runStopClass = "indicator-run";
    }
  } else if (displayExecState === "stepping") {
    runStopLabel = "Stop";
    runStopClass = "indicator-step";
  } else if (cpuBus?.cpu_stopped) {
    runStopLabel = stoppedLabel(cpuBus.exit_status, family);
    runStopClass = "indicator-stp";
  } else if (cpuBus?.cpu_waiting) {
    runStopLabel = waitingLabel(family);
    runStopClass = "indicator-wai";
  } else {
    runStopLabel = "Stop";
    runStopClass = "indicator-stop";
  }

  const [speedValue, speedUnit] = splitSpeed(cpuBus?.effective_speed ?? "0 MHz");

  return (
    <footer className="status-bar">
      {/* Unused space is reserved for future use. */}
      <div className="status-bar-spacer" />
      <div className="status-bar-cell status-bar-cycles">
        <span className="cycles-value">{cpuBus !== null ? formatCycles(cpuBus.cycles) : "—"}</span>
        <span className="status-bar-label">cycles</span>
      </div>
      <div className="status-bar-cell status-bar-speed">
        <span className="cycles-value">{speedValue}</span>
        <span className="status-bar-label">{speedUnit}</span>
      </div>
      {hasNmi && (
        <button
          className="status-bar-cell status-bar-toggle"
          onClick={handleTriggerNmi}
          title="Trigger NMI"
        >
          <span
            className={`indicator ${cpuBus?.nmi_pending ? "indicator-nmi-active" : "indicator-idle"}`}
          >
            ●
          </span>
          <span className="status-bar-label">NMI</span>
        </button>
      )}
      {hasIrq && (
        <button
          className={`status-bar-cell status-bar-toggle${irqAsserted ? " active" : ""}`}
          onClick={handleToggleIrq}
          title={cpuBus?.is_running ? "Trigger IRQ (one-shot while running)" : "Assert/Release IRQ"}
        >
          <span
            className={`indicator ${cpuBus?.irq_active ? "indicator-irq-active" : "indicator-idle"}`}
          >
            ●
          </span>
          <span className="status-bar-label">IRQ</span>
        </button>
      )}
      {hasFirq && (
        <button
          className={`status-bar-cell status-bar-toggle${firqAsserted ? " active" : ""}`}
          onClick={handleToggleFirq}
          title={
            cpuBus?.is_running ? "Trigger FIRQ (one-shot while running)" : "Assert/Release FIRQ"
          }
        >
          <span
            className={`indicator ${cpuBus?.firq_active ? "indicator-irq-active" : "indicator-idle"}`}
          >
            ●
          </span>
          <span className="status-bar-label">FIRQ</span>
        </button>
      )}
      <div className="status-bar-cell status-bar-run">
        <span className={`indicator ${runStopClass}`}>●</span>
        <span className="status-bar-label status-bar-run-label">{runStopLabel}</span>
      </div>
      <button
        className="status-bar-cell status-bar-toggle status-bar-reset"
        onClick={handleReset}
        title="Reset CPU"
      >
        Reset
      </button>
    </footer>
  );
}
