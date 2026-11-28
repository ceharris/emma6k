//! 6502-driven tests of the shared statement grammar in
//! `emma6k_core::asm::statement`, parsing through the 6502's [`Mos6502`]
//! target.

use emma6k_core::asm::Error;
use emma6k_core::asm::statement::{ByteOperand, Directive, Statement, parse_program};
use emma6k_core::location::Location;

use super::operand::OperandSyntax;
use super::target::{CpuDirective, Instruction, Mos6502};
use crate::mos6502::cpu::opcodes::Mnemonic;
use crate::mos6502::cpu::variant::CpuVariant;

type Statements<'a> = Vec<(Statement<'a, Instruction<'a>, CpuDirective>, Location)>;

/// Parses one line (or a few) of 6502 source.
fn parse_line_source(source: &str) -> Result<Statements<'_>, Error> {
    parse_program(&Mos6502::new(), source)
}

#[test]
fn parse_blank_line() {
    assert_eq!(parse_line_source("").unwrap(), vec![]);
    assert_eq!(parse_line_source("\n").unwrap(), vec![]);
    assert_eq!(parse_line_source("  \n").unwrap(), vec![]);
}

#[test]
fn parse_label_only() {
    let statements = parse_line_source("loop:\n").unwrap();
    assert_eq!(statements.len(), 1);
    assert_eq!(statements[0].0, Statement::Label(String::from("loop")));
}

#[test]
fn parse_label_followed_by_instruction() {
    let statements = parse_line_source("loop: LDA #1\n").unwrap();
    assert_eq!(statements.len(), 2);
    assert_eq!(statements[0].0, Statement::Label(String::from("loop")));
    match &statements[1].0 {
        Statement::Instruction((Mnemonic::Lda, OperandSyntax::Immediate(_))) => {}
        other => panic!("expected Instruction(Lda, Immediate), got {other:?}"),
    }
}

#[test]
fn parse_label_followed_by_directive() {
    let statements = parse_line_source("here: .byte 1\n").unwrap();
    assert_eq!(statements.len(), 2);
    assert_eq!(statements[0].0, Statement::Label(String::from("here")));
    assert!(matches!(
        &statements[1].0,
        Statement::Directive(Directive::Byte(_))
    ));
}

#[test]
fn parse_symbol_assign_with_equal() {
    let statements = parse_line_source("FOO = 42\n").unwrap();
    assert_eq!(statements.len(), 1);
    match &statements[0].0 {
        Statement::SymbolAssign(name, expr) => {
            assert_eq!(name, "FOO");
            assert_eq!(expr.token().text(), "42");
        }
        other => panic!("expected SymbolAssign, got {other:?}"),
    }
}

#[test]
fn parse_symbol_assign_with_equ() {
    let statements = parse_line_source("FOO .equ 42\n").unwrap();
    assert_eq!(statements.len(), 1);
    match &statements[0].0 {
        Statement::SymbolAssign(name, expr) => {
            assert_eq!(name, "FOO");
            assert_eq!(expr.token().text(), "42");
        }
        other => panic!("expected SymbolAssign, got {other:?}"),
    }
}

#[test]
fn parse_org_directive() {
    let statements = parse_line_source(".org $8000\n").unwrap();
    assert_eq!(statements.len(), 1);
    match &statements[0].0 {
        Statement::Directive(Directive::Org(expr)) => assert_eq!(expr.token().text(), "$8000"),
        other => panic!("expected Directive(Org), got {other:?}"),
    }
}

#[test]
fn parse_byte_directive_with_mixed_operands() {
    let statements = parse_line_source(".byte 1, \"AB\", 2\n").unwrap();
    match &statements[0].0 {
        Statement::Directive(Directive::Byte(operands)) => {
            assert_eq!(operands.len(), 3);
            assert!(matches!(&operands[0], ByteOperand::Expr(_)));
            assert_eq!(operands[1], ByteOperand::String(String::from("AB")));
            assert!(matches!(&operands[2], ByteOperand::Expr(_)));
        }
        other => panic!("expected Directive(Byte), got {other:?}"),
    }
}

#[test]
fn parse_word_directive_with_multiple_operands() {
    let statements = parse_line_source(".word $1234, $5678\n").unwrap();
    match &statements[0].0 {
        Statement::Directive(Directive::Word(exprs)) => assert_eq!(exprs.len(), 2),
        other => panic!("expected Directive(Word), got {other:?}"),
    }
}

#[test]
fn parse_res_directive() {
    let statements = parse_line_source(".res 4\n").unwrap();
    match &statements[0].0 {
        Statement::Directive(Directive::Res(expr)) => assert_eq!(expr.token().text(), "4"),
        other => panic!("expected Directive(Res), got {other:?}"),
    }
}

#[test]
fn parse_instruction_alone() {
    let statements = parse_line_source("NOP\n").unwrap();
    assert_eq!(statements.len(), 1);
    assert_eq!(
        statements[0].0,
        Statement::Instruction((Mnemonic::Nop, OperandSyntax::None))
    );
}

#[test]
fn parse_unknown_mnemonic_errors() {
    let result = parse_line_source("FROB #1\n");
    assert!(result.is_err());
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("unknown mnemonic")
    );
}

#[test]
fn parse_unknown_directive_errors() {
    let result = parse_line_source(".bogus 1\n");
    assert!(result.is_err());
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("unknown directive")
    );
}

#[test]
fn parse_trailing_garbage_errors() {
    let result = parse_line_source("LDA #1 extra\n");
    assert!(result.is_err());
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("expected end of line")
    );
}

#[test]
fn parse_program_multiple_lines() {
    let statements = parse_line_source("start:\n  LDA #1\n  STA $10\n").unwrap();
    assert_eq!(statements.len(), 3);
    assert_eq!(statements[0].0, Statement::Label(String::from("start")));
    assert!(matches!(
        &statements[1].0,
        Statement::Instruction((Mnemonic::Lda, _))
    ));
    assert!(matches!(
        &statements[2].0,
        Statement::Instruction((Mnemonic::Sta, _))
    ));
}

#[test]
fn parse_program_skips_blank_lines_and_comments() {
    let statements = parse_line_source("\n; a comment\nNOP\n\n").unwrap();
    assert_eq!(statements.len(), 1);
    assert_eq!(
        statements[0].0,
        Statement::Instruction((Mnemonic::Nop, OperandSyntax::None))
    );
}

#[test]
fn parse_setcpu_directive_accepts_all_aliases() {
    for (text, expected) in [
        ("65c02", CpuVariant::Cmos65C02),
        ("C02", CpuVariant::Cmos65C02),
        ("wdc65c02", CpuVariant::Wdc65C02),
        ("W65C02", CpuVariant::Wdc65C02),
    ] {
        let source = format!(".setcpu \"{text}\"\n");
        let statements = parse_line_source(&source).unwrap();
        assert_eq!(
            statements[0].0,
            Statement::Directive(Directive::Target(CpuDirective::SetCpu(expected)))
        );
    }
}

#[test]
fn parse_setcpu_unknown_variant_errors() {
    let result = parse_line_source(".setcpu \"6809\"\n");
    assert!(result.is_err());
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("unknown CPU variant")
    );
}

#[test]
fn parse_setcpu_requires_quoted_string() {
    let result = parse_line_source(".setcpu wdc65c02\n");
    assert!(result.is_err());
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("expected a quoted CPU variant string")
    );
}

#[test]
fn parse_wdc_only_mnemonic_syntax_is_variant_independent() {
    // Parsing always disambiguates operand syntax against the Wdc65C02
    // superset table, regardless of any .setcpu in the source (or lack
    // thereof) — whether BBR0 is actually *valid* is an encode-time
    // concern for the driver, not a parse-time one.
    let statements = parse_line_source("BBR0 $10, label\n").unwrap();
    assert!(matches!(
        &statements[0].0,
        Statement::Instruction((Mnemonic::Bbr0, OperandSyntax::ZeroPageRelative(_, _)))
    ));
}
