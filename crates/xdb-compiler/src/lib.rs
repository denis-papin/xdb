//! PL/SQL subset -> xdb-ir compiler.
//!
//! A hand-written lexer ([`lexer`]) feeds a recursive-descent parser ([`parser`])
//! that lowers the script directly to an [`xdb_ir::Program`]: `CREATE TABLE` /
//! `CREATE SEQUENCE` build the schema (ids in declaration order) and every
//! `CREATE PROCEDURE` becomes an [`xdb_ir::Procedure`].
use std::fmt;

use xdb_ir::Program;

mod lexer;
mod parser;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    pub line: u32,
    pub col: u32,
    pub msg: String,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.msg)
    }
}

impl std::error::Error for CompileError {}

/// Compiles a script (tables, sequences, procedures) to the IR.
pub fn compile(src: &str) -> Result<Program, CompileError> {
    let toks = lexer::lex(src)?;
    parser::Parser::new(toks).program()
}

#[cfg(test)]
mod tests;
