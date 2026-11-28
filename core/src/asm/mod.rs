//! The CPU-neutral part of the assemblers: the scanner, tokens, expression
//! tree, parser and evaluator, the line-oriented statement grammar with its
//! shared directives (`.org`, `.byte`, `.word`, `.res`, `=`/`.equ`), and the
//! multi-pass driver that lays out segments and resolves forward references.
//!
//! A CPU crate supplies a [`Target`]: it parses an instruction's mnemonic and
//! operand, defines any directives of its own, and encodes an instruction at
//! an address once its operands resolve. [`assemble`] runs the whole
//! pipeline for a target. `emma65::assembler` is the 6502's target; see
//! `plan/assembler-plan.md` for the original design.

pub mod driver;
pub mod error;
pub mod eval;
pub mod expr;
pub mod parser;
pub mod scanner;
pub mod statement;
pub mod token;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use crate::bus::{SymbolSource, SymbolTable};
use crate::location::Location;

pub use driver::Segment;
pub use error::Error;
pub use expr::Operand;
pub use parser::Parser;

/// The order in which a target stores a 16-bit word's two bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteOrder {
    /// Low byte first (the 6502).
    Little,
    /// High byte first (the 6809).
    Big,
}

impl ByteOrder {
    /// `value`'s two bytes, in address order.
    pub fn word_bytes(self, value: u16) -> [u8; 2] {
        match self {
            ByteOrder::Little => value.to_le_bytes(),
            ByteOrder::Big => value.to_be_bytes(),
        }
    }
}

/// An instruction encoded at an address. `byte_len` is always known, even
/// when an operand's value isn't yet (the driver's multi-pass design lays
/// out the program with it); `bytes` is `None` until every symbol the
/// operand depends on resolves.
#[derive(Clone, Debug, PartialEq)]
pub struct Encoded {
    pub byte_len: u8,
    pub bytes: Option<Vec<u8>>,
}

/// The CPU an assembler targets: everything the shared front end and driver
/// leave to the CPU.
pub trait Target {
    /// A parsed instruction: its mnemonic and operand syntax.
    type Instruction<'a>;
    /// A directive of the target's own (the 6502's `.setcpu`).
    type Directive<'a>;
    /// Assembly state the target's directives change (the 6502's CPU
    /// variant). Each pass starts from [`Target::initial_state`] and applies
    /// the directives in source order.
    type State;

    /// The byte order `.word` emits.
    const WORD_ORDER: ByteOrder;

    /// Parses an instruction at the parser's current token, a symbol, and
    /// its operand, leaving the end of the line unconsumed.
    fn parse_instruction<'a>(
        &self,
        parser: &mut Parser<'a>,
    ) -> Result<Self::Instruction<'a>, Error>;

    /// Parses the operands of the directive `name` (lowercase, without its
    /// dot, at `location`), or returns `None` if the target has no such
    /// directive. Only names the shared grammar doesn't handle get here.
    fn parse_directive<'a>(
        &self,
        name: &str,
        location: Location,
        parser: &mut Parser<'a>,
    ) -> Option<Result<Self::Directive<'a>, Error>>;

    /// The state each pass starts from.
    fn initial_state(&self) -> Self::State;

    /// Applies `directive`, at `location`, to `state`, with the symbols
    /// resolved so far.
    fn apply_directive(
        &self,
        directive: &Self::Directive<'_>,
        state: &mut Self::State,
        symbols: &HashMap<String, Operand>,
        location: Location,
    ) -> Result<(), Error>;

    /// Encodes `instruction`, at `location` in the source, to be placed at
    /// `address`, choosing the shortest encoding the symbols resolved so far
    /// allow.
    fn encode(
        &self,
        instruction: &Self::Instruction<'_>,
        address: u16,
        symbols: &HashMap<String, Operand>,
        state: &Self::State,
        location: Location,
    ) -> Result<Encoded, Error>;
}

/// The result of successfully assembling a source program: one or more
/// `.org`-delimited output segments, in source order, plus the final symbol
/// table (labels and `=`/`.equ` assignments).
#[derive(Debug, Clone)]
pub struct AssembledProgram {
    pub segments: Vec<Segment>,
    pub symbols: SymbolTable,
}

/// Assembles `source` for `target`, returning the resulting segments and
/// symbol table, or every error found (not just the first) if assembly
/// fails.
pub fn assemble<T: Target>(target: &T, source: &str) -> Result<AssembledProgram, Vec<Error>> {
    let output = driver::assemble(target, source)?;
    let mut symbols = SymbolTable::new();
    for (name, address) in output.symbols {
        symbols.insert_tagged(name, address as u16, SymbolSource::Assembler);
    }
    Ok(AssembledProgram {
        segments: output.segments,
        symbols,
    })
}
