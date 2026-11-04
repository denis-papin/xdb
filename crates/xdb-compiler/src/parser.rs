//! Recursive-descent parser that lowers the PL/SQL subset straight to `xdb_ir`.
//!
//! There is no separate AST: the parser keeps a symbol table (scopes of VarIds)
//! and emits IR statements as it goes. Expressions that need a preceding
//! statement (`seq.NEXTVAL`) push it to `pending`; every statement parser takes
//! the pending statements right after parsing its expressions and places them
//! where the expression is evaluated.

use std::collections::{HashMap, HashSet};

use xdb_ir::{
    errcode, AggFunc, BinOp, ColId, ColType, ColumnDef, Expr, Handler, PkRange, Procedure, Program,
    Schema, SequenceDef, Stmt, TableDef, TableId, UnOp, Value, VarDecl, VarId, VarType,
};

use crate::lexer::{Tok, Token};
use crate::CompileError;

type Res<T> = Result<T, CompileError>;

/// Placeholder row variable used while parsing single-row / aggregate WHERE clauses.
const SENTINEL: VarId = u32::MAX;
/// Maximum nesting depth of expressions and statements (avoids stack overflow).
const MAX_DEPTH: u32 = 100;
const PK_ONLY: &str = "only primary-key equality (WHERE <pk> = expr) is supported in this POC";

/// Words that can never start or be an operand of an expression.
const RESERVED: &[&str] = &[
    "then", "loop", "end", "and", "or", "from", "into", "where", "begin", "else", "elsif", "when",
    "is", "as", "in", "set", "values", "exception", "declare", "by", "order", "select",
];

/// How bare column names are resolved inside an expression.
#[derive(Clone)]
enum ColCtx {
    /// PL/SQL context: no columns.
    None,
    /// `UPDATE ... SET` right-hand side: columns of the updated row (`OldCol`).
    Old { table: TableId, alias: Option<String> },
    /// SQL WHERE: columns of `var`, a row of `table`.
    Row { table: TableId, var: VarId, alias: Option<String> },
}

#[derive(Default)]
struct ProcCtx {
    vars: Vec<VarDecl>,
    scopes: Vec<HashMap<String, VarId>>,
    readonly: HashSet<VarId>,
    pending: Vec<Stmt>,
    loop_depth: u32,
    rowcount_var: Option<VarId>,
    /// Stack of enclosing handlers: `Some((code, name))` for a single named exception.
    handlers: Vec<Option<(i32, String)>>,
    tmp_counter: u32,
}

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
    schema: Schema,
    procs: Vec<Procedure>,
    cur: ProcCtx,
    depth: u32,
}

fn bin(op: BinOp, a: Expr, b: Expr) -> Expr {
    Expr::Binary(op, Box::new(a), Box::new(b))
}

fn int(i: i64) -> Expr {
    Expr::Const(Value::Int(i))
}

/// `e + d`, folded when `e` is an integer constant.
fn add_const(e: Expr, d: i64) -> Expr {
    if let Expr::Const(Value::Int(i)) = e {
        if let Some(v) = i.checked_add(d) {
            return int(v);
        }
    }
    if d >= 0 {
        bin(BinOp::Add, e, int(d))
    } else {
        bin(BinOp::Sub, e, int(-d))
    }
}

/// Exit condition that is true when `c` is false *or NULL* (WHILE semantics).
fn not_true(c: Expr) -> Expr {
    Expr::Unary(UnOp::Not, Box::new(Expr::Coalesce(vec![c, int(0)])))
}

fn refs_row(e: &Expr, var: VarId) -> bool {
    match e {
        Expr::Col(v, _) => *v == var,
        Expr::Const(_) | Expr::Var(_) | Expr::OldCol(_) => false,
        Expr::Unary(_, a) | Expr::IsNull(a) => refs_row(a, var),
        Expr::Binary(_, a, b) => refs_row(a, var) || refs_row(b, var),
        Expr::Coalesce(v) => v.iter().any(|x| refs_row(x, var)),
    }
}

fn conjuncts(e: Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::Binary(BinOp::And, a, b) => {
            conjuncts(*a, out);
            conjuncts(*b, out);
        }
        other => out.push(other),
    }
}

fn flip(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        o => o,
    }
}

/// If `c` is `pk <op> e` (or mirrored) with `e` independent of the row,
/// returns the normalized operator and `e`.
fn pk_comparison(c: &Expr, var: VarId, pk: ColId) -> Option<(BinOp, Expr)> {
    if let Expr::Binary(op, a, b) = c {
        if !matches!(op, BinOp::Eq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) {
            return None;
        }
        let is_pk = |x: &Expr| matches!(x, Expr::Col(v, col) if *v == var && *col == pk);
        if is_pk(a) && !refs_row(b, var) {
            return Some((*op, (**b).clone()));
        }
        if is_pk(b) && !refs_row(a, var) {
            return Some((flip(*op), (**a).clone()));
        }
    }
    None
}

fn named_exception(name: &str) -> Option<i32> {
    Some(match name {
        "no_data_found" => errcode::NO_DATA_FOUND,
        "dup_val_on_index" => errcode::DUP_VAL_ON_INDEX,
        "zero_divide" => errcode::ZERO_DIVIDE,
        "value_error" => errcode::VALUE_ERROR,
        _ => return None,
    })
}

enum SelItem {
    Star,
    Col(String, (u32, u32)),
    Sum(String, (u32, u32)),
    Count,
}

impl Parser {
    pub fn new(toks: Vec<Token>) -> Self {
        Parser { toks, pos: 0, schema: Schema::default(), procs: Vec::new(), cur: ProcCtx::default(), depth: 0 }
    }

    // ------------------------------------------------------------------
    // Token helpers
    // ------------------------------------------------------------------

    fn tok_at(&self, n: usize) -> &Token {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i]
    }
    fn peek(&self) -> &Tok {
        &self.tok_at(0).tok
    }
    fn peek_at(&self, n: usize) -> &Tok {
        &self.tok_at(n).tok
    }
    fn here(&self) -> (u32, u32) {
        let t = self.tok_at(0);
        (t.line, t.col)
    }
    fn advance(&mut self) -> Token {
        let t = self.tok_at(0).clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn err_at(&self, (line, col): (u32, u32), msg: impl Into<String>) -> CompileError {
        CompileError { line, col, msg: msg.into() }
    }
    fn err_here(&self, msg: impl Into<String>) -> CompileError {
        self.err_at(self.here(), msg)
    }
    fn unexpected(&self, what: &str) -> CompileError {
        self.err_here(format!("expected {what}, found {}", self.peek().describe()))
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s == kw)
    }
    fn is_kw_at(&self, n: usize, kw: &str) -> bool {
        matches!(self.peek_at(n), Tok::Ident(s) if s == kw)
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn expect_kw(&mut self, kw: &str) -> Res<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.unexpected(&kw.to_uppercase()))
        }
    }
    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.advance();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: Tok) -> Res<()> {
        if self.eat(&t) {
            Ok(())
        } else {
            Err(self.unexpected(&t.describe()))
        }
    }
    fn ident(&mut self, what: &str) -> Res<String> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                self.advance();
                Ok(s)
            }
            _ => Err(self.unexpected(what)),
        }
    }
    fn semi(&mut self) -> Res<()> {
        self.expect(Tok::Semi)
    }
    fn enter(&mut self) -> Res<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err_here("nesting too deep"));
        }
        Ok(())
    }
    fn leave(&mut self) {
        self.depth -= 1;
    }

    // ------------------------------------------------------------------
    // Script level
    // ------------------------------------------------------------------

    pub fn program(mut self) -> Res<Program> {
        loop {
            match self.peek() {
                Tok::Eof => break,
                Tok::Slash | Tok::Semi => {
                    self.advance();
                    continue;
                }
                _ => {}
            }
            if !self.eat_kw("create") {
                return Err(self.unexpected("CREATE TABLE, CREATE SEQUENCE or CREATE PROCEDURE"));
            }
            if self.eat_kw("or") {
                self.expect_kw("replace")?;
            }
            if self.is_kw("table") {
                self.create_table()?;
            } else if self.is_kw("sequence") {
                self.create_sequence()?;
            } else if self.is_kw("procedure") {
                self.create_procedure()?;
            } else {
                let what = self.peek().describe();
                return Err(self.err_here(format!(
                    "unsupported CREATE {what}: only TABLE, SEQUENCE and PROCEDURE are supported"
                )));
            }
        }
        Ok(Program { schema: self.schema, procedures: self.procs })
    }

    fn column_type(&mut self) -> Res<ColType> {
        let pos = self.here();
        let name = self.ident("a column type")?;
        match name.as_str() {
            "number" => {
                if self.eat(&Tok::LParen) {
                    self.int_lit()?;
                    if self.eat(&Tok::Comma) {
                        let s = self.int_lit()?;
                        if s != 0 {
                            return Err(self.err_at(pos, "NUMBER with a non-zero scale is not supported"));
                        }
                    }
                    self.expect(Tok::RParen)?;
                }
                Ok(ColType::Number)
            }
            "integer" | "int" | "pls_integer" | "binary_integer" | "boolean" => Ok(ColType::Number),
            "varchar2" | "varchar" => {
                let mut n = 4000;
                if self.eat(&Tok::LParen) {
                    let v = self.int_lit()?;
                    n = u32::try_from(v).map_err(|_| self.err_at(pos, "invalid VARCHAR2 length"))?;
                    let _ = self.eat_kw("byte") || self.eat_kw("char");
                    self.expect(Tok::RParen)?;
                }
                Ok(ColType::Varchar(n))
            }
            other => Err(self.err_at(pos, format!("unsupported type '{other}'"))),
        }
    }

    fn int_lit(&mut self) -> Res<i64> {
        let neg = self.eat(&Tok::Minus);
        match *self.peek() {
            Tok::Int(i) => {
                self.advance();
                Ok(if neg { -i } else { i })
            }
            _ => Err(self.unexpected("an integer literal")),
        }
    }

    fn create_table(&mut self) -> Res<()> {
        self.advance(); // TABLE
        let pos = self.here();
        let name = self.ident("a table name")?;
        if self.schema.table(&name).is_some() {
            return Err(self.err_at(pos, format!("table '{name}' already exists")));
        }
        self.expect(Tok::LParen)?;
        let mut columns: Vec<ColumnDef> = Vec::new();
        let mut pk: Option<ColId> = None;
        let check_pk = |p: &Parser, ty: ColType, pos| {
            if ty != ColType::Number {
                Err(p.err_at(pos, "the PRIMARY KEY column must be NUMBER or INTEGER"))
            } else {
                Ok(())
            }
        };
        loop {
            let cpos = self.here();
            if self.eat_kw("primary") {
                // table-level PRIMARY KEY (col)
                self.expect_kw("key")?;
                self.expect(Tok::LParen)?;
                let cpos = self.here();
                let c = self.ident("a column name")?;
                self.expect(Tok::RParen)?;
                let idx = columns
                    .iter()
                    .position(|d| d.name == c)
                    .ok_or_else(|| self.err_at(cpos, format!("unknown column '{c}'")))?;
                if pk.is_some() {
                    return Err(self.err_at(cpos, "more than one PRIMARY KEY"));
                }
                check_pk(self, columns[idx].ty, cpos)?;
                pk = Some(idx as ColId);
            } else {
                let cname = self.ident("a column name")?;
                if columns.iter().any(|d| d.name == cname) {
                    return Err(self.err_at(cpos, format!("duplicate column '{cname}'")));
                }
                let ty = self.column_type()?;
                loop {
                    let kpos = self.here();
                    if self.eat_kw("primary") {
                        self.expect_kw("key")?;
                        if pk.is_some() {
                            return Err(self.err_at(kpos, "more than one PRIMARY KEY"));
                        }
                        check_pk(self, ty, kpos)?;
                        pk = Some(columns.len() as ColId);
                    } else if self.eat_kw("not") {
                        self.expect_kw("null")?;
                    } else if self.eat_kw("null") {
                    } else {
                        break;
                    }
                }
                if columns.len() >= ColId::MAX as usize {
                    return Err(self.err_at(cpos, "too many columns"));
                }
                columns.push(ColumnDef { name: cname, ty });
            }
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect(Tok::RParen)?;
            break;
        }
        let pk_col = pk.ok_or_else(|| self.err_at(pos, format!("table '{name}' needs exactly one PRIMARY KEY column")))?;
        self.semi()?;
        let id = self.schema.tables.len() as TableId;
        self.schema.tables.push(TableDef { id, name, columns, pk_col });
        Ok(())
    }

    fn create_sequence(&mut self) -> Res<()> {
        self.advance(); // SEQUENCE
        let pos = self.here();
        let name = self.ident("a sequence name")?;
        if self.schema.sequence(&name).is_some() {
            return Err(self.err_at(pos, format!("sequence '{name}' already exists")));
        }
        // Options (START WITH, INCREMENT BY, CACHE ...) are ignored.
        while !matches!(self.peek(), Tok::Semi | Tok::Eof) {
            self.advance();
        }
        self.semi()?;
        let id = self.schema.sequences.len() as u32;
        self.schema.sequences.push(SequenceDef { id, name });
        Ok(())
    }

    /// Does the procedure starting at the current position use `SQL%ROWCOUNT`?
    fn prescan_rowcount(&self) -> bool {
        let mut i = self.pos;
        while i + 2 < self.toks.len() {
            if i > self.pos
                && matches!(&self.toks[i].tok, Tok::Ident(s) if s == "create")
                && matches!(self.toks[i - 1].tok, Tok::Semi | Tok::Slash)
            {
                return false;
            }
            if matches!(&self.toks[i].tok, Tok::Ident(s) if s == "sql")
                && self.toks[i + 1].tok == Tok::Percent
                && matches!(&self.toks[i + 2].tok, Tok::Ident(s) if s == "rowcount")
            {
                return true;
            }
            i += 1;
        }
        false
    }

    fn create_procedure(&mut self) -> Res<()> {
        self.advance(); // PROCEDURE
        let pos = self.here();
        let name = self.ident("a procedure name")?;
        if self.procs.iter().any(|p| p.name == name) {
            return Err(self.err_at(pos, format!("procedure '{name}' is defined twice")));
        }
        self.cur = ProcCtx::default();
        self.cur.scopes.push(HashMap::new());
        if self.prescan_rowcount() {
            let v = self.new_var("sql%rowcount".into(), VarType::Scalar);
            self.cur.rowcount_var = Some(v);
        }
        let mut params = Vec::new();
        if self.eat(&Tok::LParen) && !self.eat(&Tok::RParen) {
            loop {
                let ppos = self.here();
                let pname = self.ident("a parameter name")?;
                self.eat_kw("in");
                if self.is_kw("out") {
                    return Err(self.err_here("OUT / IN OUT parameters are not supported"));
                }
                let tpos = self.here();
                if self.decl_type()? != VarType::Scalar {
                    return Err(self.err_at(tpos, "record (%ROWTYPE) parameters are not supported"));
                }
                if self.is_kw("default") || *self.peek() == Tok::Assign {
                    return Err(self.err_here("parameter default values are not supported"));
                }
                let id = self.declare(pname, VarType::Scalar, ppos)?;
                self.cur.readonly.insert(id);
                params.push(id);
                if self.eat(&Tok::Comma) {
                    continue;
                }
                self.expect(Tok::RParen)?;
                break;
            }
        }
        if !self.eat_kw("is") && !self.eat_kw("as") {
            return Err(self.unexpected("IS or AS"));
        }
        let mut body = self.decls(true)?;
        self.expect_kw("begin")?;
        body.extend(self.stmts_until(&["exception", "end"])?);
        let handlers = if self.eat_kw("exception") { self.handlers()? } else { Vec::new() };
        self.expect_kw("end")?;
        if let Tok::Ident(s) = self.peek().clone() {
            if s != name {
                return Err(self.err_here(format!("END {s} does not match procedure name '{name}'")));
            }
            self.advance();
        }
        self.semi()?;
        let ctx = std::mem::take(&mut self.cur);
        self.procs.push(Procedure { name, params, vars: ctx.vars, body, handlers });
        Ok(())
    }

    // ------------------------------------------------------------------
    // Variables and declarations
    // ------------------------------------------------------------------

    fn new_var(&mut self, name: String, ty: VarType) -> VarId {
        let id = self.cur.vars.len() as VarId;
        self.cur.vars.push(VarDecl { id, name, ty, init: None });
        id
    }

    fn temp(&mut self, prefix: &str, ty: VarType) -> VarId {
        self.cur.tmp_counter += 1;
        let n = format!("${prefix}{}", self.cur.tmp_counter);
        self.new_var(n, ty)
    }

    fn declare(&mut self, name: String, ty: VarType, pos: (u32, u32)) -> Res<VarId> {
        if self.cur.scopes.last().is_some_and(|s| s.contains_key(&name)) {
            return Err(self.err_at(pos, format!("'{name}' is declared twice in the same block")));
        }
        let id = self.new_var(name.clone(), ty);
        if let Some(s) = self.cur.scopes.last_mut() {
            s.insert(name, id);
        }
        Ok(id)
    }

    fn lookup(&self, name: &str) -> Option<VarId> {
        self.cur.scopes.iter().rev().find_map(|s| s.get(name).copied())
    }

    fn var_ty(&self, id: VarId) -> &VarType {
        &self.cur.vars[id as usize].ty
    }

    fn table_id(&self, name: &str, pos: (u32, u32)) -> Res<TableId> {
        self.schema
            .table(name)
            .map(|t| t.id)
            .ok_or_else(|| self.err_at(pos, format!("unknown table '{name}'")))
    }

    fn tdef(&self, id: TableId) -> &TableDef {
        &self.schema.tables[id as usize]
    }

    fn column(&self, table: TableId, col: &str, pos: (u32, u32)) -> Res<ColId> {
        let t = self.tdef(table);
        t.col(col).ok_or_else(|| self.err_at(pos, format!("table '{}' has no column '{col}'", t.name)))
    }

    /// Parses a declaration / parameter type. Returns the variable type.
    fn decl_type(&mut self) -> Res<VarType> {
        let pos = self.here();
        let name = self.ident("a type")?;
        if *self.peek() == Tok::Percent {
            self.advance();
            let apos = self.here();
            let attr = self.ident("ROWTYPE or TYPE")?;
            return match attr.as_str() {
                "rowtype" => Ok(VarType::Row(self.table_id(&name, pos)?)),
                "type" => match self.lookup(&name) {
                    Some(v) if *self.var_ty(v) == VarType::Scalar => Ok(VarType::Scalar),
                    _ => Err(self.err_at(pos, format!("unknown variable '{name}' in %TYPE"))),
                },
                _ => Err(self.err_at(apos, format!("unsupported attribute %{attr}"))),
            };
        }
        if *self.peek() == Tok::Dot {
            self.advance();
            let cpos = self.here();
            let col = self.ident("a column name")?;
            self.expect(Tok::Percent)?;
            self.expect_kw("type")?;
            let t = self.table_id(&name, pos)?;
            self.column(t, &col, cpos)?;
            return Ok(VarType::Scalar);
        }
        self.pos -= 1;
        self.column_type()?;
        Ok(VarType::Scalar)
    }

    /// Declaration section up to BEGIN. Returns the statements that initialise
    /// the declared variables (nested blocks, or initialisers needing a statement).
    fn decls(&mut self, top: bool) -> Res<Vec<Stmt>> {
        let mut out = Vec::new();
        let mut as_stmts = !top;
        while !self.is_kw("begin") {
            let pos = self.here();
            let name = self.ident("a declaration or BEGIN")?;
            if matches!(name.as_str(), "cursor" | "type" | "subtype" | "pragma" | "procedure" | "function")
                && matches!(self.peek(), Tok::Ident(_))
            {
                return Err(self.err_at(pos, format!("{} declarations are not supported", name.to_uppercase())));
            }
            self.eat_kw("constant");
            if self.is_kw("exception") {
                return Err(self.err_here("user-defined exceptions are not supported"));
            }
            let ty = self.decl_type()?;
            if self.eat_kw("not") {
                self.expect_kw("null")?;
            }
            let init = if self.eat(&Tok::Assign) || self.eat_kw("default") {
                if ty != VarType::Scalar {
                    return Err(self.err_at(pos, "record variables cannot have an initial value"));
                }
                Some(self.expr(&ColCtx::None)?)
            } else {
                None
            };
            self.semi()?;
            let id = self.declare(name, ty.clone(), pos)?;
            if !self.cur.pending.is_empty() {
                as_stmts = true;
            }
            if as_stmts {
                out.append(&mut self.cur.pending);
                if ty == VarType::Scalar {
                    out.push(Stmt::Assign { var: id, expr: init.unwrap_or(Expr::Const(Value::Null)) });
                }
            } else {
                self.cur.vars[id as usize].init = init;
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // Statements
    // ------------------------------------------------------------------

    fn take_pending(&mut self) -> Vec<Stmt> {
        std::mem::take(&mut self.cur.pending)
    }

    fn stmts_until(&mut self, terms: &[&str]) -> Res<Vec<Stmt>> {
        let mut out = Vec::new();
        while !terms.iter().any(|t| self.is_kw(t)) {
            if *self.peek() == Tok::Eof {
                return Err(self.err_here("unexpected end of input (missing END?)"));
            }
            self.enter()?;
            let r = self.stmt(&mut out);
            self.leave();
            r?;
        }
        Ok(out)
    }

    fn stmt(&mut self, out: &mut Vec<Stmt>) -> Res<()> {
        let pos = self.here();
        let kw = match self.peek() {
            Tok::Ident(s) => s.clone(),
            _ => return Err(self.unexpected("a statement")),
        };
        match kw.as_str() {
            "null" => {
                self.advance();
                self.semi()?;
                out.push(Stmt::Nop);
            }
            "if" => {
                self.advance();
                let s = self.if_rest()?;
                out.extend(s);
                self.expect_kw("end")?;
                self.expect_kw("if")?;
                self.semi()?;
            }
            "loop" => {
                self.advance();
                let body = self.loop_body()?;
                out.push(Stmt::Loop { body });
            }
            "while" => {
                self.advance();
                let cond = self.expr(&ColCtx::None)?;
                let mut body = self.take_pending();
                body.push(Stmt::Exit { cond: Some(not_true(cond)) });
                self.expect_kw("loop")?;
                body.extend(self.loop_body()?);
                out.push(Stmt::Loop { body });
            }
            "for" => self.for_stmt(out)?,
            "exit" => {
                self.advance();
                if self.cur.loop_depth == 0 {
                    return Err(self.err_at(pos, "EXIT outside of a loop"));
                }
                let cond = if self.eat_kw("when") { Some(self.expr(&ColCtx::None)?) } else { None };
                if matches!(self.peek(), Tok::Ident(_)) {
                    return Err(self.err_here("EXIT with a label is not supported"));
                }
                self.semi()?;
                out.append(&mut self.cur.pending);
                out.push(Stmt::Exit { cond });
            }
            "select" => self.select_into(out)?,
            "update" => self.update(out)?,
            "insert" => self.insert(out)?,
            "delete" => self.delete(out)?,
            "raise" => self.raise(out)?,
            "raise_application_error" => self.raise_app_error(out)?,
            "commit" => {
                self.advance();
                self.eat_kw("work");
                self.semi()?;
                out.push(Stmt::Commit);
            }
            "rollback" => {
                self.advance();
                self.eat_kw("work");
                if self.is_kw("to") {
                    return Err(self.err_here("ROLLBACK TO SAVEPOINT is not supported"));
                }
                self.semi()?;
                out.push(Stmt::Rollback);
            }
            "dbms_output" => {
                self.advance();
                self.expect(Tok::Dot)?;
                let fpos = self.here();
                let f = self.ident("PUT_LINE")?;
                if f != "put_line" {
                    return Err(self.err_at(fpos, format!("DBMS_OUTPUT.{} is not supported", f.to_uppercase())));
                }
                self.expect(Tok::LParen)?;
                let e = self.expr(&ColCtx::None)?;
                self.expect(Tok::RParen)?;
                self.semi()?;
                out.append(&mut self.cur.pending);
                out.push(Stmt::Output(e));
            }
            "declare" | "begin" => self.block(out)?,
            "return" | "goto" | "continue" | "open" | "fetch" | "close" | "execute" | "case" | "merge"
            | "savepoint" | "lock" | "pipe" | "forall" => {
                return Err(self.err_at(pos, format!("{} statement is not supported", kw.to_uppercase())));
            }
            _ => self.assignment(out)?,
        }
        Ok(())
    }

    fn loop_body(&mut self) -> Res<Vec<Stmt>> {
        self.cur.loop_depth += 1;
        let body = self.stmts_until(&["end"]);
        self.cur.loop_depth -= 1;
        let body = body?;
        self.expect_kw("end")?;
        self.expect_kw("loop")?;
        if matches!(self.peek(), Tok::Ident(_)) {
            return Err(self.err_here("loop labels are not supported"));
        }
        self.semi()?;
        Ok(body)
    }

    /// After IF / ELSIF: `cond THEN stmts [ELSIF ...] [ELSE stmts]`, stops at END.
    fn if_rest(&mut self) -> Res<Vec<Stmt>> {
        self.enter()?;
        let r = self.if_rest_inner();
        self.leave();
        r
    }

    fn if_rest_inner(&mut self) -> Res<Vec<Stmt>> {
        let cond = self.expr(&ColCtx::None)?;
        let mut out = self.take_pending();
        self.expect_kw("then")?;
        let then_body = self.stmts_until(&["elsif", "else", "end"])?;
        let else_body = if self.eat_kw("elsif") {
            self.if_rest()?
        } else if self.eat_kw("else") {
            self.stmts_until(&["end"])?
        } else {
            Vec::new()
        };
        out.push(Stmt::If { cond, then_body, else_body });
        Ok(out)
    }

    fn assignment(&mut self, out: &mut Vec<Stmt>) -> Res<()> {
        let pos = self.here();
        let name = self.ident("a statement")?;
        if *self.peek() == Tok::Dot {
            self.advance();
            let field = self.ident("a field name")?;
            if *self.peek() == Tok::Assign {
                return Err(self.err_at(
                    pos,
                    format!("row field assignment ({name}.{field} := ...) is not supported: the IR has no field-assignment statement"),
                ));
            }
            return Err(self.err_at(pos, format!("unsupported statement starting with '{name}.{field}'")));
        }
        match self.peek() {
            Tok::Assign => {}
            Tok::LParen | Tok::Semi => {
                return Err(self.err_at(pos, format!("procedure calls are not supported ('{name}')")));
            }
            _ => return Err(self.err_at(pos, format!("unsupported statement starting with '{name}'"))),
        }
        self.advance();
        let var = self.writable_scalar(&name, pos)?;
        let expr = self.expr(&ColCtx::None)?;
        self.semi()?;
        out.append(&mut self.cur.pending);
        out.push(Stmt::Assign { var, expr });
        Ok(())
    }

    fn writable_scalar(&self, name: &str, pos: (u32, u32)) -> Res<VarId> {
        let var = self.lookup(name).ok_or_else(|| self.err_at(pos, format!("unknown variable '{name}'")))?;
        if *self.var_ty(var) != VarType::Scalar {
            return Err(self.err_at(pos, format!("cannot assign to record variable '{name}' as a whole")));
        }
        if self.cur.readonly.contains(&var) {
            return Err(self.err_at(pos, format!("'{name}' is an IN parameter or loop variable and cannot be assigned")));
        }
        Ok(var)
    }

    fn block(&mut self, out: &mut Vec<Stmt>) -> Res<()> {
        self.cur.scopes.push(HashMap::new());
        let r = self.block_inner(out);
        self.cur.scopes.pop();
        r
    }

    fn block_inner(&mut self, out: &mut Vec<Stmt>) -> Res<()> {
        let inits = if self.eat_kw("declare") { self.decls(false)? } else { Vec::new() };
        self.expect_kw("begin")?;
        let body = self.stmts_until(&["exception", "end"])?;
        let handlers = if self.eat_kw("exception") { self.handlers()? } else { Vec::new() };
        self.expect_kw("end")?;
        if matches!(self.peek(), Tok::Ident(_)) {
            self.advance(); // optional block label
        }
        self.semi()?;
        out.extend(inits);
        out.push(Stmt::Block { body, handlers });
        Ok(())
    }

    fn handlers(&mut self) -> Res<Vec<Handler>> {
        let mut hs = Vec::new();
        if !self.is_kw("when") {
            return Err(self.unexpected("WHEN"));
        }
        while self.eat_kw("when") {
            let mut codes = Vec::new();
            let mut names = Vec::new();
            loop {
                let pos = self.here();
                let n = self.ident("an exception name")?;
                let code = if n == "others" {
                    None
                } else {
                    Some(named_exception(&n).ok_or_else(|| {
                        self.err_at(pos, format!("unsupported exception '{n}' (supported: NO_DATA_FOUND, DUP_VAL_ON_INDEX, ZERO_DIVIDE, VALUE_ERROR, OTHERS)"))
                    })?)
                };
                codes.push(code);
                names.push(n);
                if !self.eat_kw("or") {
                    break;
                }
            }
            self.expect_kw("then")?;
            // `WHEN a OR b THEN body` becomes one handler per name. The body is
            // lowered once per name so that a bare RAISE re-raises that name.
            let start = self.pos;
            for (code, n) in codes.into_iter().zip(names) {
                self.pos = start;
                self.cur.handlers.push(code.map(|c| (c, n.to_uppercase())));
                let body = self.stmts_until(&["when", "end"]);
                self.cur.handlers.pop();
                hs.push(Handler { code, body: body? });
            }
        }
        Ok(hs)
    }

    fn raise(&mut self, out: &mut Vec<Stmt>) -> Res<()> {
        let pos = self.here();
        self.advance();
        if *self.peek() == Tok::Semi {
            self.advance();
            return match self.cur.handlers.last() {
                Some(Some((code, name))) => {
                    out.push(Stmt::Raise { code: *code, msg: name.clone() });
                    Ok(())
                }
                _ => Err(self.err_at(pos, "bare RAISE is only supported inside a WHEN <named exception> handler")),
            };
        }
        let npos = self.here();
        let n = self.ident("an exception name")?;
        let code = named_exception(&n)
            .ok_or_else(|| self.err_at(npos, format!("unsupported exception '{n}' in RAISE")))?;
        self.semi()?;
        out.push(Stmt::Raise { code, msg: n.to_uppercase() });
        Ok(())
    }

    fn raise_app_error(&mut self, out: &mut Vec<Stmt>) -> Res<()> {
        self.advance();
        self.expect(Tok::LParen)?;
        let cpos = self.here();
        let code = self.int_lit()?;
        if !(-20999..=-20000).contains(&code) {
            return Err(self.err_at(cpos, "RAISE_APPLICATION_ERROR code must be between -20999 and -20000"));
        }
        self.expect(Tok::Comma)?;
        let msg = match self.peek().clone() {
            Tok::Str(s) => {
                self.advance();
                s
            }
            _ => return Err(self.err_here("RAISE_APPLICATION_ERROR message must be a string literal")),
        };
        if self.eat(&Tok::Comma) && !self.eat_kw("true") && !self.eat_kw("false") {
            return Err(self.unexpected("TRUE or FALSE"));
        }
        self.expect(Tok::RParen)?;
        self.semi()?;
        out.push(Stmt::Raise { code: code as i32, msg });
        Ok(())
    }

    fn for_stmt(&mut self, out: &mut Vec<Stmt>) -> Res<()> {
        self.advance(); // FOR
        let vpos = self.here();
        let name = self.ident("a loop variable")?;
        self.expect_kw("in")?;
        if *self.peek() == Tok::LParen && self.is_kw_at(1, "select") {
            return self.cursor_for(name, out);
        }
        if matches!(self.peek(), Tok::Ident(_)) && self.is_kw_at(1, "loop") {
            return Err(self.err_here("cursor FOR loops over named cursors are not supported"));
        }
        let reverse = self.eat_kw("reverse");
        let lo = self.expr(&ColCtx::None)?;
        self.expect(Tok::DotDot)?;
        let hi = self.expr(&ColCtx::None)?;
        out.append(&mut self.cur.pending);
        self.expect_kw("loop")?;
        let (start, end, cmp, step) = if reverse { (hi, lo, BinOp::Lt, -1) } else { (lo, hi, BinOp::Gt, 1) };
        let bound = self.temp("for_bound", VarType::Scalar);
        self.cur.scopes.push(HashMap::new());
        let r = (|| {
            let ivar = self.declare(name, VarType::Scalar, vpos)?;
            self.cur.readonly.insert(ivar);
            let body = self.loop_body()?;
            Ok((ivar, body))
        })();
        self.cur.scopes.pop();
        let (ivar, body) = r?;
        out.push(Stmt::Assign { var: ivar, expr: start });
        out.push(Stmt::Assign { var: bound, expr: end });
        let mut lbody = vec![Stmt::Exit {
            cond: Some(Expr::Coalesce(vec![bin(cmp, Expr::Var(ivar), Expr::Var(bound)), int(1)])),
        }];
        lbody.extend(body);
        lbody.push(Stmt::Assign { var: ivar, expr: add_const(Expr::Var(ivar), step) });
        out.push(Stmt::Loop { body: lbody });
        Ok(())
    }

    /// Optional table alias after a table name in FROM / UPDATE / DELETE.
    fn alias(&mut self) -> Option<String> {
        const NOT_ALIAS: &[&str] = &["where", "order", "fetch", "for", "set", "values"];
        match self.peek().clone() {
            Tok::Ident(s) if !NOT_ALIAS.contains(&s.as_str()) => {
                self.advance();
                Some(s)
            }
            _ => None,
        }
    }

    /// Column name, optionally qualified by the table name or alias.
    fn col_ref(&mut self) -> Res<(String, (u32, u32))> {
        let pos = self.here();
        let mut c = self.ident("a column name")?;
        if self.eat(&Tok::Dot) {
            c = self.ident("a column name")?;
        }
        Ok((c, pos))
    }

    fn cursor_for(&mut self, name: String, out: &mut Vec<Stmt>) -> Res<()> {
        self.advance(); // (
        self.advance(); // SELECT
        let mut cols = Vec::new();
        if !self.eat(&Tok::Star) {
            loop {
                if matches!(self.peek(), Tok::Ident(_)) && *self.peek_at(1) == Tok::LParen {
                    return Err(self.err_here("functions and aggregates are not supported in a cursor FOR loop query"));
                }
                cols.push(self.col_ref()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect_kw("from")?;
        let tpos = self.here();
        let tname = self.ident("a table name")?;
        let table = self.table_id(&tname, tpos)?;
        if *self.peek() == Tok::Comma || self.is_kw("join") {
            return Err(self.err_here("joins are not supported"));
        }
        let alias = self.alias();
        for (c, p) in &cols {
            self.column(table, c, *p)?;
        }
        let pk = self.tdef(table).pk_col;
        let row_var = self.new_var(name.clone(), VarType::Row(table));
        let wpos = self.here();
        let cond = if self.eat_kw("where") {
            let before = self.cur.pending.len();
            let c = self.expr(&ColCtx::Row { table, var: row_var, alias: alias.clone() })?;
            if self.cur.pending.len() != before {
                return Err(self.err_at(wpos, "sequence NEXTVAL is not supported in a cursor WHERE clause"));
            }
            Some(c)
        } else {
            None
        };
        let (range, filter) = self.split_range(cond, row_var, pk, wpos)?;
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            let (c, p) = self.col_ref()?;
            if self.column(table, &c, p)? != pk {
                return Err(self.err_at(p, "ORDER BY is only supported on the primary key"));
            }
            self.eat_kw("asc");
            if self.is_kw("desc") {
                return Err(self.err_here("ORDER BY ... DESC is not supported"));
            }
        }
        let limit = if self.eat_kw("fetch") {
            if !self.eat_kw("first") && !self.eat_kw("next") {
                return Err(self.unexpected("FIRST"));
            }
            let n = self.expr(&ColCtx::None)?;
            if !self.eat_kw("rows") && !self.eat_kw("row") {
                return Err(self.unexpected("ROWS"));
            }
            self.expect_kw("only")?;
            Some(n)
        } else {
            None
        };
        let for_update = if self.eat_kw("for") {
            self.expect_kw("update")?;
            true
        } else {
            false
        };
        if !matches!(self.peek(), Tok::RParen) {
            return Err(self.err_here(format!(
                "unsupported clause {} in cursor query (supported: WHERE, ORDER BY pk, FETCH FIRST n ROWS ONLY, FOR UPDATE)",
                self.peek().describe()
            )));
        }
        self.advance();
        out.append(&mut self.cur.pending);
        self.expect_kw("loop")?;
        self.cur.scopes.push(HashMap::new());
        if let Some(s) = self.cur.scopes.last_mut() {
            s.insert(name, row_var);
        }
        let body = self.loop_body();
        self.cur.scopes.pop();
        let body = body?;
        out.push(Stmt::ForRows { table, range, filter, limit, for_update, row_var, body });
        Ok(())
    }

    /// Splits a WHERE condition into a primary-key range and the remaining filter.
    fn split_range(&self, cond: Option<Expr>, var: VarId, pk: ColId, pos: (u32, u32)) -> Res<(PkRange, Option<Expr>)> {
        let mut parts = Vec::new();
        if let Some(c) = cond {
            conjuncts(c, &mut parts);
        }
        let (mut lo, mut hi): (Option<Expr>, Option<Expr>) = (None, None);
        let mut rest: Vec<Expr> = Vec::new();
        for c in parts {
            let Some((op, e)) = pk_comparison(&c, var, pk) else {
                rest.push(c);
                continue;
            };
            let (l, h) = match op {
                BinOp::Eq => (Some(e.clone()), Some(e)),
                BinOp::Ge => (Some(e), None),
                BinOp::Gt => (Some(add_const(e, 1)), None),
                BinOp::Le => (None, Some(e)),
                BinOp::Lt => (None, Some(add_const(e, -1))),
                _ => unreachable!("pk_comparison only returns comparisons"),
            };
            if let Some(l) = l {
                if lo.replace(l).is_some() {
                    return Err(self.err_at(pos, "more than one lower bound on the primary key is not supported"));
                }
            }
            if let Some(h) = h {
                if hi.replace(h).is_some() {
                    return Err(self.err_at(pos, "more than one upper bound on the primary key is not supported"));
                }
            }
        }
        let filter = rest.into_iter().reduce(|a, b| bin(BinOp::And, a, b));
        let range = PkRange { lo: lo.unwrap_or(int(i64::MIN)), hi: hi.unwrap_or(int(i64::MAX)) };
        Ok((range, filter))
    }

    /// `WHERE pk = expr` for single-row statements. Returns the key expression.
    fn pk_eq_where(&mut self, table: TableId, alias: Option<String>) -> Res<Expr> {
        let wpos = self.here();
        if !self.eat_kw("where") {
            return Err(self.err_at(wpos, format!("missing WHERE clause: {PK_ONLY}")));
        }
        let cond = self.expr(&ColCtx::Row { table, var: SENTINEL, alias })?;
        let pk = self.tdef(table).pk_col;
        let mut parts = Vec::new();
        conjuncts(cond, &mut parts);
        if parts.len() == 1 {
            if let Some((BinOp::Eq, e)) = pk_comparison(&parts[0], SENTINEL, pk) {
                return Ok(e);
            }
        }
        Err(self.err_at(wpos, format!("unsupported WHERE clause: {PK_ONLY}")))
    }

    fn rowcount_one(&self, out: &mut Vec<Stmt>) {
        if let Some(rc) = self.cur.rowcount_var {
            out.push(Stmt::Assign { var: rc, expr: int(1) });
        }
    }

    fn select_into(&mut self, out: &mut Vec<Stmt>) -> Res<()> {
        let spos = self.here();
        self.advance(); // SELECT
        let mut items = Vec::new();
        if self.eat(&Tok::Star) {
            items.push(SelItem::Star);
        } else {
            loop {
                let ipos = self.here();
                if self.is_kw("sum") && *self.peek_at(1) == Tok::LParen {
                    self.advance();
                    self.advance();
                    let (c, p) = self.col_ref()?;
                    self.expect(Tok::RParen)?;
                    items.push(SelItem::Sum(c, p));
                } else if self.is_kw("count") && *self.peek_at(1) == Tok::LParen {
                    self.advance();
                    self.advance();
                    if !self.eat(&Tok::Star) {
                        return Err(self.err_here("only COUNT(*) is supported"));
                    }
                    self.expect(Tok::RParen)?;
                    items.push(SelItem::Count);
                } else if matches!(self.peek(), Tok::Ident(_)) && *self.peek_at(1) == Tok::LParen {
                    return Err(self.err_at(ipos, "only plain columns, SUM(col) and COUNT(*) are supported in SELECT"));
                } else if matches!(self.peek(), Tok::Ident(_)) {
                    let (c, p) = self.col_ref()?;
                    items.push(SelItem::Col(c, p));
                } else {
                    return Err(self.unexpected("a column name"));
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let is_agg = items.iter().any(|i| matches!(i, SelItem::Sum(..) | SelItem::Count));
        if is_agg && items.len() != 1 {
            return Err(self.err_at(spos, "an aggregate SELECT must have exactly one SUM(col) or COUNT(*) item"));
        }
        self.expect_kw("into")?;
        let mut targets = Vec::new();
        loop {
            let pos = self.here();
            let n = self.ident("an INTO variable")?;
            if *self.peek() == Tok::Dot {
                return Err(self.err_at(pos, "INTO a record field is not supported: the IR has no field assignment"));
            }
            let v = self.lookup(&n).ok_or_else(|| self.err_at(pos, format!("unknown variable '{n}'")))?;
            targets.push((n, v, pos));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect_kw("from")?;
        let tpos = self.here();
        let tname = self.ident("a table name")?;
        let table = self.table_id(&tname, tpos)?;
        if *self.peek() == Tok::Comma || self.is_kw("join") {
            return Err(self.err_here("joins are not supported"));
        }
        let alias = self.alias();

        if is_agg {
            let func = match &items[0] {
                SelItem::Sum(c, p) => AggFunc::Sum(self.column(table, c, *p)?),
                _ => AggFunc::Count,
            };
            let wpos = self.here();
            let cond = if self.eat_kw("where") {
                Some(self.expr(&ColCtx::Row { table, var: SENTINEL, alias })?)
            } else {
                None
            };
            let pk = self.tdef(table).pk_col;
            let (range, filter) = self.split_range(cond, SENTINEL, pk, wpos)?;
            if filter.is_some() {
                return Err(self.err_at(wpos, "aggregate WHERE: only primary-key range predicates are supported in this POC"));
            }
            if self.is_kw("for") {
                return Err(self.err_here("FOR UPDATE is not supported on aggregates"));
            }
            self.semi()?;
            if targets.len() != 1 {
                return Err(self.err_at(targets[1].2, "an aggregate SELECT must have exactly one INTO variable"));
            }
            let (n, _, p) = &targets[0];
            let into = self.writable_scalar(n, *p)?;
            out.append(&mut self.cur.pending);
            out.push(Stmt::Aggregate { table, range, func, into });
            self.rowcount_one(out);
            return Ok(());
        }

        let key = self.pk_eq_where(table, alias)?;
        let for_update = if self.eat_kw("for") {
            self.expect_kw("update")?;
            true
        } else {
            false
        };
        self.semi()?;
        out.append(&mut self.cur.pending);

        let star = matches!(items.as_slice(), [SelItem::Star]);
        if let [(n, v, p)] = targets.as_slice() {
            if let VarType::Row(t) = *self.var_ty(*v) {
                if !star {
                    return Err(self.err_at(*p, format!("SELECT INTO record '{n}' requires SELECT *")));
                }
                if t != table {
                    return Err(self.err_at(*p, format!("record '{n}' is not a %ROWTYPE of table '{tname}'")));
                }
                out.push(Stmt::Get { table, key, into: *v, for_update });
                self.rowcount_one(out);
                return Ok(());
            }
        }
        let cols: Vec<ColId> = if star {
            (0..self.tdef(table).columns.len() as ColId).collect()
        } else {
            items
                .iter()
                .map(|i| match i {
                    SelItem::Col(c, p) => self.column(table, c, *p),
                    _ => Err(self.err_at(spos, "internal: unexpected select item")),
                })
                .collect::<Res<_>>()?
        };
        if cols.len() != targets.len() {
            return Err(self.err_at(
                spos,
                format!("SELECT returns {} column(s) but INTO has {} variable(s)", cols.len(), targets.len()),
            ));
        }
        let vars: Vec<VarId> =
            targets.iter().map(|(n, _, p)| self.writable_scalar(n, *p)).collect::<Res<_>>()?;
        let tmp = self.temp(&format!("row_{tname}_"), VarType::Row(table));
        out.push(Stmt::Get { table, key, into: tmp, for_update });
        for (var, col) in vars.into_iter().zip(cols) {
            out.push(Stmt::Assign { var, expr: Expr::Col(tmp, col) });
        }
        self.rowcount_one(out);
        Ok(())
    }

    fn update(&mut self, out: &mut Vec<Stmt>) -> Res<()> {
        self.advance(); // UPDATE
        let tpos = self.here();
        let tname = self.ident("a table name")?;
        let table = self.table_id(&tname, tpos)?;
        let alias = self.alias();
        self.expect_kw("set")?;
        let mut sets: Vec<(ColId, Expr)> = Vec::new();
        loop {
            if *self.peek() == Tok::LParen {
                return Err(self.err_here("SET (cols) = (...) is not supported"));
            }
            let (c, p) = self.col_ref()?;
            let col = self.column(table, &c, p)?;
            if col == self.tdef(table).pk_col {
                return Err(self.err_at(p, "updating the primary key is not supported"));
            }
            if sets.iter().any(|(x, _)| *x == col) {
                return Err(self.err_at(p, format!("column '{c}' is set twice")));
            }
            self.expect(Tok::Eq)?;
            let e = self.expr(&ColCtx::Old { table, alias: alias.clone() })?;
            sets.push((col, e));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let key = self.pk_eq_where(table, alias)?;
        self.semi()?;
        out.append(&mut self.cur.pending);
        out.push(Stmt::Update { table, key, sets, rowcount_into: self.cur.rowcount_var });
        Ok(())
    }

    fn insert(&mut self, out: &mut Vec<Stmt>) -> Res<()> {
        let ipos = self.here();
        self.advance(); // INSERT
        self.expect_kw("into")?;
        let tpos = self.here();
        let tname = self.ident("a table name")?;
        let table = self.table_id(&tname, tpos)?;
        let ncols = self.tdef(table).columns.len();
        let mut cols: Option<Vec<ColId>> = None;
        if self.eat(&Tok::LParen) {
            let mut v: Vec<ColId> = Vec::new();
            loop {
                let (c, p) = self.col_ref()?;
                let col = self.column(table, &c, p)?;
                if v.contains(&col) {
                    return Err(self.err_at(p, format!("column '{c}' listed twice")));
                }
                v.push(col);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen)?;
            cols = Some(v);
        }
        if self.is_kw("select") {
            return Err(self.err_here("INSERT ... SELECT is not supported"));
        }
        self.expect_kw("values")?;
        self.expect(Tok::LParen)?;
        let mut vals = Vec::new();
        loop {
            vals.push(self.expr(&ColCtx::None)?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        self.semi()?;
        let cols = cols.unwrap_or_else(|| (0..ncols as ColId).collect());
        if cols.len() != vals.len() {
            return Err(self.err_at(ipos, format!("INSERT has {} column(s) but {} value(s)", cols.len(), vals.len())));
        }
        let pk = self.tdef(table).pk_col;
        if !cols.contains(&pk) {
            return Err(self.err_at(ipos, "INSERT must provide the primary-key column"));
        }
        let mut row = vec![Expr::Const(Value::Null); ncols];
        for (c, e) in cols.into_iter().zip(vals) {
            row[c as usize] = e;
        }
        out.append(&mut self.cur.pending);
        out.push(Stmt::Insert { table, row });
        self.rowcount_one(out);
        Ok(())
    }

    fn delete(&mut self, out: &mut Vec<Stmt>) -> Res<()> {
        self.advance(); // DELETE
        self.eat_kw("from");
        let tpos = self.here();
        let tname = self.ident("a table name")?;
        let table = self.table_id(&tname, tpos)?;
        let alias = self.alias();
        let key = self.pk_eq_where(table, alias)?;
        self.semi()?;
        out.append(&mut self.cur.pending);
        out.push(Stmt::Delete { table, key, rowcount_into: self.cur.rowcount_var });
        Ok(())
    }

    // ------------------------------------------------------------------
    // Expressions
    // ------------------------------------------------------------------

    fn expr(&mut self, ctx: &ColCtx) -> Res<Expr> {
        self.enter()?;
        let r = self.or_expr(ctx);
        self.leave();
        r
    }

    fn or_expr(&mut self, ctx: &ColCtx) -> Res<Expr> {
        let mut l = self.and_expr(ctx)?;
        while self.eat_kw("or") {
            let r = self.and_expr(ctx)?;
            l = bin(BinOp::Or, l, r);
        }
        Ok(l)
    }

    fn and_expr(&mut self, ctx: &ColCtx) -> Res<Expr> {
        let mut l = self.not_expr(ctx)?;
        while self.eat_kw("and") {
            let r = self.not_expr(ctx)?;
            l = bin(BinOp::And, l, r);
        }
        Ok(l)
    }

    fn not_expr(&mut self, ctx: &ColCtx) -> Res<Expr> {
        if self.eat_kw("not") {
            self.enter()?;
            let e = self.not_expr(ctx);
            self.leave();
            return Ok(Expr::Unary(UnOp::Not, Box::new(e?)));
        }
        self.cmp_expr(ctx)
    }

    fn cmp_expr(&mut self, ctx: &ColCtx) -> Res<Expr> {
        let l = self.add_expr(ctx)?;
        let op = match self.peek() {
            Tok::Eq => Some(BinOp::Eq),
            Tok::Ne => Some(BinOp::Ne),
            Tok::Lt => Some(BinOp::Lt),
            Tok::Le => Some(BinOp::Le),
            Tok::Gt => Some(BinOp::Gt),
            Tok::Ge => Some(BinOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let r = self.add_expr(ctx)?;
            return Ok(bin(op, l, r));
        }
        if self.eat_kw("is") {
            let neg = self.eat_kw("not");
            self.expect_kw("null")?;
            let e = Expr::IsNull(Box::new(l));
            return Ok(if neg { Expr::Unary(UnOp::Not, Box::new(e)) } else { e });
        }
        let neg = self.is_kw("not") && (self.is_kw_at(1, "between") || self.is_kw_at(1, "like") || self.is_kw_at(1, "in"));
        if neg {
            self.advance();
        }
        if self.eat_kw("between") {
            let a = self.add_expr(ctx)?;
            self.expect_kw("and")?;
            let b = self.add_expr(ctx)?;
            let e = bin(BinOp::And, bin(BinOp::Ge, l.clone(), a), bin(BinOp::Le, l, b));
            return Ok(if neg { Expr::Unary(UnOp::Not, Box::new(e)) } else { e });
        }
        if self.is_kw("like") || self.is_kw("in") {
            let what = if self.is_kw("like") { "LIKE" } else { "IN lists" };
            return Err(self.err_here(format!("{what} is not supported")));
        }
        Ok(l)
    }

    fn add_expr(&mut self, ctx: &ColCtx) -> Res<Expr> {
        let mut l = self.mul_expr(ctx)?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                Tok::Concat => BinOp::Concat,
                _ => return Ok(l),
            };
            self.advance();
            let r = self.mul_expr(ctx)?;
            l = bin(op, l, r);
        }
    }

    fn mul_expr(&mut self, ctx: &ColCtx) -> Res<Expr> {
        let mut l = self.unary(ctx)?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                _ => return Ok(l),
            };
            self.advance();
            let r = self.unary(ctx)?;
            l = bin(op, l, r);
        }
    }

    fn unary(&mut self, ctx: &ColCtx) -> Res<Expr> {
        if matches!(self.peek(), Tok::Minus | Tok::Plus) {
            let neg = self.advance().tok == Tok::Minus;
            self.enter()?;
            let e = self.unary(ctx);
            self.leave();
            let e = e?;
            if !neg {
                return Ok(e);
            }
            if let Expr::Const(Value::Int(i)) = e {
                if let Some(n) = i.checked_neg() {
                    return Ok(int(n));
                }
            }
            return Ok(Expr::Unary(UnOp::Neg, Box::new(e)));
        }
        self.primary(ctx)
    }

    fn args(&mut self, ctx: &ColCtx) -> Res<Vec<Expr>> {
        self.expect(Tok::LParen)?;
        let mut v = Vec::new();
        if *self.peek() != Tok::RParen {
            loop {
                v.push(self.expr(ctx)?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(Tok::RParen)?;
        Ok(v)
    }

    fn ctx_col(&self, ctx: &ColCtx, col: &str) -> Option<Expr> {
        match ctx {
            ColCtx::None => None,
            ColCtx::Old { table, .. } => self.tdef(*table).col(col).map(Expr::OldCol),
            ColCtx::Row { table, var, .. } => self.tdef(*table).col(col).map(|c| Expr::Col(*var, c)),
        }
    }

    fn ctx_names(&self, ctx: &ColCtx, q: &str) -> bool {
        let (table, alias) = match ctx {
            ColCtx::None => return false,
            ColCtx::Old { table, alias } | ColCtx::Row { table, alias, .. } => (*table, alias),
        };
        alias.as_deref() == Some(q) || (alias.is_none() && self.tdef(table).name == q)
    }

    fn primary(&mut self, ctx: &ColCtx) -> Res<Expr> {
        let pos = self.here();
        match self.peek().clone() {
            Tok::Int(i) => {
                self.advance();
                Ok(int(i))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Const(Value::Str(s)))
            }
            Tok::LParen => {
                if self.is_kw_at(1, "select") {
                    return Err(self.err_here("subqueries are not supported"));
                }
                self.advance();
                let e = self.expr(ctx)?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::Ident(name) => self.ident_expr(name, pos, ctx),
            _ => Err(self.unexpected("an expression")),
        }
    }

    fn ident_expr(&mut self, name: String, pos: (u32, u32), ctx: &ColCtx) -> Res<Expr> {
        if RESERVED.contains(&name.as_str()) {
            return Err(self.unexpected("an expression"));
        }
        self.advance();
        match name.as_str() {
            "null" => return Ok(Expr::Const(Value::Null)),
            "true" => return Ok(int(1)),
            "false" => return Ok(int(0)),
            "case" | "exists" | "prior" | "sqlcode" | "sqlerrm" | "sysdate" | "systimestamp" | "rownum" => {
                return Err(self.err_at(pos, format!("{} is not supported", name.to_uppercase())));
            }
            _ => {}
        }
        if name == "sql" && *self.peek() == Tok::Percent {
            self.advance();
            let apos = self.here();
            let attr = self.ident("ROWCOUNT")?;
            if attr != "rowcount" {
                return Err(self.err_at(apos, format!("SQL%{} is not supported", attr.to_uppercase())));
            }
            let v = self
                .cur
                .rowcount_var
                .ok_or_else(|| self.err_at(pos, "SQL%ROWCOUNT is only supported inside a procedure"))?;
            return Ok(Expr::Var(v));
        }
        if *self.peek() == Tok::LParen {
            let args = self.args(ctx)?;
            let arity = |n: usize, p: &Parser| {
                if args.len() == n {
                    Ok(())
                } else {
                    Err(p.err_at(pos, format!("{} expects {n} arguments", name.to_uppercase())))
                }
            };
            return match name.as_str() {
                "mod" => {
                    arity(2, self)?;
                    let mut it = args.into_iter();
                    let (a, b) = (it.next().unwrap_or(int(0)), it.next().unwrap_or(int(0)));
                    Ok(bin(BinOp::Mod, a, b))
                }
                "nvl" => {
                    arity(2, self)?;
                    Ok(Expr::Coalesce(args))
                }
                "coalesce" => {
                    if args.is_empty() {
                        return Err(self.err_at(pos, "COALESCE expects at least one argument"));
                    }
                    Ok(Expr::Coalesce(args))
                }
                _ => Err(self.err_at(pos, format!("function {} is not supported", name.to_uppercase()))),
            };
        }
        if *self.peek() == Tok::Percent {
            return Err(self.err_here(format!("attribute after '{name}' is not supported in expressions")));
        }
        if *self.peek() == Tok::Dot {
            self.advance();
            let fpos = self.here();
            let field = self.ident("a field name")?;
            if self.ctx_names(ctx, &name) {
                return self
                    .ctx_col(ctx, &field)
                    .ok_or_else(|| self.err_at(fpos, format!("table '{name}' has no column '{field}'")));
            }
            if let Some(v) = self.lookup(&name) {
                return match *self.var_ty(v) {
                    VarType::Row(t) => Ok(Expr::Col(v, self.column(t, &field, fpos)?)),
                    VarType::Scalar => Err(self.err_at(pos, format!("'{name}' is not a record"))),
                };
            }
            if let Some(seq) = self.schema.sequence(&name).map(|s| s.id) {
                if field != "nextval" {
                    return Err(self.err_at(fpos, format!("{name}.{} is not supported (only NEXTVAL)", field.to_uppercase())));
                }
                let tmp = self.temp(&format!("{name}_nextval_"), VarType::Scalar);
                self.cur.pending.push(Stmt::SeqNext { seq, into: tmp });
                return Ok(Expr::Var(tmp));
            }
            return Err(self.err_at(pos, format!("unknown identifier '{name}.{field}'")));
        }
        if let Some(e) = self.ctx_col(ctx, &name) {
            return Ok(e);
        }
        if let Some(v) = self.lookup(&name) {
            if *self.var_ty(v) != VarType::Scalar {
                return Err(self.err_at(pos, format!("record '{name}' cannot be used as a scalar value")));
            }
            return Ok(Expr::Var(v));
        }
        Err(self.err_at(pos, format!("unknown identifier '{name}'")))
    }
}
