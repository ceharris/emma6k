import { describe, expect, it } from "vitest";
import { toBytes } from "./cpuDescription";

describe("toBytes", () => {
  it("writes least significant byte first for a little-endian CPU", () => {
    expect(toBytes(0x1234, 2, "little")).toEqual([0x34, 0x12]);
    expect(toBytes(0x12345678, 4, "little")).toEqual([0x78, 0x56, 0x34, 0x12]);
  });

  it("writes most significant byte first for a big-endian CPU", () => {
    expect(toBytes(0x1234, 2, "big")).toEqual([0x12, 0x34]);
    expect(toBytes(0x12345678, 4, "big")).toEqual([0x12, 0x34, 0x56, 0x78]);
  });

  it("writes a byte the same way in either order", () => {
    expect(toBytes(0xab, 1, "little")).toEqual([0xab]);
    expect(toBytes(0xab, 1, "big")).toEqual([0xab]);
  });

  it("splits a dword above 2^31 without sign trouble", () => {
    expect(toBytes(0xfedcba98, 4, "big")).toEqual([0xfe, 0xdc, 0xba, 0x98]);
  });
});
