//! Tests of the shared front end and driver through a toy [`Target`]. The
//! 6502's assembler (`emma65::assembler`) tests the same code with a real
//! instruction set.
//!
//! The toy's instructions: `NOP` is one byte (`$00`, or whatever `.nopcode`
//! last set); `JMP expr` is `$11 lo` when `expr` fits a byte and
//! `$10 hi lo` (big-endian) otherwise, or while `expr` is unresolved.

use std::collections::HashMap;

use super::eval::evaluate;
use super::expr::Expr;
use super::statement::symbol_name;
use super::*;

struct Toy;

#[derive(Debug)]
enum ToyInstruction<'a> {
    Nop,
    Jmp(Expr<'a>),
}

#[derive(Debug)]
struct NopCode<'a>(Expr<'a>);

impl Target for Toy {
    type Instruction<'a> = ToyInstruction<'a>;
    type Directive<'a> = NopCode<'a>;
    type State = u8;

    const WORD_ORDER: ByteOrder = ByteOrder::Big;

    fn parse_instruction<'a>(&self, parser: &mut Parser<'a>) -> Result<ToyInstruction<'a>, Error> {
        let token = parser.advance().unwrap();
        match symbol_name(&token).to_ascii_uppercase().as_str() {
            "NOP" => Ok(ToyInstruction::Nop),
            "JMP" => Ok(ToyInstruction::Jmp(parser.parse_expr()?)),
            name => Err(Error::from(
                token.location.line,
                token.location.column,
                &format!("unknown mnemonic '{name}'"),
            )),
        }
    }

    fn parse_directive<'a>(
        &self,
        name: &str,
        _location: Location,
        parser: &mut Parser<'a>,
    ) -> Option<Result<NopCode<'a>, Error>> {
        (name == "nopcode").then(|| parser.parse_expr().map(NopCode))
    }

    fn initial_state(&self) -> u8 {
        0x00
    }

    fn apply_directive(
        &self,
        directive: &NopCode<'_>,
        state: &mut u8,
        symbols: &HashMap<String, Operand>,
        location: Location,
    ) -> Result<(), Error> {
        match evaluate(&directive.0, symbols)? {
            Some(value) => {
                *state = value as u8;
                Ok(())
            }
            None => Err(Error::from(
                location.line,
                location.column,
                "'.nopcode' must not depend on a forward reference",
            )),
        }
    }

    fn encode(
        &self,
        instruction: &ToyInstruction<'_>,
        _address: u16,
        symbols: &HashMap<String, Operand>,
        state: &u8,
        _location: Location,
    ) -> Result<Encoded, Error> {
        Ok(match instruction {
            ToyInstruction::Nop => Encoded {
                byte_len: 1,
                bytes: Some(vec![*state]),
            },
            ToyInstruction::Jmp(expr) => match evaluate(expr, symbols)? {
                Some(value) if value <= 0xFF => Encoded {
                    byte_len: 2,
                    bytes: Some(vec![0x11, value as u8]),
                },
                Some(value) => {
                    let [hi, lo] = (value as u16).to_be_bytes();
                    Encoded {
                        byte_len: 3,
                        bytes: Some(vec![0x10, hi, lo]),
                    }
                }
                None => Encoded {
                    byte_len: 3,
                    bytes: None,
                },
            },
        })
    }
}

fn assemble_ok(source: &str) -> AssembledProgram {
    assemble(&Toy, source).unwrap_or_else(|errors| panic!("expected success, got {errors:?}"))
}

fn assemble_err(source: &str) -> Vec<Error> {
    assemble(&Toy, source).expect_err("expected assembly to fail")
}

#[test]
fn assembles_instructions_labels_and_shared_directives() {
    let program = assemble_ok(
        ".org $1000\nstart: NOP\nJMP start\n.byte 1, \"A\"\n.word $1234\n.res 2\nend:\n",
    );
    assert_eq!(program.segments.len(), 1);
    assert_eq!(program.segments[0].origin, 0x1000);
    assert_eq!(
        program.segments[0].bytes,
        vec![0x00, 0x10, 0x10, 0x00, 1, b'A', 0x12, 0x34, 0, 0]
    );
    assert_eq!(program.symbols.address_for("start"), Some(0x1000));
    assert_eq!(program.symbols.address_for("end"), Some(0x100A));
}

#[test]
fn word_follows_the_targets_byte_order() {
    assert_eq!(ByteOrder::Big.word_bytes(0x1234), [0x12, 0x34]);
    assert_eq!(ByteOrder::Little.word_bytes(0x1234), [0x34, 0x12]);
    let program = assemble_ok(".org 0\n.word $ABCD, target\ntarget:\n");
    assert_eq!(program.segments[0].bytes, vec![0xAB, 0xCD, 0x00, 0x04]);
}

#[test]
fn forward_reference_shrinks_to_the_short_encoding() {
    let program = assemble_ok(".org $1000\nJMP near\nnear = $10\n");
    assert_eq!(program.segments[0].bytes, vec![0x11, 0x10]);
}

#[test]
fn labels_after_a_shrunk_instruction_move_with_it() {
    let program = assemble_ok(".org $1000\nJMP near\nafter: NOP\nnear = $10\n");
    assert_eq!(program.symbols.address_for("after"), Some(0x1002));
}

#[test]
fn target_directive_applies_from_that_point_onward_each_pass() {
    let program = assemble_ok(".org 0\nNOP\n.nopcode $EA\nNOP\nJMP fwd\nfwd = 1\n");
    assert_eq!(program.segments[0].bytes, vec![0x00, 0xEA, 0x11, 0x01]);
}

#[test]
fn target_directive_errors_are_reported() {
    let errors = assemble_err(".org 0\n.nopcode later\nlater = 1\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message().contains("forward reference"))
    );
}

#[test]
fn unknown_directive_is_an_error() {
    let errors = assemble_err(".org 0\n.bogus 1\n");
    assert!(errors[0].message().contains("unknown directive '.bogus'"));
}

#[test]
fn target_parse_errors_are_reported() {
    let errors = assemble_err(".org 0\nFROB\n");
    assert_eq!(errors[0].line(), 2);
    assert!(errors[0].message().contains("unknown mnemonic"));
}

#[test]
fn undefined_symbol_is_reported_on_the_final_pass() {
    let errors = assemble_err(".org 0\nJMP nowhere\n");
    assert!(errors.iter().any(|e| e.message() == "undefined symbol"));
}

#[test]
fn non_convergence_is_reported_when_the_pass_limit_is_too_low() {
    let errors = driver::assemble_with_pass_limit(&Toy, ".org 0\nJMP near\nnear = 1\n", 1)
        .expect_err("expected non-convergence");
    assert!(
        errors
            .iter()
            .any(|e| e.message().contains("did not converge"))
    );
}
