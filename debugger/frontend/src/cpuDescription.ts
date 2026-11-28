/**
 * The shape of the backend's CPU description (`debugger/src-tauri/src/cpu.rs`), and
 * helpers for reading its flags. `CpuDescriptionContext` holds the session's description.
 */

/** What a register is for, which decides where the panels show it. */
export type RegisterRole = "data" | "index" | "stack" | "pc" | "flags";

/** One register, in the order the Register panel lists it. */
export interface RegisterDescription {
  /** The register's name: its key in a snapshot's `values`, and the name `set_register` takes. */
  name: string;
  width_bits: number;
  role: RegisterRole;
}

/** One bit of the flags register. */
export interface FlagDescription {
  /** The character shown when the flag is set; `-` for an unused bit. */
  label: string;
  /** The bit's position in the flags register, 0 for the least significant. */
  bit: number;
  name: string;
}

/** The order of a word's bytes in memory. */
export type ByteOrder = "little" | "big";

/** What the session's CPU looks like, as the backend describes it. */
export interface CpuDescription {
  family: string;
  variant: string;
  registers: RegisterDescription[];
  /** The flags register's bits, high bit first. */
  flags: FlagDescription[];
  /** The name of the register `flags` describes. */
  flags_register: string;
  interrupt_lines: string[];
  stack: { pointer: string; page_fixed: boolean };
  byte_order: ByteOrder;
  max_instruction_len: number;
  assembler: boolean;
}

/** True for a flag bit that has no meaning, which is never marked changed or toggled. */
export function isUnusedFlag(flag: FlagDescription): boolean {
  return flag.label === "-";
}

/** The bitmask of `flag` within the flags register. */
export function flagMask(flag: FlagDescription): number {
  return 1 << flag.bit;
}

/**
 * Splits `value` into `sizeBytes` bytes for `write_memory`, in address order: least
 * significant first for a `little` CPU (the 6502), most significant first for a `big` one.
 */
export function toBytes(value: number, sizeBytes: number, order: ByteOrder): number[] {
  const bytes: number[] = [];
  for (let i = 0; i < sizeBytes; i++) {
    bytes.push((value >>> (i * 8)) & 0xff);
  }
  return order === "big" ? bytes.reverse() : bytes;
}

/** The width, in characters, of an instruction's bytes shown as space-separated hex pairs:
 *  8 for the 6502's three-byte instructions, 14 for the 6809's five. */
export function instructionBytesWidth(maxInstructionLen: number): number {
  return Math.max(maxInstructionLen, 1) * 3 - 1;
}

/** Formats an instruction's bytes as space-separated hex pairs, padded to the width of
 *  the CPU's longest instruction, so the columns after them line up. */
export function formatInstructionBytes(bytes: string[], maxInstructionLen: number): string {
  return bytes
    .map((b) => b.padStart(2, "0"))
    .join(" ")
    .padEnd(instructionBytesWidth(maxInstructionLen), " ");
}
