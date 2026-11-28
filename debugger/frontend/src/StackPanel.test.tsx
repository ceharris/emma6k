import { render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { CpuDescriptionProvider } from "./CpuDescriptionContext";
import StackPanel from "./StackPanel";
import { buildRows, StackSnapshot } from "./stackRows";
import { DESCRIPTION_6502 } from "./test/cpuFixtures";
import { invoke, resetTauriMocks } from "./test/tauriMock";

/** A 6502 stack: page 1, each byte holding its own page offset, S at `s`. */
function stack6502(s: number): StackSnapshot {
  return {
    pointer: 0x0100 | s,
    top: 0x0100 | ((s + 1) & 0xff),
    base: 0x0100,
    bytes: Array.from({ length: 256 }, (_, i) => i),
  };
}

describe("buildRows on the 6502's page-fixed stack", () => {
  it("starts with the even pair holding the top byte, the slot at S a placeholder", () => {
    const rows = buildRows(stack6502(0xfc), true, false);
    expect(rows).toHaveLength(8);
    expect(rows[0]).toEqual({ addr: 0x01fc, lo: null, hi: 0xfd, isActive: true });
    expect(rows[1]).toEqual({ addr: 0x01fe, lo: 0xfe, hi: 0xff, isActive: false });
    // Then around the page, not into page 2.
    expect(rows[2]).toEqual({ addr: 0x0100, lo: 0x00, hi: 0x01, isActive: false });
  });

  it("starts the pair at the top byte when it's the pair's lo byte", () => {
    const rows = buildRows(stack6502(0xfb), true, false);
    expect(rows[0]).toEqual({ addr: 0x01fc, lo: 0xfc, hi: 0xfd, isActive: true });
  });

  it("wraps an odd pair across the end of the page", () => {
    // S = $FF: the top byte is $0100, so the odd pair holding it starts at $01FF.
    const rows = buildRows(stack6502(0xff), true, true);
    expect(rows[0]).toEqual({ addr: 0x01ff, lo: null, hi: 0x00, isActive: true });
    expect(rows[1].addr).toBe(0x0101);
  });

  it("wraps an even pair across the end of the page", () => {
    // S = $FD: the top byte is $01FE, and the pair after $01FE/$01FF is $0100/$0101.
    const rows = buildRows(stack6502(0xfd), true, false);
    expect(rows.map((r) => r.addr)).toEqual([
      0x01fe, 0x0100, 0x0102, 0x0104, 0x0106, 0x0108, 0x010a, 0x010c,
    ]);
  });
});

describe("buildRows on a stack that isn't page-fixed", () => {
  // A window like the 6809's: S, the top byte, 32 bytes above the window's base.
  const snap: StackSnapshot = {
    pointer: 0x7fe1,
    top: 0x7fe1,
    base: 0x7fc1,
    bytes: Array.from({ length: 40 }, (_, i) => i),
  };

  it("uses full addresses and doesn't wrap at a page boundary", () => {
    const rows = buildRows(snap, false, false);
    expect(rows[0]).toEqual({ addr: 0x7fe0, lo: null, hi: 32, isActive: true });
    expect(rows[1]).toEqual({ addr: 0x7fe2, lo: 33, hi: 34, isActive: false });
  });

  it("shows bytes past the window's end as placeholders", () => {
    const rows = buildRows(snap, false, true);
    expect(rows[0]).toEqual({ addr: 0x7fe1, lo: 32, hi: 33, isActive: true });
    // The window ends at $7FE8, so $7FE9 onward is outside it.
    expect(rows[3]).toEqual({ addr: 0x7fe7, lo: 38, hi: 39, isActive: false });
    expect(rows[4]).toEqual({ addr: 0x7fe9, lo: null, hi: null, isActive: false });
  });
});

describe("StackPanel", () => {
  beforeEach(() => {
    resetTauriMocks();
    invoke.mockImplementation(async (cmd: string) => {
      if (cmd === "get_cpu_description") return DESCRIPTION_6502;
      if (cmd === "get_stack") return stack6502(0xfd);
      return undefined;
    });
  });

  it("shows the 6502 stack's page 1 addresses, wrapping around the page", async () => {
    render(
      <CpuDescriptionProvider>
        <StackPanel />
      </CpuDescriptionProvider>,
    );
    // The second row follows $01FE/$01FF with $0100 once the description says the stack is
    // page-fixed.
    await waitFor(() => expect(screen.getByText("0100")).toBeInTheDocument());
    expect(screen.getByText("01FE")).toBeInTheDocument();
    expect(screen.queryByText("0200")).not.toBeInTheDocument();
  });
});
