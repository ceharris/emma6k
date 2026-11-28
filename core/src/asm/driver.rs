//! Multi-pass assembly driver: lays out statements across one or more
//! `.org`-delimited segments, resolves forward references, lets the
//! [`Target`] shrink operands whose values turn out to fit a shorter
//! encoding (the 6502's zero page), and reports final errors (undefined
//! symbols, segment overlaps, duplicate labels, non-convergence).
//!
//! See the "Multi-pass resolution and zero-page optimization" and
//! "`.org` / multiple segments" sections of `plan/assembler-plan.md` for the
//! algorithm this implements.

use std::collections::HashMap;

use super::Target;
use super::error::Error;
use super::eval::evaluate;
use super::statement::{ByteOperand, Directive, Statement, TargetStatement, parse_program};
use crate::location::Location;

/// A finite cap on the number of layout passes, guarding against a genuine
/// non-convergence (see the plan doc) rather than looping forever. Ordinary
/// programs converge in two or three passes; with no macro support or other
/// source of pass-count variability, this leaves generous headroom without
/// being effectively unbounded.
const DEFAULT_MAX_PASSES: usize = 10;

/// One contiguous output region, starting at `origin`.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub origin: u16,
    pub bytes: Vec<u8>,
}

impl Segment {
    /// The address just past the last byte emitted so far — i.e. where the
    /// next label or instruction in this segment lands.
    fn next_address(&self) -> u16 {
        self.origin.wrapping_add(self.bytes.len() as u16)
    }
}

/// The driver's result: assembled segments in `.org` order, plus the final
/// symbol table (label and `=`/`.equ` values). [`super::assemble`] adapts
/// this into an [`super::AssembledProgram`].
#[derive(Debug, Clone, PartialEq)]
pub struct AssembledOutput {
    pub segments: Vec<Segment>,
    pub symbols: HashMap<String, u32>,
}

/// Assembles `source` for `target`, collecting all errors found rather than
/// stopping at the first one.
pub fn assemble<T: Target>(target: &T, source: &str) -> Result<AssembledOutput, Vec<Error>> {
    assemble_with_pass_limit(target, source, DEFAULT_MAX_PASSES)
}

/// [`assemble`] with at most `max_passes` layout passes before giving up
/// with a non-convergence error; a low limit lets a test provoke that error.
pub fn assemble_with_pass_limit<T: Target>(
    target: &T,
    source: &str,
    max_passes: usize,
) -> Result<AssembledOutput, Vec<Error>> {
    let statements = parse_program(target, source).map_err(|e| vec![e])?;
    check_duplicate_symbols(&statements)?;

    let mut symbols: HashMap<String, u32> = HashMap::new();
    let mut sizes: Vec<u8> = vec![0; statements.len()];
    let mut converged = false;
    for _ in 0..max_passes {
        let previous_sizes = sizes.clone();
        run_pass(target, &statements, &mut symbols, &mut sizes, false)?;
        if sizes == previous_sizes {
            converged = true;
            break;
        }
    }
    if !converged {
        return Err(vec![Error::from(
            0,
            0,
            "assembly did not converge after too many passes (possible oscillating forward references)",
        )]);
    }

    let segments = run_pass(target, &statements, &mut symbols, &mut sizes, true)?;
    check_overlaps(&segments)?;

    Ok(AssembledOutput { segments, symbols })
}

/// Detects two symbol-defining statements (a `Label` or `SymbolAssign`)
/// using the same name. This is a purely syntactic check, independent of
/// address resolution, so it runs once up front rather than per pass.
fn check_duplicate_symbols<I, D>(
    statements: &[(Statement<'_, I, D>, Location)],
) -> Result<(), Vec<Error>> {
    let mut seen: HashMap<&str, Location> = HashMap::new();
    let mut errors = Vec::new();
    for (statement, location) in statements {
        let name = match statement {
            Statement::Label(name) => Some(name.as_str()),
            Statement::SymbolAssign(name, _) => Some(name.as_str()),
            _ => None,
        };
        if let Some(name) = name {
            if seen.contains_key(name) {
                errors.push(Error::from(
                    location.line,
                    location.column,
                    &format!("symbol '{name}' is already defined"),
                ));
            } else {
                seen.insert(name, *location);
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Detects segment address ranges that overlap, or two segments sharing the
/// same origin even when neither has emitted any bytes yet.
fn check_overlaps(segments: &[Segment]) -> Result<(), Vec<Error>> {
    let mut errors = Vec::new();
    for i in 0..segments.len() {
        for j in (i + 1)..segments.len() {
            let a = &segments[i];
            let b = &segments[j];
            let a_start = a.origin as u32;
            let a_end = a_start + a.bytes.len() as u32;
            let b_start = b.origin as u32;
            let b_end = b_start + b.bytes.len() as u32;
            if a_start == b_start || (a_start < b_end && b_start < a_end) {
                errors.push(Error::from(
                    0,
                    0,
                    &format!("segment at ${a_start:04X} overlaps segment at ${b_start:04X}"),
                ));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Walks every statement once, in order, updating `symbols` and `sizes` in
/// place and returning the resulting segments. On a non-final pass an
/// operand that isn't yet resolvable is tolerated (filled with a zero
/// placeholder so layout can continue); on the final pass it becomes a real
/// `Error`. Structural errors (a byte-emitting statement before any `.org`,
/// an `.org`/`.res` address that depends on a forward reference) are always
/// real errors, since more passes can never fix them.
fn run_pass<'a, T: Target>(
    target: &T,
    statements: &[(TargetStatement<'a, T>, Location)],
    symbols: &mut HashMap<String, u32>,
    sizes: &mut [u8],
    final_pass: bool,
) -> Result<Vec<Segment>, Vec<Error>> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut current: Option<usize> = None;
    let mut errors: Vec<Error> = Vec::new();
    // Resets to the target's initial state at the start of every pass, then
    // gets updated in source order as the target's directives (the 6502's
    // `.setcpu`) are (re-)encountered — matching how `current`/`segments`
    // are also rebuilt fresh each pass.
    let mut state = target.initial_state();

    for (i, (statement, location)) in statements.iter().enumerate() {
        let location = *location;
        match statement {
            Statement::Label(name) => match current {
                Some(seg) => {
                    let addr = segments[seg].next_address();
                    symbols.insert(name.clone(), addr as u32);
                }
                None => errors.push(no_active_segment_error(location, "label")),
            },
            Statement::Directive(Directive::Target(directive)) => {
                if let Err(e) = target.apply_directive(directive, &mut state, symbols, location) {
                    errors.push(e);
                }
            }
            Statement::SymbolAssign(name, expr) => match evaluate(expr, symbols) {
                Ok(Some(value)) => {
                    symbols.insert(name.clone(), value);
                }
                Ok(None) => {
                    if final_pass {
                        errors.push(undefined_symbol_error(location));
                    }
                }
                Err(e) => errors.push(e),
            },
            Statement::Directive(Directive::Org(expr)) => match evaluate(expr, symbols) {
                Ok(Some(value)) => {
                    segments.push(Segment {
                        origin: value as u16,
                        bytes: Vec::new(),
                    });
                    current = Some(segments.len() - 1);
                }
                Ok(None) => errors.push(Error::from(
                    location.line,
                    location.column,
                    "'.org' address must not depend on a forward reference",
                )),
                Err(e) => errors.push(e),
            },
            Statement::Directive(Directive::Byte(operands)) => match current {
                None => errors.push(no_active_segment_error(location, "'.byte'")),
                Some(seg) => {
                    for operand in operands {
                        match operand {
                            ByteOperand::String(s) => segments[seg].bytes.extend(s.bytes()),
                            ByteOperand::Expr(expr) => match evaluate(expr, symbols) {
                                Ok(Some(value)) => {
                                    if value > 0xFF {
                                        errors.push(Error::from(
                                            location.line,
                                            location.column,
                                            "'.byte' operand out of range",
                                        ));
                                    }
                                    segments[seg].bytes.push(value as u8);
                                }
                                Ok(None) => {
                                    if final_pass {
                                        errors.push(undefined_symbol_error(location));
                                    }
                                    segments[seg].bytes.push(0);
                                }
                                Err(e) => errors.push(e),
                            },
                        }
                    }
                }
            },
            Statement::Directive(Directive::Word(exprs)) => match current {
                None => errors.push(no_active_segment_error(location, "'.word'")),
                Some(seg) => {
                    for expr in exprs {
                        match evaluate(expr, symbols) {
                            Ok(Some(value)) => segments[seg]
                                .bytes
                                .extend(T::WORD_ORDER.word_bytes(value as u16)),
                            Ok(None) => {
                                if final_pass {
                                    errors.push(undefined_symbol_error(location));
                                }
                                segments[seg].bytes.extend([0, 0]);
                            }
                            Err(e) => errors.push(e),
                        }
                    }
                }
            },
            Statement::Directive(Directive::Res(expr)) => match current {
                None => errors.push(no_active_segment_error(location, "'.res'")),
                Some(seg) => match evaluate(expr, symbols) {
                    Ok(Some(value)) => {
                        let new_len = segments[seg].bytes.len() + value as usize;
                        segments[seg].bytes.resize(new_len, 0);
                    }
                    Ok(None) => errors.push(Error::from(
                        location.line,
                        location.column,
                        "'.res' size must not depend on a forward reference",
                    )),
                    Err(e) => errors.push(e),
                },
            },
            Statement::Instruction(instruction) => match current {
                None => errors.push(no_active_segment_error(location, "instruction")),
                Some(seg) => {
                    let addr = segments[seg].next_address();
                    match target.encode(instruction, addr, symbols, &state, location) {
                        Ok(encoded) => {
                            sizes[i] = encoded.byte_len;
                            match encoded.bytes {
                                Some(bytes) => segments[seg].bytes.extend(bytes),
                                None => {
                                    if final_pass {
                                        errors.push(undefined_symbol_error(location));
                                    }
                                    let new_len =
                                        segments[seg].bytes.len() + encoded.byte_len as usize;
                                    segments[seg].bytes.resize(new_len, 0);
                                }
                            }
                        }
                        Err(e) => errors.push(e),
                    }
                }
            },
        }
    }

    if errors.is_empty() {
        Ok(segments)
    } else {
        Err(errors)
    }
}

fn undefined_symbol_error(location: Location) -> Error {
    Error::from(location.line, location.column, "undefined symbol")
}

fn no_active_segment_error(location: Location, what: &str) -> Error {
    Error::from(
        location.line,
        location.column,
        &format!("{what} used before any '.org' directive"),
    )
}
