//! Shared contract of the XDB proof of concept.
//!
//! * [`Value`], [`Row`], [`Key`]: data model.
//! * [`Schema`]: tables and sequences.
//! * [`Procedure`], [`Stmt`], [`Expr`]: the IR produced by `xdb-compiler` from the
//!   PL/SQL subset and executed by `xdb-core`. The core turns data statements into
//!   micro-instructions (MIs) at run time.
//! * [`History`]: what `xdb-core` records about committed transactions, checked by
//!   `xdb-checker`.
//!
//! This crate is frozen: other crates depend on it. Change it only in coordination.

use std::fmt;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// A column value. NUMBER is an `i64` (POC compromise: no decimals).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Value {
    Null,
    Int(i64),
    Str(String),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
    /// PL/SQL truthiness for conditions: `Int(0)` is false, `Null` is false (unknown),
    /// any other `Int` is true. Comparisons produce `Int(1)` / `Int(0)` / `Null`.
    pub fn is_true(&self) -> bool {
        matches!(self, Value::Int(i) if *i != 0)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Str(s) => write!(f, "'{s}'"),
        }
    }
}

/// A row: values by column position, as declared in the table.
pub type Row = Vec<Value>;

pub type TableId = u32;
pub type ColId = u16;
pub type SeqId = u32;
pub type VarId = u32;

/// A record key. Every table has a single integer primary-key column
/// (POC compromise). The key orders by table first, then primary key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub table: TableId,
    pub pk: i64,
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColType {
    Number,
    Varchar(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String, // lower case
    pub ty: ColType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableDef {
    pub id: TableId,   // == index in Schema::tables
    pub name: String,  // lower case
    pub columns: Vec<ColumnDef>,
    /// Position of the primary-key column (type Number).
    pub pk_col: ColId,
    /// `CHECK (col >= n)`-style bounds on integer columns (inclusive). Enforced on
    /// every write; escrow deltas are accepted only if the bound holds in the worst
    /// case of all pending deltas (O'Neil escrow).
    pub checks: Vec<ColCheck>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColCheck {
    pub col: ColId,
    pub min: Option<i64>,
    pub max: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequenceDef {
    pub id: SeqId,    // == index in Schema::sequences
    pub name: String, // lower case
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schema {
    pub tables: Vec<TableDef>,
    pub sequences: Vec<SequenceDef>,
}

impl Schema {
    pub fn table(&self, name: &str) -> Option<&TableDef> {
        self.tables.iter().find(|t| t.name.eq_ignore_ascii_case(name))
    }
    pub fn sequence(&self, name: &str) -> Option<&SequenceDef> {
        self.sequences.iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }
}

impl TableDef {
    pub fn col(&self, name: &str) -> Option<ColId> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
            .map(|p| p as ColId)
    }
}

// ---------------------------------------------------------------------------
// Exceptions
// ---------------------------------------------------------------------------

/// Oracle-compatible error codes used by the IR.
pub mod errcode {
    /// `NO_DATA_FOUND` (Oracle ORA-01403, SQLCODE +100).
    pub const NO_DATA_FOUND: i32 = 100;
    /// `DUP_VAL_ON_INDEX` (ORA-00001).
    pub const DUP_VAL_ON_INDEX: i32 = -1;
    /// `ZERO_DIVIDE` (ORA-01476).
    pub const ZERO_DIVIDE: i32 = -1476;
    /// `VALUE_ERROR` (ORA-06502): type errors, NULL where a value is required.
    pub const VALUE_ERROR: i32 = -6502;
    /// Snapshot too old (ORA-01555).
    pub const SNAPSHOT_TOO_OLD: i32 = -1555;
    /// Fetch out of sequence (ORA-01002).
    pub const FETCH_OUT_OF_SEQUENCE: i32 = -1002;
    /// Check constraint violated (ORA-02290).
    pub const CHECK_VIOLATION: i32 = -2290;
    // RAISE_APPLICATION_ERROR uses -20000 ..= -20999.
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    /// Integer division, truncating toward zero. Division by zero raises ZERO_DIVIDE.
    Div,
    Mod,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

/// Expressions are pure: they never touch storage.
/// NULL semantics follow SQL: arithmetic and comparison with NULL give NULL;
/// `AND`/`OR` use three-valued logic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    Const(Value),
    /// A scalar variable (parameters are variables too).
    Var(VarId),
    /// A column of a row variable (`r.balance`, `%ROWTYPE` records, cursor rows).
    Col(VarId, ColId),
    /// Only inside `Stmt::Update::sets`: the current value of a column of the row
    /// being updated (`SET balance = balance + 1`).
    OldCol(ColId),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>),
    /// `NVL(a, b)` / `COALESCE(a, b, ...)`.
    Coalesce(Vec<Expr>),
}

// ---------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    /// `SUM(col)`; NULL when no row matches.
    Sum(ColId),
    /// `COUNT(*)`.
    Count,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handler {
    /// `None` = `WHEN OTHERS`. `Some(code)` matches that error code.
    pub code: Option<i32>,
    pub body: Vec<Stmt>,
}

/// Range bounds on the primary key, both inclusive. Compilers map
/// `pk > x` to `lo = x + 1`, `pk BETWEEN a AND b` to `lo = a, hi = b`, and a missing
/// bound to `i64::MIN` / `i64::MAX` constants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PkRange {
    pub lo: Expr,
    pub hi: Expr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stmt {
    /// `NULL;`
    Nop,
    /// `v := expr;`
    Assign { var: VarId, expr: Expr },

    // ---- data statements (become micro-instructions) ----
    /// `SELECT * | cols INTO ... FROM t WHERE pk = key [FOR UPDATE]`.
    /// Stores the whole row in the row variable `into`.
    /// Raises NO_DATA_FOUND if the row does not exist.
    /// The compiler emits extra `Assign`s to copy columns into scalar variables.
    Get {
        table: TableId,
        key: Expr,
        into: VarId,
        for_update: bool,
    },
    /// `UPDATE t SET c1 = e1, ... WHERE pk = key`. `sets` exprs may use `OldCol`.
    /// Missing row: no effect (0 rows updated), no error.
    /// If `rowcount_into` is set, 1 or 0 is stored there (`SQL%ROWCOUNT`).
    Update {
        table: TableId,
        key: Expr,
        sets: Vec<(ColId, Expr)>,
        rowcount_into: Option<VarId>,
    },
    /// `INSERT INTO t VALUES (...)`: one expr per column, in table order
    /// (missing columns are `Const(Null)`). Raises DUP_VAL_ON_INDEX if the key exists.
    Insert { table: TableId, row: Vec<Expr> },
    /// `DELETE FROM t WHERE pk = key`. Missing row: no effect.
    Delete {
        table: TableId,
        key: Expr,
        rowcount_into: Option<VarId>,
    },
    /// `SELECT SUM(col) | COUNT(*) INTO v FROM t WHERE pk BETWEEN lo AND hi`.
    Aggregate {
        table: TableId,
        range: PkRange,
        func: AggFunc,
        into: VarId,
    },
    /// Cursor FOR loop:
    /// `FOR r IN (SELECT * FROM t WHERE pk BETWEEN lo AND hi [AND filter]
    ///            ORDER BY pk [FETCH FIRST limit ROWS ONLY]) LOOP body END LOOP;`
    /// Rows are visited in primary-key order. `filter` may use `Col(row_var, _)`.
    /// The row set is read once, when the loop starts. `Exit` leaves the loop.
    ForRows {
        table: TableId,
        range: PkRange,
        filter: Option<Expr>,
        limit: Option<Expr>,
        for_update: bool,
        row_var: VarId,
        body: Vec<Stmt>,
    },
    /// `v := seq.NEXTVAL` (non-transactional, gaps allowed, like Oracle).
    SeqNext { seq: SeqId, into: VarId },

    // ---- control flow ----
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        else_body: Vec<Stmt>,
    },
    /// `LOOP ... END LOOP`. WHILE and numeric FOR loops compile to this plus `Exit`.
    Loop { body: Vec<Stmt> },
    /// `EXIT [WHEN cond]`: leaves the innermost `Loop` or `ForRows`.
    Exit { cond: Option<Expr> },
    /// `RAISE_APPLICATION_ERROR(code, msg)` or re-raise of a named exception.
    Raise { code: i32, msg: String },
    /// `BEGIN body EXCEPTION handlers END`. An implicit savepoint is taken at the
    /// start of the block; when a handler catches an error, the block's writes are
    /// rolled back to that savepoint before the handler runs (Oracle semantics for
    /// the failing statement; POC simplification: the whole inner block).
    Block {
        body: Vec<Stmt>,
        handlers: Vec<Handler>,
    },
    /// `COMMIT;` ends the current transaction block and starts a new one.
    Commit,
    /// `ROLLBACK;`
    Rollback,
    /// `DBMS_OUTPUT.PUT_LINE(expr)` — buffered, discarded if the block restarts.
    Output(Expr),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VarType {
    Scalar,
    /// `%ROWTYPE` record or cursor row of this table.
    Row(TableId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VarDecl {
    pub id: VarId, // == index in Procedure::vars
    pub name: String,
    pub ty: VarType,
    /// Initial value expression (`v NUMBER := 0;`), evaluated at procedure start.
    pub init: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Procedure {
    pub name: String, // lower case
    /// Parameter variables, in call order. They are also listed in `vars`.
    pub params: Vec<VarId>,
    /// All variables: parameters, declared locals, loop rows and compiler temporaries.
    pub vars: Vec<VarDecl>,
    pub body: Vec<Stmt>,
    /// Top-level `EXCEPTION` section of the procedure.
    pub handlers: Vec<Handler>,
}

/// Output of the compiler: the schema from `CREATE TABLE` / `CREATE SEQUENCE`
/// plus every procedure of the script.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Program {
    pub schema: Schema,
    pub procedures: Vec<Procedure>,
}

impl Program {
    pub fn procedure(&self, name: &str) -> Option<&Procedure> {
        self.procedures.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }
}

// ---------------------------------------------------------------------------
// History (recorded by xdb-core, verified by xdb-checker)
// ---------------------------------------------------------------------------

/// Id of one transaction *attempt*. A restarted block gets a new `TxnId`.
pub type TxnId = u64;

/// A hybrid-logical-clock timestamp: simulated wall time in nanoseconds, a logical
/// counter, and the node id as a tie-breaker. Totally ordered and unique per node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Ts {
    pub wall: u64,
    pub logical: u32,
    pub node: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Committed,
    Aborted,
}

/// A point read. `observed` is the transaction whose committed write produced the
/// version that was read, `None` for the initial load (or no version at all), and
/// the reader's own id when it read its own earlier write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadEvent {
    pub key: Key,
    pub observed: Option<TxnId>,
    /// The value read; `None` = row absent (or deleted).
    pub value: Option<Row>,
}

/// A range read (cursor, aggregate). `rows` lists every *present* row seen, in key
/// order, with the writer of the version seen (same convention as `ReadEvent`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeReadEvent {
    pub table: TableId,
    pub lo: i64,
    pub hi: i64,
    pub rows: Vec<(i64, Option<TxnId>)>,
}

/// Final effect of a transaction on one key (last write wins inside a txn).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteEvent {
    pub key: Key,
    /// `None` = delete.
    pub value: Option<Row>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxnRecord {
    pub id: TxnId,
    pub outcome: Outcome,
    /// Final read timestamp (after any refresh) = serialization point.
    /// For committed transactions `commit_ts` is the serialization timestamp and all
    /// reads are valid as of `commit_ts`.
    pub commit_ts: Ts,
    pub reads: Vec<ReadEvent>,
    pub range_reads: Vec<RangeReadEvent>,
    pub writes: Vec<WriteEvent>,
}

/// Initial data plus every transaction attempt that ended (committed or aborted).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Rows present before any transaction ran (written by "txn None").
    pub initial: Vec<(Key, Row)>,
    pub txns: Vec<TxnRecord>,
}
