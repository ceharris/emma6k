//! 6502-driven tests of the shared multi-pass driver in
//! `emma6k_core::asm::driver`, assembling through the 6502's [`Mos6502`]
//! target.

use emma6k_core::asm::Error;
use emma6k_core::asm::driver::{self, AssembledOutput, Segment};

use super::target::Mos6502;

fn assemble(source: &str) -> Result<AssembledOutput, Vec<Error>> {
    driver::assemble(&Mos6502::new(), source)
}

fn assemble_with_pass_limit(
    source: &str,
    max_passes: usize,
) -> Result<AssembledOutput, Vec<Error>> {
    driver::assemble_with_pass_limit(&Mos6502::new(), source, max_passes)
}

fn assemble_ok(source: &str) -> AssembledOutput {
    assemble(source).unwrap_or_else(|errors| {
        panic!("expected assembly to succeed, got errors: {errors:?}");
    })
}

fn assemble_err(source: &str) -> Vec<Error> {
    assemble(source).expect_err("expected assembly to fail")
}

#[test]
fn assemble_simple_program() {
    let output = assemble_ok(".org $8000\nLDA #$01\nSTA $10\n");
    assert_eq!(output.segments.len(), 1);
    assert_eq!(output.segments[0].origin, 0x8000);
    assert_eq!(output.segments[0].bytes, vec![0xA9, 0x01, 0x85, 0x10]);
}

#[test]
fn assemble_backward_label_reference() {
    let output = assemble_ok(".org $8000\nloop:\nLDA #1\nBNE loop\n");
    // BNE at 0x8002, 2-byte instruction -> pc_after 0x8004, target 0x8000 -> displacement -4
    assert_eq!(output.segments[0].bytes, vec![0xA9, 0x01, 0xD0, 0xFC]);
    assert_eq!(output.symbols.get("loop"), Some(&0x8000));
}

#[test]
fn assemble_forward_reference_shrinks_to_zero_page() {
    // "forward" isn't known until after LDA references it; the driver
    // must discover across passes that it resolves to a zero-page value
    // and shrink the instruction from Absolute down to ZeroPage.
    let output = assemble_ok(".org $0200\nLDA forward\nforward = $10\n");
    assert_eq!(output.segments[0].bytes, vec![0xA5, 0x10]);
}

#[test]
fn assemble_forward_label_reference_in_word_directive() {
    let output = assemble_ok(".org $8000\n.word target\nNOP\ntarget:\nNOP\n");
    // target = $8000 (segment origin) + 2 (.word) + 1 (first NOP) = $8003
    assert_eq!(&output.segments[0].bytes[0..2], &[0x03, 0x80]);
    assert_eq!(output.symbols.get("target"), Some(&0x8003));
}

#[test]
fn assemble_byte_directive_with_mixed_operands() {
    let output = assemble_ok(".org $8000\n.byte 1, \"AB\", 2\n");
    assert_eq!(output.segments[0].bytes, vec![1, b'A', b'B', 2]);
}

#[test]
fn assemble_word_directive() {
    let output = assemble_ok(".org $8000\n.word $1234, $5678\n");
    assert_eq!(output.segments[0].bytes, vec![0x34, 0x12, 0x78, 0x56]);
}

#[test]
fn assemble_res_directive_reserves_zero_bytes_and_advances_address() {
    let output = assemble_ok(".org $8000\n.res 4\nhere:\nNOP\n");
    assert_eq!(output.segments[0].bytes, vec![0, 0, 0, 0, 0xEA]);
    assert_eq!(output.symbols.get("here"), Some(&0x8004));
}

#[test]
fn assemble_multiple_non_overlapping_segments() {
    let output = assemble_ok(".org $8000\nLDA #1\n.org $9000\nLDA #2\n");
    assert_eq!(output.segments.len(), 2);
    assert_eq!(
        output.segments[0],
        Segment {
            origin: 0x8000,
            bytes: vec![0xA9, 0x01]
        }
    );
    assert_eq!(
        output.segments[1],
        Segment {
            origin: 0x9000,
            bytes: vec![0xA9, 0x02]
        }
    );
}

#[test]
fn assemble_overlapping_segments_error() {
    let errors = assemble_err(".org $8000\n.byte 1,2,3,4,5\n.org $8003\nLDA #1\n");
    assert!(errors.iter().any(|e| e.message().contains("overlaps")));
}

#[test]
fn assemble_duplicate_org_address_error() {
    let errors = assemble_err(".org $8000\n.org $8000\n");
    assert!(errors.iter().any(|e| e.message().contains("overlaps")));
}

#[test]
fn assemble_byte_before_org_error() {
    let errors = assemble_err(".byte 1\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message().contains("before any '.org'"))
    );
}

#[test]
fn assemble_duplicate_label_error() {
    let errors = assemble_err(".org $8000\nfoo:\nfoo:\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message().contains("already defined"))
    );
}

#[test]
fn assemble_undefined_symbol_error() {
    let errors = assemble_err(".org $8000\nLDA missing\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message().contains("undefined symbol"))
    );
}

#[test]
fn assemble_res_forward_reference_error() {
    let errors = assemble_err(".org $8000\n.res forward\nforward = 4\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message().contains("forward reference"))
    );
}

#[test]
fn assemble_org_forward_reference_error() {
    let errors = assemble_err(".org forward\nforward = $8000\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message().contains("forward reference"))
    );
}

#[test]
fn assemble_non_convergence_reported_when_pass_limit_too_low() {
    // This program needs two passes to shrink LDA's forward reference to
    // zero page; capping at one pass must surface as an error rather
    // than silently emitting the wrong (unconverged) bytes.
    let errors = assemble_with_pass_limit(".org $0200\nLDA forward\nforward = $10\n", 1)
        .expect_err("expected non-convergence error");
    assert!(
        errors
            .iter()
            .any(|e| e.message().contains("did not converge"))
    );
}

#[test]
fn assemble_defaults_to_cmos_variant_rejecting_wdc_only_mnemonics() {
    let errors = assemble_err(".org $8000\nSTP\n");
    assert!(errors.iter().any(|e| e.message().contains("not supported")));
}

#[test]
fn assemble_setcpu_enables_wdc_only_mnemonics() {
    let output = assemble_ok(".setcpu \"wdc65c02\"\n.org $8000\nSTP\n");
    assert_eq!(output.segments[0].bytes, vec![0xDB]);
}

#[test]
fn assemble_setcpu_takes_effect_only_from_that_point_onward() {
    // STP before the .setcpu must still fail even though the source as a
    // whole selects the Wdc65C02 variant partway through.
    let errors = assemble_err(".org $8000\nSTP\n.setcpu \"wdc65c02\"\nSTP\n");
    assert!(errors.iter().any(|e| e.message().contains("not supported")));
}

#[test]
fn assemble_setcpu_can_switch_back_to_cmos() {
    // The first STP (under wdc65c02) must succeed; only the second, after
    // switching back to 65c02, should fail.
    let errors = assemble_err(".org $8000\n.setcpu \"wdc65c02\"\nSTP\n.setcpu \"65c02\"\nSTP\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message().contains("not supported"));
}

#[test]
fn assemble_setcpu_unknown_variant_error() {
    let errors = assemble_err(".setcpu \"6809\"\n");
    assert!(
        errors
            .iter()
            .any(|e| e.message().contains("unknown CPU variant"))
    );
}
