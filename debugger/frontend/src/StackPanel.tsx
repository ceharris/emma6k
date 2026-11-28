import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { useCpuDescription } from "./CpuDescriptionContext";
import { DataRadix, RadixButton, STACK_RADIX_CYCLE, useRadixCycle } from "./RadixControl";
import { buildRows, StackSnapshot } from "./stackRows";
import "./styles/stack.scss";

// --- radix cycling ---

/** Formats a stack byte in `radix`. Not reachable with "bin" — `dataRadix` is always confined to `STACK_RADIX_CYCLE` — but the switch stays exhaustive over the shared `DataRadix` type. */
function formatData(value: number | null, radix: DataRadix): string {
  if (value !== null) {
    switch (radix) {
      case "hex":
        return value.toString(16).toUpperCase().padStart(2, "0");
      case "udec":
        return value.toString(10);
      case "sdec":
        return ((value << 24) >> 24).toString(10);
      case "oct":
        return value.toString(8).padStart(3, "0");
      case "bin":
        return value.toString(2).padStart(8, "0");
    }
  } else {
    switch (radix) {
      case "hex":
        return "--";
      case "udec":
        return "---";
      case "sdec":
        return "----";
      case "oct":
        return "---";
      case "bin":
        return "--------";
    }
  }
}

function radixDataWidth(radix: DataRadix): number {
  switch (radix) {
    case "hex":
      return 50;
    case "udec":
      return 60;
    case "sdec":
      return 65;
    case "oct":
      return 60;
    case "bin":
      return 100;
  }
}

/** Returns a 4-digit uppercase hex address string. */
function formatAddr(addr: number): string {
  return addr.toString(16).toUpperCase().padStart(4, "0");
}

export default function StackPanel() {
  const [snap, setSnap] = useState<StackSnapshot | null>(null);
  const pageFixed = useCpuDescription()?.stack.page_fixed ?? false;
  const [dataRadix, cycleDataRadix] = useRadixCycle(STACK_RADIX_CYCLE, "hex");
  const [alignOdd, setAlignOdd] = useState(false);
  const dataWidth = radixDataWidth(dataRadix);

  const fetchStack = useCallback(async () => {
    try {
      const result = await invoke<StackSnapshot>("get_stack");
      setSnap(result);
    } catch (e) {
      console.error("get_stack failed:", e);
    }
  }, []);

  useEffect(() => {
    fetchStack();
  }, [fetchStack]);

  useEffect(() => {
    const unlistenHalted = listen("debugger-halted", () => {
      fetchStack();
    });
    const unlistenTick = listen("debugger-running-tick", () => {
      fetchStack();
    });
    return () => {
      unlistenHalted.then((f) => f());
      unlistenTick.then((f) => f());
    };
  }, [fetchStack]);

  const toggleAlign = useCallback(() => {
    setAlignOdd((v) => !v);
  }, []);

  return (
    <div className="stack-panel">
      <div className="stack-header">
        <RadixButton radix={dataRadix} onCycle={cycleDataRadix} />
        <button className="align-btn" onClick={toggleAlign} title="Toggle even/odd word alignment">
          {alignOdd ? "ODD" : "EVEN"}
        </button>
      </div>
      {snap === null ? (
        <span className="stack-empty">Waiting…</span>
      ) : (
        <div className="stack-body" style={{ width: `${dataWidth}%` }}>
          {buildRows(snap, pageFixed, alignOdd).map((row) => (
            <div key={row.addr} className={`stack-row${row.isActive ? " stack-row-active" : ""}`}>
              <span className="stack-chevron">{row.isActive ? ">" : " "}</span>
              <span className="stack-addr">{formatAddr(row.addr)}</span>
              <span className={`stack-lo${row.lo === null ? " stack-placeholder" : ""}`}>
                {formatData(row.lo, dataRadix)}
              </span>
              <span className={`stack-hi${row.hi === null ? " stack-placeholder" : ""}`}>
                {formatData(row.hi, dataRadix)}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
