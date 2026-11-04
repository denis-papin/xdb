//! Compiles the workload scripts under `workloads/` and checks key IR shapes.

use std::path::PathBuf;

use xdb_compiler::compile;
use xdb_ir::*;

fn load(name: &str) -> Program {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../workloads").join(name);
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    compile(&src).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn bank_transfer_shape() {
    let prog = load("bank.sql");
    let account = prog.schema.table("account").unwrap().id;
    let ledger = prog.schema.table("ledger").unwrap().id;
    let seq = prog.schema.sequence("ledger_seq").unwrap().id;
    let t = prog.procedure("transfer").unwrap();
    assert_eq!(t.params.len(), 3);
    let kinds: Vec<&str> = t
        .body
        .iter()
        .map(|s| match s {
            Stmt::Get { .. } => "Get",
            Stmt::Assign { .. } => "Assign",
            Stmt::If { .. } => "If",
            Stmt::Update { .. } => "Update",
            Stmt::SeqNext { .. } => "SeqNext",
            Stmt::Insert { .. } => "Insert",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["Get", "Assign", "If", "Update", "Update", "SeqNext", "Insert"]);

    let (p_from, p_to, p_amt) = (t.params[0], t.params[1], t.params[2]);
    let Stmt::Get { table, key, for_update, .. } = &t.body[0] else { unreachable!() };
    assert_eq!((*table, key, *for_update), (account, &Expr::Var(p_from), false));
    let Stmt::If { then_body, else_body, .. } = &t.body[2] else { unreachable!() };
    assert_eq!(then_body, &vec![Stmt::Raise { code: -20001, msg: "insufficient funds".into() }]);
    assert!(else_body.is_empty());
    let Stmt::Update { key, sets, .. } = &t.body[3] else { unreachable!() };
    assert_eq!(key, &Expr::Var(p_from));
    assert_eq!(
        sets,
        &vec![(1, Expr::Binary(BinOp::Sub, Box::new(Expr::OldCol(1)), Box::new(Expr::Var(p_amt))))]
    );
    let Stmt::Update { key, .. } = &t.body[4] else { unreachable!() };
    assert_eq!(key, &Expr::Var(p_to));
    let Stmt::SeqNext { seq: s, into } = &t.body[5] else { unreachable!() };
    assert_eq!(*s, seq);
    let Stmt::Insert { table, row } = &t.body[6] else { unreachable!() };
    assert_eq!(*table, ledger);
    assert_eq!(row, &vec![Expr::Var(*into), Expr::Var(p_from), Expr::Var(p_to), Expr::Var(p_amt)]);

    let d = prog.procedure("deposit").unwrap();
    assert!(matches!(d.body.as_slice(), [Stmt::Update { .. }]));
    let a = prog.procedure("audit_total").unwrap();
    assert!(matches!(
        a.body.as_slice(),
        [Stmt::Aggregate { func: AggFunc::Sum(1), .. }, Stmt::Output(_)]
    ));
}

#[test]
fn batch_compiles() {
    let prog = load("batch.sql");
    let p = prog.procedure("monthly_interest").unwrap();
    let [Stmt::Get { .. }, Stmt::Assign { .. }, Stmt::Loop { body }] = p.body.as_slice() else {
        panic!("{:#?}", p.body)
    };
    let Stmt::ForRows { range, limit, filter, .. } = &body[1] else { panic!("{body:#?}") };
    assert!(limit.is_some());
    assert!(filter.is_none());
    assert!(matches!(range.lo, Expr::Binary(BinOp::Add, _, _)));
    assert_eq!(range.hi, Expr::Const(Value::Int(i64::MAX)));
    assert!(matches!(body[2], Stmt::Exit { cond: Some(_) }));
    assert!(matches!(body[3], Stmt::Update { .. }));
    assert_eq!(body[4], Stmt::Commit);
}

#[test]
fn tpcc_compiles() {
    let prog = load("tpcc_lite.sql");
    assert_eq!(prog.schema.tables.len(), 7);
    let n = prog.procedure("new_order").unwrap();
    assert_eq!(n.params.len(), 13);
    assert!(matches!(n.body[0], Stmt::Assign { .. }));
    assert!(matches!(n.body[1], Stmt::Get { for_update: true, .. }));
    let p = prog.procedure("payment").unwrap();
    assert_eq!(p.body.iter().filter(|s| matches!(s, Stmt::Update { .. })).count(), 3);
}
