//! Lowering of the structured IR into a flat instruction list ("the MI program" of a
//! procedure). Data instructions become micro-instructions sent to shards at run time.

use crate::eval::expr_vars;
use xdb_ir::*;

#[derive(Clone, Debug)]
pub enum DataOp {
    Get { table: TableId, key: Expr, into: VarId, for_update: bool },
    Update { table: TableId, key: Expr, sets: Vec<(ColId, Expr)>, rowcount_into: Option<VarId> },
    Insert { table: TableId, row: Vec<Expr> },
    Delete { table: TableId, key: Expr, rowcount_into: Option<VarId> },
    Aggregate { table: TableId, range: PkRange, func: AggFunc, into: VarId },
    ScanOpen {
        cursor: usize,
        table: TableId,
        range: PkRange,
        filter: Option<Expr>,
        limit: Option<Expr>,
        for_update: bool,
        row_var: VarId,
    },
}

impl DataOp {
    pub fn table(&self) -> TableId {
        match self {
            DataOp::Get { table, .. }
            | DataOp::Update { table, .. }
            | DataOp::Insert { table, .. }
            | DataOp::Delete { table, .. }
            | DataOp::Aggregate { table, .. }
            | DataOp::ScanOpen { table, .. } => *table,
        }
    }
    /// True for operations that only write (their result is not needed to continue),
    /// which write pipelining may retire before the shard answers.
    pub fn pipelinable(&self) -> bool {
        matches!(
            self,
            DataOp::Update { rowcount_into: None, .. }
                | DataOp::Insert { .. }
                | DataOp::Delete { rowcount_into: None, .. }
        )
    }
}

#[derive(Clone, Debug)]
pub enum Ins {
    Assign { var: VarId, expr: Expr },
    Data(DataOp),
    SeqNext { seq: SeqId, into: VarId },
    /// Move to the next row of `cursor` into `row_var`, or jump to `end` when done.
    CursorNext { cursor: usize, row_var: VarId, end: usize },
    /// Jump to `target` if `cond.is_true() == jump_if`, popping `pop` handler frames.
    /// `guard`: the fall-through path is only a `Raise` (an `IF c THEN RAISE` check).
    Branch { cond: Expr, jump_if: bool, target: usize, pop: usize, guard: bool },
    Jump { target: usize, pop: usize },
    Raise { code: i32, msg: String },
    /// Enter a block with exception handlers (takes a savepoint).
    PushHandler { def: usize },
    PopHandler,
    Commit,
    Rollback,
    Output(Expr),
    End,
}

#[derive(Clone, Debug)]
pub struct HandlerDef {
    /// (error code or None for OTHERS, pc of the handler body)
    pub arms: Vec<(Option<i32>, usize)>,
}

#[derive(Clone, Debug)]
pub struct Code {
    pub name: String,
    pub ins: Vec<Ins>,
    pub handlers: Vec<HandlerDef>,
    pub n_cursors: usize,
    pub params: Vec<VarId>,
    pub var_types: Vec<VarType>,
    /// Variables read / written by each instruction (for the lookahead window).
    pub reads: Vec<Vec<VarId>>,
    pub writes: Vec<Vec<VarId>>,
    /// No statement writes or locks: eligible for follower reads.
    pub read_only: bool,
}

const PATCH: usize = usize::MAX;

struct Lower {
    ins: Vec<Ins>,
    handlers: Vec<HandlerDef>,
    n_cursors: usize,
    /// For each enclosing loop: pcs of exits to patch, handler depth at loop entry.
    loops: Vec<(Vec<usize>, usize)>,
    depth: usize,
}

impl Lower {
    fn emit(&mut self, i: Ins) -> usize {
        self.ins.push(i);
        self.ins.len() - 1
    }
    fn patch(&mut self, at: usize, to: usize) {
        match &mut self.ins[at] {
            Ins::Branch { target, .. } | Ins::Jump { target, .. } => *target = to,
            Ins::CursorNext { end, .. } => *end = to,
            _ => unreachable!("patch of non-jump"),
        }
    }
    fn stmts(&mut self, body: &[Stmt]) -> Result<(), String> {
        for s in body {
            self.stmt(s)?;
        }
        Ok(())
    }
    fn block(&mut self, body: &[Stmt], handlers: &[Handler]) -> Result<(), String> {
        if handlers.is_empty() {
            return self.stmts(body);
        }
        let def = self.handlers.len();
        self.handlers.push(HandlerDef { arms: vec![] });
        self.emit(Ins::PushHandler { def });
        self.depth += 1;
        self.stmts(body)?;
        self.depth -= 1;
        self.emit(Ins::PopHandler);
        let mut to_end = vec![self.emit(Ins::Jump { target: PATCH, pop: 0 })];
        let mut arms = vec![];
        for h in handlers {
            let pc = self.ins.len();
            self.stmts(&h.body)?;
            to_end.push(self.emit(Ins::Jump { target: PATCH, pop: 0 }));
            arms.push((h.code, pc));
        }
        self.handlers[def].arms = arms;
        let end = self.ins.len();
        for j in to_end {
            self.patch(j, end);
        }
        Ok(())
    }
    fn stmt(&mut self, s: &Stmt) -> Result<(), String> {
        match s {
            Stmt::Nop => {}
            Stmt::Assign { var, expr } => {
                self.emit(Ins::Assign { var: *var, expr: expr.clone() });
            }
            Stmt::Get { table, key, into, for_update } => {
                self.emit(Ins::Data(DataOp::Get {
                    table: *table,
                    key: key.clone(),
                    into: *into,
                    for_update: *for_update,
                }));
            }
            Stmt::Update { table, key, sets, rowcount_into } => {
                self.emit(Ins::Data(DataOp::Update {
                    table: *table,
                    key: key.clone(),
                    sets: sets.clone(),
                    rowcount_into: *rowcount_into,
                }));
            }
            Stmt::Insert { table, row } => {
                self.emit(Ins::Data(DataOp::Insert { table: *table, row: row.clone() }));
            }
            Stmt::Delete { table, key, rowcount_into } => {
                self.emit(Ins::Data(DataOp::Delete {
                    table: *table,
                    key: key.clone(),
                    rowcount_into: *rowcount_into,
                }));
            }
            Stmt::Aggregate { table, range, func, into } => {
                self.emit(Ins::Data(DataOp::Aggregate {
                    table: *table,
                    range: range.clone(),
                    func: *func,
                    into: *into,
                }));
            }
            Stmt::ForRows { table, range, filter, limit, for_update, row_var, body } => {
                let cursor = self.n_cursors;
                self.n_cursors += 1;
                self.emit(Ins::Data(DataOp::ScanOpen {
                    cursor,
                    table: *table,
                    range: range.clone(),
                    filter: filter.clone(),
                    limit: limit.clone(),
                    for_update: *for_update,
                    row_var: *row_var,
                }));
                let head = self.emit(Ins::CursorNext { cursor, row_var: *row_var, end: PATCH });
                self.loops.push((vec![], self.depth));
                self.stmts(body)?;
                self.emit(Ins::Jump { target: head, pop: 0 });
                let end = self.ins.len();
                self.patch(head, end);
                let (exits, _) = self.loops.pop().unwrap();
                for e in exits {
                    self.patch(e, end);
                }
            }
            Stmt::SeqNext { seq, into } => {
                self.emit(Ins::SeqNext { seq: *seq, into: *into });
            }
            Stmt::If { cond, then_body, else_body } => {
                let guard = else_body.is_empty()
                    && then_body.len() == 1
                    && matches!(then_body[0], Stmt::Raise { .. });
                let br = self.emit(Ins::Branch {
                    cond: cond.clone(),
                    jump_if: false,
                    target: PATCH,
                    pop: 0,
                    guard,
                });
                self.stmts(then_body)?;
                if else_body.is_empty() {
                    let end = self.ins.len();
                    self.patch(br, end);
                } else {
                    let j = self.emit(Ins::Jump { target: PATCH, pop: 0 });
                    let els = self.ins.len();
                    self.patch(br, els);
                    self.stmts(else_body)?;
                    let end = self.ins.len();
                    self.patch(j, end);
                }
            }
            Stmt::Loop { body } => {
                let head = self.ins.len();
                self.loops.push((vec![], self.depth));
                self.stmts(body)?;
                self.emit(Ins::Jump { target: head, pop: 0 });
                let end = self.ins.len();
                let (exits, _) = self.loops.pop().unwrap();
                for e in exits {
                    self.patch(e, end);
                }
            }
            Stmt::Exit { cond } => {
                let Some((_, d)) = self.loops.last() else {
                    return Err("EXIT outside a loop".into());
                };
                let pop = self.depth - *d;
                let at = match cond {
                    Some(c) => self.emit(Ins::Branch {
                        cond: c.clone(),
                        jump_if: true,
                        target: PATCH,
                        pop,
                        guard: false,
                    }),
                    None => self.emit(Ins::Jump { target: PATCH, pop }),
                };
                self.loops.last_mut().unwrap().0.push(at);
            }
            Stmt::Raise { code, msg } => {
                self.emit(Ins::Raise { code: *code, msg: msg.clone() });
            }
            Stmt::Block { body, handlers } => self.block(body, handlers)?,
            Stmt::Commit => {
                self.emit(Ins::Commit);
            }
            Stmt::Rollback => {
                self.emit(Ins::Rollback);
            }
            Stmt::Output(e) => {
                self.emit(Ins::Output(e.clone()));
            }
        }
        Ok(())
    }
}

fn rw(ins: &Ins) -> (Vec<VarId>, Vec<VarId>) {
    let mut r = vec![];
    let mut w = vec![];
    match ins {
        Ins::Assign { var, expr } => {
            expr_vars(expr, &mut r);
            w.push(*var);
        }
        Ins::SeqNext { into, .. } => w.push(*into),
        Ins::CursorNext { row_var, .. } => w.push(*row_var),
        Ins::Branch { cond, .. } => expr_vars(cond, &mut r),
        Ins::Output(e) => expr_vars(e, &mut r),
        Ins::Data(op) => match op {
            DataOp::Get { key, into, .. } => {
                expr_vars(key, &mut r);
                w.push(*into);
            }
            DataOp::Update { key, sets, rowcount_into, .. } => {
                expr_vars(key, &mut r);
                sets.iter().for_each(|(_, e)| expr_vars(e, &mut r));
                w.extend(rowcount_into);
            }
            DataOp::Insert { row, .. } => row.iter().for_each(|e| expr_vars(e, &mut r)),
            DataOp::Delete { key, rowcount_into, .. } => {
                expr_vars(key, &mut r);
                w.extend(rowcount_into);
            }
            DataOp::Aggregate { range, into, .. } => {
                expr_vars(&range.lo, &mut r);
                expr_vars(&range.hi, &mut r);
                w.push(*into);
            }
            DataOp::ScanOpen { range, filter, limit, row_var, .. } => {
                expr_vars(&range.lo, &mut r);
                expr_vars(&range.hi, &mut r);
                if let Some(f) = filter {
                    let mut fv = vec![];
                    expr_vars(f, &mut fv);
                    r.extend(fv.into_iter().filter(|v| v != row_var));
                }
                if let Some(l) = limit {
                    expr_vars(l, &mut r);
                }
            }
        },
        _ => {}
    }
    r.sort_unstable();
    r.dedup();
    (r, w)
}

/// Implicit `SELECT ... FOR UPDATE` (CockroachDB): a point read of a row that a later
/// UPDATE or DELETE of the same block targets with the same key expression takes the
/// write intent at once, instead of failing a read refresh or being wounded later.
/// The scan stops at the end of the block (COMMIT / ROLLBACK / END) or when a
/// variable of the key is reassigned.
/// `c = c + e` / `c = c - e` / `c = e + c` for every assignment, with `e` free of
/// `OldCol`: the shape that escrow turns into a commutative delta.
pub fn delta_shaped(sets: &[(ColId, Expr)]) -> bool {
    fn no_old(e: &Expr) -> bool {
        match e {
            Expr::OldCol(_) => false,
            Expr::Const(_) | Expr::Var(_) | Expr::Col(..) => true,
            Expr::Unary(_, a) | Expr::IsNull(a) => no_old(a),
            Expr::Binary(_, a, b) => no_old(a) && no_old(b),
            Expr::Coalesce(l) => l.iter().all(no_old),
        }
    }
    !sets.is_empty()
        && sets.iter().all(|(c, e)| match e {
            Expr::Binary(BinOp::Add, a, b) => {
                (matches!(a.as_ref(), Expr::OldCol(x) if x == c) && no_old(b))
                    || (matches!(b.as_ref(), Expr::OldCol(x) if x == c) && no_old(a))
            }
            Expr::Binary(BinOp::Sub, a, b) => matches!(a.as_ref(), Expr::OldCol(x) if x == c) && no_old(b),
            _ => false,
        })
}

/// With escrow on, an UPDATE that will become a delta must not be preceded by an
/// implicit lock: the lock would block other transactions' deltas on the row.
fn implicit_sfu(ins: &mut [Ins], writes: &[Vec<VarId>], escrow: bool) -> usize {
    let mut n = 0;
    for i in 0..ins.len() {
        let Ins::Data(DataOp::Get { table, key, for_update: false, .. }) = &ins[i] else { continue };
        let (table, key) = (*table, key.clone());
        let mut kv = vec![];
        expr_vars(&key, &mut kv);
        let mut hit = false;
        for j in i + 1..ins.len() {
            match &ins[j] {
                Ins::Commit | Ins::Rollback | Ins::End => break,
                Ins::Data(DataOp::Update { table: t, key: k, sets, .. }) if *t == table && *k == key => {
                    hit = !(escrow && delta_shaped(sets));
                    break;
                }
                Ins::Data(DataOp::Delete { table: t, key: k, .. }) if *t == table && *k == key => {
                    hit = true;
                    break;
                }
                _ => {}
            }
            if writes[j].iter().any(|w| kv.contains(w)) {
                break;
            }
        }
        if hit {
            if let Ins::Data(DataOp::Get { for_update, .. }) = &mut ins[i] {
                *for_update = true;
                n += 1;
            }
        }
    }
    n
}

pub fn lower(p: &Procedure, sfu: bool, escrow: bool) -> Result<Code, String> {
    let mut l = Lower { ins: vec![], handlers: vec![], n_cursors: 0, loops: vec![], depth: 0 };
    for v in &p.vars {
        if let Some(init) = &v.init {
            l.emit(Ins::Assign { var: v.id, expr: init.clone() });
        }
    }
    l.block(&p.body, &p.handlers)?;
    l.emit(Ins::End);
    let (reads, writes): (Vec<_>, Vec<_>) = l.ins.iter().map(rw).unzip();
    if sfu {
        implicit_sfu(&mut l.ins, &writes, escrow);
    }
    let read_only = l.ins.iter().all(|i| match i {
        Ins::Data(DataOp::Get { for_update, .. }) | Ins::Data(DataOp::ScanOpen { for_update, .. }) => !for_update,
        Ins::Data(DataOp::Aggregate { .. }) => true,
        Ins::Data(_) => false,
        _ => true,
    });
    Ok(Code {
        read_only,
        name: p.name.clone(),
        ins: l.ins,
        handlers: l.handlers,
        n_cursors: l.n_cursors,
        params: p.params.clone(),
        var_types: p.vars.iter().map(|v| v.ty.clone()).collect(),
        reads,
        writes,
    })
}
