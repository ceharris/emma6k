//! The 6502's [`Target`] for the shared assembler in `emma6k_core::asm`:
//! mnemonics and operand syntax, the `.setcpu` directive, and encoding.
//!
//! Operand syntax is parsed the same way regardless of the CPU variant
//! selected by any `.setcpu` directive appearing earlier in the source
//! (parsing always uses the `Wdc65C02` [`InstructionTable`] to disambiguate
//! syntax, since it's a strict superset of `Cmos65C02`). A mnemonic that
//! isn't actually valid under the variant active at that point in the
//! source (the driver's pass state) is a semantic error caught at encode
//! time, not a parse-time error — this keeps parsing independent of where
//! `.setcpu` directives happen to fall.

use std::collections::HashMap;

use emma6k_core::asm::expr::Operand;
use emma6k_core::asm::statement::symbol_name;
use emma6k_core::asm::token::TokenType;
use emma6k_core::asm::{ByteOrder, Encoded, Error, Parser, Target};
use emma6k_core::location::Location;

use super::instructions::{InstructionTable, mnemonic_from_str};
use super::operand::{OperandSyntax, encode, parse_operand};
use crate::mos6502::cpu::opcodes::{AddressingMode, Mnemonic};
use crate::mos6502::cpu::variant::CpuVariant;

/// A parsed 6502 instruction: its mnemonic and operand syntax.
pub type Instruction<'a> = (Mnemonic, OperandSyntax<'a>);

/// The 6502's own directives.
#[derive(Clone, Debug, PartialEq)]
pub enum CpuDirective {
    /// `.setcpu "variant"` — selects the CPU variant used to validate and
    /// encode every instruction from this point in the source onward
    /// (until the next `.setcpu`, if any). Defaults to `Cmos65C02` when no
    /// `.setcpu` has appeared yet.
    SetCpu(CpuVariant),
}

/// The 6502 assembler target, holding the [`InstructionTable`] for each
/// supported [`CpuVariant`], built once per assembly and selected per
/// instruction according to whichever variant is active at that point in
/// the source (see [`CpuDirective::SetCpu`]).
pub struct Mos6502 {
    cmos: InstructionTable,
    wdc: InstructionTable,
}

impl Mos6502 {
    pub fn new() -> Self {
        Self {
            cmos: InstructionTable::new(CpuVariant::Cmos65C02),
            wdc: InstructionTable::new(CpuVariant::Wdc65C02),
        }
    }

    fn table(&self, variant: CpuVariant) -> &InstructionTable {
        match variant {
            CpuVariant::Cmos65C02 => &self.cmos,
            CpuVariant::Wdc65C02 => &self.wdc,
        }
    }
}

impl Target for Mos6502 {
    type Instruction<'a> = Instruction<'a>;
    type Directive<'a> = CpuDirective;
    type State = CpuVariant;

    const WORD_ORDER: ByteOrder = ByteOrder::Little;

    fn parse_instruction<'a>(&self, parser: &mut Parser<'a>) -> Result<Instruction<'a>, Error> {
        let token = parser.advance().unwrap();
        let location = token.location;
        let name = symbol_name(&token);
        let mnemonic = mnemonic_from_str(&name).ok_or_else(|| {
            Error::from(
                location.line,
                location.column,
                &format!("unknown mnemonic '{name}'"),
            )
        })?;
        // The Wdc65C02 superset table disambiguates operand *syntax* only
        // (see the module docs).
        let syntax_table = &self.wdc;
        let accumulator_supported = syntax_table
            .get(mnemonic, AddressingMode::Accumulator)
            .is_some();
        let zero_page_relative_supported = syntax_table
            .get(mnemonic, AddressingMode::ZeroPageRelative)
            .is_some();
        let operand = parse_operand(parser, accumulator_supported, zero_page_relative_supported)?;
        Ok((mnemonic, operand))
    }

    fn parse_directive<'a>(
        &self,
        name: &str,
        location: Location,
        parser: &mut Parser<'a>,
    ) -> Option<Result<CpuDirective, Error>> {
        (name == "setcpu").then(|| parse_cpu_variant(parser, location).map(CpuDirective::SetCpu))
    }

    fn initial_state(&self) -> CpuVariant {
        CpuVariant::Cmos65C02
    }

    fn apply_directive(
        &self,
        directive: &CpuDirective,
        state: &mut CpuVariant,
        _symbols: &HashMap<String, Operand>,
        _location: Location,
    ) -> Result<(), Error> {
        let CpuDirective::SetCpu(variant) = directive;
        *state = *variant;
        Ok(())
    }

    fn encode(
        &self,
        (mnemonic, operand): &Instruction<'_>,
        address: u16,
        symbols: &HashMap<String, Operand>,
        state: &CpuVariant,
        location: Location,
    ) -> Result<Encoded, Error> {
        let encoded = encode(
            *mnemonic,
            operand,
            address,
            symbols,
            self.table(*state),
            location,
        )?;
        Ok(Encoded {
            byte_len: encoded.byte_len,
            bytes: encoded.bytes,
        })
    }
}

/// Parses `.setcpu`'s quoted string argument and maps it to a [`CpuVariant`],
/// accepting the aliases `"65c02"`/`"c02"` for [`CpuVariant::Cmos65C02`] and
/// `"wdc65c02"`/`"w65c02"` for [`CpuVariant::Wdc65C02`], case-insensitively.
fn parse_cpu_variant(
    parser: &mut Parser,
    directive_location: Location,
) -> Result<CpuVariant, Error> {
    match parser.peek().cloned() {
        Some(token) => match token.token_type() {
            TokenType::String(s) => {
                let variant = cpu_variant_from_str(s).ok_or_else(|| {
                    Error::from(
                        token.location.line,
                        token.location.column,
                        &format!("unknown CPU variant '{s}'"),
                    )
                })?;
                parser.advance();
                Ok(variant)
            }
            _ => Err(Error::from(
                token.location.line,
                token.location.column,
                "expected a quoted CPU variant string",
            )),
        },
        None => Err(Error::from(
            directive_location.line,
            directive_location.column,
            "expected a quoted CPU variant string",
        )),
    }
}

fn cpu_variant_from_str(s: &str) -> Option<CpuVariant> {
    match s.to_ascii_lowercase().as_str() {
        "65c02" | "c02" => Some(CpuVariant::Cmos65C02),
        "wdc65c02" | "w65c02" => Some(CpuVariant::Wdc65C02),
        _ => None,
    }
}
