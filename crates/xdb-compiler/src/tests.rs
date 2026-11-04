//! Parser and lowering unit tests.

use crate::compile;
use xdb_ir::*;

const SCHEMA: &str = "
CREATE TABLE account (id NUMBER PRIMARY KEY, balance NUMBER, owner VARCHAR2(40));
CREATE TABLE t2 (k INTEGER, v NUMBER, PRIMARY KEY (k));
CREATE SEQUENCE s START WITH 10 INCREMENT BY 1;
";

fn proc_of(body: &str) -> Procedure {
    let src = format!("{SCHEMA}\nCREATE OR REPLACE PROCEDURE p(a NUMBER, b IN VARCHAR2) IS {body}\n/\n");
    let prog = compile(&src).unwrap_or_else(|e| panic!("compile failed: {e}"));
    prog.procedures.into_iter().next().unwrap()
}

fn err_of(body: &str) -> crate::CompileError {
    let src = format!("{SCHEMA}\nCREATE PROCEDURE p(a NUMBER, b IN VARCHAR2) IS {body}");
    compile(&src).expect_err("expected a compile error")
}

fn var(p: &Procedure, name: &str) -> VarId {
    p.vars.iter().rev().find(|v| v.name == name).unwrap_or_else(|| panic!("no var {name}")).id
}

fn int(i: i64) -> Expr {
    Expr::Const(Value::Int(i))
}

fn bin(op: BinOp, a: Expr, b: Expr) -> Expr {
    Expr::Binary(op, Box::new(a), Box::new(b))
}

#[test]
fn schema_ids_and_types() {
    let p = compile(SCHEMA).unwrap();
    assert_eq!(p.schema.tables.len(), 2);
    let a = &p.schema.tables[0];
    assert_eq!((a.id, a.name.as_str(), a.pk_col), (0, "account", 0));
    assert_eq!(a.columns[2].ty, ColType::Varchar(40));
    let t2 = &p.schema.tables[1];
    assert_eq!((t2.id, t2.pk_col), (1, 0));
    assert_eq!(p.schema.sequences, vec![SequenceDef { id: 0, name: "s".into() }]);
}

#[test]
fn case_insensitive_and_params() {
    let src = format!("{SCHEMA} create procedure Foo(P_X number) as V NUMBER := p_x; begin v := V + 1; end FOO;");
    let prog = compile(&src).unwrap();
    let p = prog.procedure("foo").unwrap();
    assert_eq!(p.params, vec![0]);
    assert_eq!(p.vars[1].init, Some(Expr::Var(0)));
    assert_eq!(p.body, vec![Stmt::Assign { var: 1, expr: bin(BinOp::Add, Expr::Var(1), int(1)) }]);
}

#[test]
fn expression_precedence() {
    let p = proc_of("v NUMBER; BEGIN v := 1 + 2 * -3 - MOD(a, 2); END;");
    let v = var(&p, "v");
    let a = var(&p, "a");
    let expected = bin(
        BinOp::Sub,
        bin(BinOp::Add, int(1), bin(BinOp::Mul, int(2), int(-3))),
        bin(BinOp::Mod, Expr::Var(a), int(2)),
    );
    assert_eq!(p.body, vec![Stmt::Assign { var: v, expr: expected }]);
}

#[test]
fn boolean_precedence_and_null_tests() {
    let p = proc_of("v NUMBER; BEGIN v := NOT a = 1 OR a IS NOT NULL AND TRUE; END;");
    let a = Expr::Var(var(&p, "a"));
    let expected = bin(
        BinOp::Or,
        Expr::Unary(UnOp::Not, Box::new(bin(BinOp::Eq, a.clone(), int(1)))),
        bin(BinOp::And, Expr::Unary(UnOp::Not, Box::new(Expr::IsNull(Box::new(a)))), int(1)),
    );
    let Stmt::Assign { expr, .. } = &p.body[0] else { panic!() };
    assert_eq!(*expr, expected);
}

#[test]
fn concat_nvl_coalesce_strings() {
    let p = proc_of("v VARCHAR2(10); BEGIN v := NVL(b, 'x''y') || COALESCE(NULL, b, 'z'); END;");
    let b = Expr::Var(var(&p, "b"));
    let s = |x: &str| Expr::Const(Value::Str(x.into()));
    let expected = bin(
        BinOp::Concat,
        Expr::Coalesce(vec![b.clone(), s("x'y")]),
        Expr::Coalesce(vec![Expr::Const(Value::Null), b, s("z")]),
    );
    let Stmt::Assign { expr, .. } = &p.body[0] else { panic!() };
    assert_eq!(*expr, expected);
}

#[test]
fn select_into_scalars_uses_temp_row() {
    let p = proc_of("x NUMBER; y VARCHAR2(40); BEGIN SELECT balance, owner INTO x, y FROM account WHERE id = a FOR UPDATE; END;");
    let tmp = p.vars.iter().find(|v| v.ty == VarType::Row(0)).unwrap().id;
    assert_eq!(
        p.body,
        vec![
            Stmt::Get { table: 0, key: Expr::Var(var(&p, "a")), into: tmp, for_update: true },
            Stmt::Assign { var: var(&p, "x"), expr: Expr::Col(tmp, 1) },
            Stmt::Assign { var: var(&p, "y"), expr: Expr::Col(tmp, 2) },
        ]
    );
}

#[test]
fn select_star_into_rowtype_and_field_read() {
    let p = proc_of("r account%ROWTYPE; x account.balance%TYPE; BEGIN SELECT * INTO r FROM account WHERE id = 7; x := r.balance; END;");
    let r = var(&p, "r");
    assert_eq!(p.vars[r as usize].ty, VarType::Row(0));
    assert_eq!(
        p.body,
        vec![
            Stmt::Get { table: 0, key: int(7), into: r, for_update: false },
            Stmt::Assign { var: var(&p, "x"), expr: Expr::Col(r, 1) },
        ]
    );
}

#[test]
fn update_uses_oldcol() {
    let p = proc_of("BEGIN UPDATE account SET balance = balance + a, owner = 'z' WHERE id = 3; END;");
    assert_eq!(
        p.body,
        vec![Stmt::Update {
            table: 0,
            key: int(3),
            sets: vec![
                (1, bin(BinOp::Add, Expr::OldCol(1), Expr::Var(var(&p, "a")))),
                (2, Expr::Const(Value::Str("z".into()))),
            ],
            rowcount_into: None,
        }]
    );
}

#[test]
fn rowcount_after_update_and_delete() {
    let p = proc_of("n NUMBER; BEGIN UPDATE t2 SET v = 1 WHERE k = 1; n := SQL%ROWCOUNT; DELETE FROM t2 WHERE k = 2; END;");
    let rc = var(&p, "sql%rowcount");
    assert!(matches!(&p.body[0], Stmt::Update { rowcount_into: Some(x), .. } if *x == rc));
    assert_eq!(p.body[1], Stmt::Assign { var: var(&p, "n"), expr: Expr::Var(rc) });
    assert!(matches!(&p.body[2], Stmt::Delete { rowcount_into: Some(x), .. } if *x == rc));
}

#[test]
fn insert_with_columns_and_nextval() {
    let p = proc_of("BEGIN INSERT INTO account (owner, id) VALUES (b, s.NEXTVAL); END;");
    let tmp = p.vars.iter().find(|v| v.name.starts_with("$s_nextval")).unwrap().id;
    assert_eq!(
        p.body,
        vec![
            Stmt::SeqNext { seq: 0, into: tmp },
            Stmt::Insert {
                table: 0,
                row: vec![Expr::Var(tmp), Expr::Const(Value::Null), Expr::Var(var(&p, "b"))]
            },
        ]
    );
}

#[test]
fn aggregate_ranges() {
    let p = proc_of("v NUMBER; BEGIN SELECT SUM(balance) INTO v FROM account WHERE id BETWEEN 1 AND a; SELECT COUNT(*) INTO v FROM account WHERE id > 5; SELECT COUNT(*) INTO v FROM t2; END;");
    let v = var(&p, "v");
    let a = Expr::Var(var(&p, "a"));
    assert_eq!(
        p.body,
        vec![
            Stmt::Aggregate { table: 0, range: PkRange { lo: int(1), hi: a }, func: AggFunc::Sum(1), into: v },
            Stmt::Aggregate { table: 0, range: PkRange { lo: int(6), hi: int(i64::MAX) }, func: AggFunc::Count, into: v },
            Stmt::Aggregate { table: 1, range: PkRange { lo: int(i64::MIN), hi: int(i64::MAX) }, func: AggFunc::Count, into: v },
        ]
    );
}

#[test]
fn cursor_for_loop() {
    let p = proc_of(
        "v NUMBER := 0; BEGIN FOR r IN (SELECT * FROM account WHERE id >= a AND balance > 0 AND id < 100 ORDER BY id FETCH FIRST 10 ROWS ONLY FOR UPDATE) LOOP v := v + r.balance; EXIT WHEN v > 1000; END LOOP; END;",
    );
    let r = var(&p, "r");
    let v = var(&p, "v");
    assert_eq!(p.vars[r as usize].ty, VarType::Row(0));
    assert_eq!(
        p.body,
        vec![Stmt::ForRows {
            table: 0,
            range: PkRange { lo: Expr::Var(var(&p, "a")), hi: int(99) },
            filter: Some(bin(BinOp::Gt, Expr::Col(r, 1), int(0))),
            limit: Some(int(10)),
            for_update: true,
            row_var: r,
            body: vec![
                Stmt::Assign { var: v, expr: bin(BinOp::Add, Expr::Var(v), Expr::Col(r, 1)) },
                Stmt::Exit { cond: Some(bin(BinOp::Gt, Expr::Var(v), int(1000))) },
            ],
        }]
    );
}

#[test]
fn cursor_without_where_is_full_range() {
    let p = proc_of("BEGIN FOR r IN (SELECT k, v FROM t2) LOOP NULL; END LOOP; END;");
    let Stmt::ForRows { range, filter, limit, for_update, .. } = &p.body[0] else { panic!() };
    assert_eq!(*range, PkRange { lo: int(i64::MIN), hi: int(i64::MAX) });
    assert_eq!((filter, limit, for_update), (&None, &None, &false));
}

#[test]
fn numeric_for_loop_lowering() {
    let p = proc_of("v NUMBER := 0; BEGIN FOR i IN REVERSE 1 .. a LOOP v := v + i; END LOOP; END;");
    let i = var(&p, "i");
    let bound = p.vars.iter().find(|v| v.name.starts_with("$for_bound")).unwrap().id;
    let v = var(&p, "v");
    assert_eq!(
        p.body,
        vec![
            Stmt::Assign { var: i, expr: Expr::Var(var(&p, "a")) },
            Stmt::Assign { var: bound, expr: int(1) },
            Stmt::Loop {
                body: vec![
                    Stmt::Exit {
                        cond: Some(Expr::Coalesce(vec![bin(BinOp::Lt, Expr::Var(i), Expr::Var(bound)), int(1)]))
                    },
                    Stmt::Assign { var: v, expr: bin(BinOp::Add, Expr::Var(v), Expr::Var(i)) },
                    Stmt::Assign { var: i, expr: bin(BinOp::Sub, Expr::Var(i), int(1)) },
                ]
            },
        ]
    );
}

#[test]
fn while_and_loop_exit() {
    let p = proc_of("v NUMBER := 0; BEGIN WHILE v < 10 LOOP v := v + 1; END LOOP; LOOP EXIT; END LOOP; END;");
    let v = var(&p, "v");
    let cond = bin(BinOp::Lt, Expr::Var(v), int(10));
    let Stmt::Loop { body } = &p.body[0] else { panic!() };
    assert_eq!(
        body[0],
        Stmt::Exit { cond: Some(Expr::Unary(UnOp::Not, Box::new(Expr::Coalesce(vec![cond, int(0)])))) }
    );
    assert_eq!(p.body[1], Stmt::Loop { body: vec![Stmt::Exit { cond: None }] });
}

#[test]
fn if_elsif_else() {
    let p = proc_of("v NUMBER; BEGIN IF a = 1 THEN v := 1; ELSIF a = 2 THEN v := 2; ELSE NULL; END IF; END;");
    let a = Expr::Var(var(&p, "a"));
    let v = var(&p, "v");
    assert_eq!(
        p.body,
        vec![Stmt::If {
            cond: bin(BinOp::Eq, a.clone(), int(1)),
            then_body: vec![Stmt::Assign { var: v, expr: int(1) }],
            else_body: vec![Stmt::If {
                cond: bin(BinOp::Eq, a, int(2)),
                then_body: vec![Stmt::Assign { var: v, expr: int(2) }],
                else_body: vec![Stmt::Nop],
            }],
        }]
    );
}

#[test]
fn nested_block_shadowing_and_handlers() {
    let p = proc_of(
        "v NUMBER := 1; BEGIN DECLARE v NUMBER := 2; BEGIN v := v + 1; EXCEPTION WHEN NO_DATA_FOUND OR ZERO_DIVIDE THEN RAISE; WHEN OTHERS THEN ROLLBACK; END; v := 5; COMMIT; DBMS_OUTPUT.PUT_LINE(v); EXCEPTION WHEN DUP_VAL_ON_INDEX THEN NULL; END;",
    );
    let outer = p.vars.iter().find(|x| x.name == "v").unwrap().id;
    let inner = var(&p, "v");
    assert_ne!(outer, inner);
    assert_eq!(p.vars[outer as usize].init, Some(int(1)));
    assert_eq!(p.body[0], Stmt::Assign { var: inner, expr: int(2) });
    let Stmt::Block { body, handlers } = &p.body[1] else { panic!("{:?}", p.body[1]) };
    assert_eq!(body, &vec![Stmt::Assign { var: inner, expr: bin(BinOp::Add, Expr::Var(inner), int(1)) }]);
    assert_eq!(handlers.len(), 3);
    assert_eq!(handlers[0].code, Some(errcode::NO_DATA_FOUND));
    assert_eq!(handlers[1].code, Some(errcode::ZERO_DIVIDE));
    assert_eq!(handlers[0].body, vec![Stmt::Raise { code: errcode::NO_DATA_FOUND, msg: "NO_DATA_FOUND".into() }]);
    assert_eq!(handlers[1].body, vec![Stmt::Raise { code: errcode::ZERO_DIVIDE, msg: "ZERO_DIVIDE".into() }]);
    assert_eq!(handlers[2], Handler { code: None, body: vec![Stmt::Rollback] });
    assert_eq!(p.body[2], Stmt::Assign { var: outer, expr: int(5) });
    assert_eq!(p.body[3], Stmt::Commit);
    assert_eq!(p.body[4], Stmt::Output(Expr::Var(outer)));
    assert_eq!(p.handlers, vec![Handler { code: Some(errcode::DUP_VAL_ON_INDEX), body: vec![Stmt::Nop] }]);
}

#[test]
fn raises() {
    let p = proc_of("BEGIN RAISE NO_DATA_FOUND; RAISE_APPLICATION_ERROR(-20005, 'boom'); END;");
    assert_eq!(p.body[0], Stmt::Raise { code: errcode::NO_DATA_FOUND, msg: "NO_DATA_FOUND".into() });
    assert_eq!(p.body[1], Stmt::Raise { code: -20005, msg: "boom".into() });
}

#[test]
fn nextval_in_if_condition_precedes_if() {
    let p = proc_of("BEGIN IF s.nextval > 3 THEN NULL; END IF; END;");
    assert!(matches!(p.body[0], Stmt::SeqNext { seq: 0, .. }));
    assert!(matches!(p.body[1], Stmt::If { .. }));
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[test]
fn error_unsupported_where() {
    let e = err_of("v NUMBER;\nBEGIN\n  SELECT balance INTO v FROM account WHERE balance = 3;\nEND;");
    assert!(e.msg.contains("only primary-key equality"), "{e}");
    assert_eq!(e.line, 8);
    let e = err_of("BEGIN UPDATE account SET balance = 0 WHERE id > 3; END;");
    assert!(e.msg.contains("only primary-key equality"), "{e}");
    let e = err_of("BEGIN DELETE FROM account WHERE id = 1 AND balance = 0; END;");
    assert!(e.msg.contains("only primary-key equality"), "{e}");
    let e = err_of("BEGIN DELETE FROM account; END;");
    assert!(e.msg.contains("only primary-key equality"), "{e}");
}

#[test]
fn error_bad_syntax_line() {
    let e = err_of("v NUMBER;\nBEGIN\n  v := ;\nEND;");
    assert_eq!((e.line, e.col), (8, 8), "{e}");
    let e = err_of("BEGIN\n IF a THEN NULL;\n END;");
    assert_eq!(e.line, 8, "{e}");
    assert!(e.msg.contains("expected IF"), "{e}");
    let e = err_of("BEGIN NULL;");
    assert!(e.msg.contains("end of input"), "{e}");
}

#[test]
fn error_field_assignment() {
    let e = err_of("r account%ROWTYPE; BEGIN r.balance := 1; END;");
    assert!(e.msg.contains("field assignment"), "{e}");
}

#[test]
fn error_misc() {
    for (body, needle) in [
        ("BEGIN x := 1; END;", "unknown variable"),
        ("BEGIN a := 1; END;", "IN parameter"),
        ("BEGIN EXIT; END;", "EXIT outside"),
        ("v NUMBER; BEGIN v := foo(1); END;", "function FOO"),
        ("v NUMBER; BEGIN v := s.currval; END;", "NEXTVAL"),
        ("BEGIN RAISE_APPLICATION_ERROR(-1, 'x'); END;", "-20999"),
        ("BEGIN RAISE; END;", "bare RAISE"),
        ("BEGIN INSERT INTO account VALUES (1, 2); END;", "value"),
        ("BEGIN UPDATE account SET id = 2 WHERE id = 1; END;", "primary key"),
        ("CURSOR c IS SELECT * FROM account; BEGIN NULL; END;", "CURSOR"),
        ("BEGIN FOR r IN (SELECT * FROM account WHERE id > 1 AND id > 2) LOOP NULL; END LOOP; END;", "lower bound"),
        ("BEGIN FOR r IN (SELECT * FROM account ORDER BY balance) LOOP NULL; END LOOP; END;", "ORDER BY"),
        ("v NUMBER; BEGIN SELECT SUM(balance) INTO v FROM account WHERE balance > 1; END;", "range"),
        ("v NUMBER; BEGIN v := 1 LIKE 2; END;", "LIKE"),
        ("BEGIN RETURN; END;", "RETURN"),
        ("BEGIN NULL; EXCEPTION WHEN my_err THEN NULL; END;", "unsupported exception"),
    ] {
        let e = err_of(body);
        assert!(e.msg.contains(needle), "body {body:?}: got {e}");
        assert!(e.line >= 5, "body {body:?}: line {}", e.line);
    }
}

#[test]
fn error_schema() {
    assert!(compile("CREATE TABLE x (a NUMBER, b NUMBER);").unwrap_err().msg.contains("PRIMARY KEY"));
    assert!(compile("CREATE TABLE x (a VARCHAR2(3) PRIMARY KEY);").is_err());
    assert!(compile("CREATE TABLE x (a NUMBER PRIMARY KEY, b NUMBER PRIMARY KEY);").is_err());
    assert!(compile("CREATE VIEW v AS SELECT 1;").is_err());
    assert!(compile("SELECT 1 FROM dual;").is_err());
}

#[test]
fn never_panics_on_garbage() {
    let samples = [
        "", "/", ";;", "CREATE", "CREATE PROCEDURE", "CREATE PROCEDURE p IS", "CREATE PROCEDURE p IS BEGIN",
        "CREATE PROCEDURE p( IS BEGIN NULL; END;", "CREATE TABLE t (", "CREATE TABLE t (id NUMBER PRIMARY",
        "CREATE PROCEDURE p IS v t%ROWTYPE; BEGIN NULL; END;", "CREATE PROCEDURE p IS BEGIN FOR i IN 1 .. LOOP",
        "CREATE PROCEDURE p IS BEGIN FOR r IN (SELECT", "CREATE PROCEDURE p IS v NUMBER; BEGIN v := ((((1",
        "CREATE PROCEDURE p IS v NUMBER; BEGIN v := 1 BETWEEN 2; END;",
    ];
    for s in samples {
        assert!(compile(s).is_err() || s.is_empty() || s == "/" || s == ";;", "{s:?}");
    }
    let deep = format!("CREATE PROCEDURE p IS v NUMBER; BEGIN v := {}1{}; END;", "(".repeat(5000), ")".repeat(5000));
    assert!(compile(&deep).unwrap_err().msg.contains("nesting"));
    let deep_neg = format!("CREATE PROCEDURE p IS v NUMBER; BEGIN v := {}1; END;", "-".repeat(5000));
    assert!(compile(&deep_neg).is_err());
    let deep_if = format!("CREATE PROCEDURE p IS BEGIN {} NULL; END;", "IF 1 THEN ".repeat(5000));
    assert!(compile(&deep_if).is_err());
}
