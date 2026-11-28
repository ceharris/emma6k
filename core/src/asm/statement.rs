//! Line-oriented statement grammar: labels, symbol assignment, directives,
//! and instructions, plus `parse_program`, which turns a whole source string
//! into a flat, ordered list of statements for the driver to lay out and
//! encode.
//!
//! A source line is one of:
//! - blank (only a comment and/or newline — the scanner already strips
//!   comments, so this just means "nothing before the newline")
//! - `IDENT '=' expr` or `IDENT '.equ' expr` — symbol assignment, occupies
//!   the whole line
//! - `IDENT ':'` — a label, optionally followed by a directive or
//!   instruction on the same line (`my_routine:  LDA #$55`)
//! - a directive: one of the shared `.org`/`.byte`/`.word`/`.res`, or one
//!   the [`Target`] defines (the 6502's `.setcpu`)
//! - an instruction: a symbol token, after which the [`Target`] parses the
//!   mnemonic and its operand syntax

use super::Target;
use super::error::Error;
use super::expr::Expr;
use super::parser::Parser;
use super::token::{Token, TokenType};
use crate::location::Location;

/// A `.byte` operand: either a numeric expression (truncated to one byte) or
/// a string literal (each character emitted as one byte).
#[derive(Clone, Debug, PartialEq)]
pub enum ByteOperand<'a> {
    Expr(Expr<'a>),
    String(String),
}

/// A directive and its operand(s), independent of any preceding label.
/// `D` is the target's own directive type (see [`Target::Directive`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Directive<'a, D> {
    /// `.org expr` — starts a new output segment at `expr`.
    Org(Expr<'a>),
    /// `.byte op, op, ...` — emits one byte per numeric operand, or one byte
    /// per character for a string operand.
    Byte(Vec<ByteOperand<'a>>),
    /// `.word expr, expr, ...` — emits each operand as two bytes, in the
    /// target's byte order ([`Target::WORD_ORDER`]).
    Word(Vec<Expr<'a>>),
    /// `.res expr` — reserves `expr` zero bytes.
    Res(Expr<'a>),
    /// A directive the target defines, applied by
    /// [`Target::apply_directive`] when the driver reaches it.
    Target(D),
}

/// One parsed statement. A single source line can produce up to two of
/// these (a label followed by a directive or instruction). `I` and `D` are
/// the target's instruction and directive types.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement<'a, I, D> {
    Label(String),
    SymbolAssign(String, Expr<'a>),
    Directive(Directive<'a, D>),
    Instruction(I),
}

/// The statement type `parse_program` produces for target `T`.
pub type TargetStatement<'a, T> =
    Statement<'a, <T as Target>::Instruction<'a>, <T as Target>::Directive<'a>>;

/// Parses the entire `source` into a flat, ordered list of statements, each
/// paired with the source location of its first token.
pub fn parse_program<'a, T: Target>(
    target: &T,
    source: &'a str,
) -> Result<Vec<(TargetStatement<'a, T>, Location)>, Error> {
    let mut parser = Parser::new(source)?;
    let mut statements = Vec::new();
    while !parser.is_at_end() {
        statements.extend(parse_line(target, &mut parser)?);
    }
    Ok(statements)
}

/// Parses one source line (through its trailing newline, if any) into zero,
/// one, or two statements.
fn parse_line<'a, T: Target>(
    target: &T,
    parser: &mut Parser<'a>,
) -> Result<Vec<(TargetStatement<'a, T>, Location)>, Error> {
    let mut statements = Vec::new();

    if is_end_of_line(parser) {
        consume_newline(parser);
        return Ok(statements);
    }

    if is_symbol_assign_form(parser) {
        statements.push(parse_symbol_assign(parser)?);
        expect_end_of_line(parser)?;
        return Ok(statements);
    }

    if is_label_form(parser) {
        statements.push(parse_label(parser)?);
        if is_end_of_line(parser) {
            expect_end_of_line(parser)?;
            return Ok(statements);
        }
    }

    match parser.peek() {
        Some(token) if matches!(token.token_type(), TokenType::Directive(_)) => {
            statements.push(parse_directive_statement(target, parser)?)
        }
        Some(token) if matches!(token.token_type(), TokenType::Symbol(_)) => {
            let location = token.location;
            let instruction = target.parse_instruction(parser)?;
            statements.push((Statement::Instruction(instruction), location));
        }
        _ => return Err(unexpected_token_error(parser)),
    }

    expect_end_of_line(parser)?;
    Ok(statements)
}

fn is_symbol_assign_form(parser: &Parser) -> bool {
    if !matches!(
        parser.peek().map(|t| t.token_type()),
        Some(TokenType::Symbol(_))
    ) {
        return false;
    }
    match parser.peek_at(1).map(|t| t.token_type()) {
        Some(TokenType::Equal) => true,
        Some(TokenType::Directive(name)) => name.eq_ignore_ascii_case("equ"),
        _ => false,
    }
}

fn is_label_form(parser: &Parser) -> bool {
    matches!(
        parser.peek().map(|t| t.token_type()),
        Some(TokenType::Symbol(_))
    ) && matches!(
        parser.peek_at(1).map(|t| t.token_type()),
        Some(TokenType::Colon)
    )
}

/// The name a symbol token carries. Panics if `token` isn't a symbol, so a
/// caller must have checked its type first.
pub fn symbol_name(token: &Token) -> String {
    match token.token_type() {
        TokenType::Symbol(name) => name.clone(),
        _ => unreachable!("caller guaranteed a Symbol token"),
    }
}

fn parse_label<'a, I, D>(
    parser: &mut Parser<'a>,
) -> Result<(Statement<'a, I, D>, Location), Error> {
    let name_token = parser.advance().unwrap();
    let location = name_token.location;
    let name = symbol_name(&name_token);
    parser.advance(); // ':'
    Ok((Statement::Label(name), location))
}

fn parse_symbol_assign<'a, I, D>(
    parser: &mut Parser<'a>,
) -> Result<(Statement<'a, I, D>, Location), Error> {
    let name_token = parser.advance().unwrap();
    let location = name_token.location;
    let name = symbol_name(&name_token);
    parser.advance(); // '=' or '.equ'
    let expr = parser.parse_expr()?;
    Ok((Statement::SymbolAssign(name, expr), location))
}

fn parse_directive_statement<'a, T: Target>(
    target: &T,
    parser: &mut Parser<'a>,
) -> Result<(TargetStatement<'a, T>, Location), Error> {
    let token = parser.advance().unwrap();
    let location = token.location;
    let name = match token.token_type() {
        TokenType::Directive(name) => name.clone(),
        _ => unreachable!("caller guaranteed a Directive token"),
    };
    let name = name.to_ascii_lowercase();
    let directive = match name.as_str() {
        "org" => Directive::Org(parser.parse_expr()?),
        "byte" => Directive::Byte(parse_byte_operands(parser)?),
        "word" => Directive::Word(parse_expr_list(parser)?),
        "res" => Directive::Res(parser.parse_expr()?),
        other => match target.parse_directive(other, location, parser) {
            Some(directive) => Directive::Target(directive?),
            None => {
                return Err(Error::from(
                    location.line,
                    location.column,
                    &format!("unknown directive '.{other}'"),
                ));
            }
        },
    };
    Ok((Statement::Directive(directive), location))
}

fn parse_byte_operands<'a>(parser: &mut Parser<'a>) -> Result<Vec<ByteOperand<'a>>, Error> {
    let mut operands = vec![parse_byte_operand(parser)?];
    while let Some(token) = parser.peek()
        && token.token_type() == TokenType::Comma
    {
        parser.advance();
        operands.push(parse_byte_operand(parser)?);
    }
    Ok(operands)
}

fn parse_byte_operand<'a>(parser: &mut Parser<'a>) -> Result<ByteOperand<'a>, Error> {
    if let Some(token) = parser.peek()
        && let TokenType::String(s) = token.token_type()
    {
        let s = s.clone();
        parser.advance();
        return Ok(ByteOperand::String(s));
    }
    Ok(ByteOperand::Expr(parser.parse_expr()?))
}

fn parse_expr_list<'a>(parser: &mut Parser<'a>) -> Result<Vec<Expr<'a>>, Error> {
    let mut exprs = vec![parser.parse_expr()?];
    while let Some(token) = parser.peek()
        && token.token_type() == TokenType::Comma
    {
        parser.advance();
        exprs.push(parser.parse_expr()?);
    }
    Ok(exprs)
}

fn is_end_of_line(parser: &Parser) -> bool {
    match parser.peek() {
        None => true,
        Some(token) => token.token_type() == TokenType::Newline,
    }
}

fn consume_newline(parser: &mut Parser) {
    if let Some(token) = parser.peek()
        && token.token_type() == TokenType::Newline
    {
        parser.advance();
    }
}

fn expect_end_of_line(parser: &mut Parser) -> Result<(), Error> {
    if is_end_of_line(parser) {
        consume_newline(parser);
        Ok(())
    } else {
        let token = parser.peek().unwrap();
        Err(Error::from(
            token.location.line,
            token.location.column,
            "expected end of line",
        ))
    }
}

fn unexpected_token_error(parser: &Parser) -> Error {
    match parser.peek() {
        Some(token) => Error::from(
            token.location.line,
            token.location.column,
            "unexpected token",
        ),
        None => Error::from(0, 0, "unexpected end of input"),
    }
}
