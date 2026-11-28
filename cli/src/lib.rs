//! What the emulator's front ends share: the CPU families and how a
//! configuration names its CPU. The `emma6k` binary and the debugger both
//! use it, so they agree on which variants each family runs and which
//! templates there are.

pub mod family;
pub mod variant;
