/** The Stack panel's data: the backend's stack snapshot, and the word-pair rows built from it. */

/** The backend's stack snapshot (`debugger/src-tauri/src/stack.rs`). Addresses are full bus addresses. */
export interface StackSnapshot {
  /** Where the stack pointer register points. */
  pointer: number;
  /** The address of the byte most recently pushed. */
  top: number;
  /** The address of `bytes[0]`. */
  base: number;
  /** The CPU's stack window. */
  bytes: number[];
}

/** Number of word-pair rows visible in the panel. */
const VISIBLE_PAIRS = 8;

export interface StackRow {
  /** Bus address of the lo byte of this pair. */
  addr: number;
  /** Lo byte value, or null when the slot isn't on the stack (or isn't in the window). */
  lo: number | null;
  /** Hi byte value, or null when the slot isn't in the window. */
  hi: number | null;
  /** True for the first row (the pair containing the most recently pushed byte). */
  isActive: boolean;
}

/**
 * Builds exactly VISIBLE_PAIRS word-pair rows, starting with the pair that holds `top`, the
 * most recently pushed byte, and proceeding toward higher addresses.
 *
 * `alignOdd` shifts pair boundaries by one byte: even pairs start at an even address, odd
 * pairs at an odd one. When `top` is the pair's hi byte, the lo byte below it isn't on the
 * stack, and shows as the `--` placeholder. (On the 6502 that's the slot S points to, the
 * next one a push writes.)
 *
 * A `pageFixed` stack (the 6502's page 1) is the whole window, and the rows wrap around it:
 * with S at $FF, the odd pair holding $0100 starts at $01FF. Otherwise addresses run
 * straight through the 16-bit space, and any byte outside the window is a placeholder too.
 */
export function buildRows(snap: StackSnapshot, pageFixed: boolean, alignOdd: boolean): StackRow[] {
  const { top, base, bytes } = snap;
  const len = bytes.length;
  // Steps `delta` bytes from `addr`: around the window if it's the whole stack, else
  // through the address space.
  const step = (addr: number, delta: number) =>
    pageFixed && len > 0
      ? base + ((((addr - base + delta) % len) + len) % len)
      : (addr + delta) & 0xffff;
  const byteAt = (addr: number) => {
    const offset = (addr - base) & 0xffff;
    return offset < len ? bytes[offset] : null;
  };

  const topIsLo = (top % 2 === 1) === alignOdd;
  const first = topIsLo ? top : step(top, -1);

  const rows: StackRow[] = [];
  for (let i = 0; i < VISIBLE_PAIRS; i++) {
    const addr = step(first, i * 2);
    rows.push({
      addr,
      lo: i === 0 && !topIsLo ? null : byteAt(addr),
      hi: byteAt(step(addr, 1)),
      isActive: i === 0,
    });
  }
  return rows;
}
