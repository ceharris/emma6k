/** The status bar's run-state labels, which name the CPU's own halting instructions. */

/**
 * The stopped CPU's indicator label: the exit status if the program exited, else STP on
 * the 6502 (the 6809 has no STP, so it stops only for an exit).
 */
export function stoppedLabel(exitStatus: number | null | undefined, family = "6502"): string {
  if (exitStatus != null) return `Exit ${exitStatus}`;
  return family === "6502" ? "STP Executed" : "Stopped";
}

/** The waiting CPU's indicator label: the 6502's WAI, or the 6809's CWAI or SYNC. */
export function waitingLabel(family = "6502"): string {
  return family === "6502" ? "WAI Executed" : "Waiting";
}
