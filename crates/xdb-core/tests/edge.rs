//! Edge cases of transaction blocks (design doc §7) and a multi-seed fuzz test.

use xdb_core::*;
use xdb_ir::{errcode, Key, Program, Value};

fn compile(src: &str) -> Program {
    xdb_compiler::compile(src).unwrap_or_else(|e| panic!("{e:?}"))
}

fn sim(p: &Program, mode: ExecMode, rows: i64) -> Sim {
    let cfg = ClusterConfig { shards: 3, mode, seed: 3, table_ranges: vec![(0, 1, rows), (1, 0, 100)], ..Default::default() };
    let mut s = Sim::new(p.clone(), cfg).unwrap();
    s.load((1..=rows).map(|i| (Key { table: 0, pk: i }, vec![Value::Int(i), Value::Int(10)])));
    s
}

fn v(s: &Sim, k: i64) -> Option<i64> {
    s.committed(Key { table: 0, pk: k }).map(|r| r[1].as_int().unwrap())
}

fn ok_serial(s: &Sim) {
    let r = xdb_checker::check(s.history());
    assert!(r.ok, "{:?}", r.violations.first().map(|v| v.to_string()));
    assert_eq!(s.pending_intents(), 0);
}

const SCHEMA: &str = "CREATE TABLE t (id NUMBER PRIMARY KEY, v NUMBER);
CREATE TABLE log (id NUMBER PRIMARY KEY, n NUMBER);
";

#[test]
fn for_update_cursor_across_commit_fails_like_oracle() {
    let p = compile(&format!(
        "{SCHEMA}
CREATE OR REPLACE PROCEDURE p IS
BEGIN
  FOR r IN (SELECT * FROM t WHERE id BETWEEN 1 AND 5 FOR UPDATE) LOOP
    UPDATE t SET v = v + 1 WHERE id = r.id;
    COMMIT;
  END LOOP;
END p;
/"
    ));
    for mode in ExecMode::all() {
        let mut s = sim(&p, mode, 10);
        let st = s.call(0, Call::new("p", &[]));
        assert_eq!(st.result.unwrap_err().0, errcode::FETCH_OUT_OF_SEQUENCE, "{}", mode.name());
        assert_eq!(v(&s, 1), Some(11), "first block committed");
        assert_eq!(v(&s, 2), Some(10));
        ok_serial(&s);
    }
}

#[test]
fn plain_cursor_across_commit_reads_its_snapshot() {
    let p = compile(&format!(
        "{SCHEMA}
CREATE OR REPLACE PROCEDURE p IS
BEGIN
  FOR r IN (SELECT * FROM t WHERE id BETWEEN 1 AND 5) LOOP
    UPDATE t SET v = r.v * 2 WHERE id = r.id;
    COMMIT;
  END LOOP;
END p;
/"
    ));
    for mode in ExecMode::all() {
        let mut s = sim(&p, mode, 10);
        assert!(s.call(0, Call::new("p", &[])).result.is_ok());
        for k in 1..=5 {
            assert_eq!(v(&s, k), Some(20), "{}", mode.name());
        }
        assert_eq!(v(&s, 6), Some(10));
        ok_serial(&s);
    }
}

#[test]
fn rowcount_rollback_and_error_after_commit() {
    let p = compile(&format!(
        "{SCHEMA}
CREATE OR REPLACE PROCEDURE p(p_bad IN NUMBER) IS
  n NUMBER;
BEGIN
  UPDATE t SET v = 99 WHERE id = 1000;
  n := SQL%ROWCOUNT;
  INSERT INTO log VALUES (1, n);
  COMMIT;
  UPDATE t SET v = 0 WHERE id = 2;
  ROLLBACK;
  UPDATE t SET v = v + 5 WHERE id = 3;
  COMMIT;
  UPDATE t SET v = 0 WHERE id = 4;
  IF p_bad = 1 THEN
    RAISE_APPLICATION_ERROR(-20009, 'boom');
  END IF;
END p;
/"
    ));
    for mode in ExecMode::all() {
        let mut s = sim(&p, mode, 10);
        let st = s.call(0, Call::new("p", &[1]));
        assert_eq!(st.result.unwrap_err().0, -20009);
        assert_eq!(s.committed(Key { table: 1, pk: 1 }).unwrap()[1], Value::Int(0), "rowcount of missing row");
        assert_eq!(v(&s, 2), Some(10), "{}: rolled back", mode.name());
        assert_eq!(v(&s, 3), Some(15), "{}: committed block stays", mode.name());
        assert_eq!(v(&s, 4), Some(10), "{}: failing block undone", mode.name());
        ok_serial(&s);
    }
}

#[test]
fn snapshot_audit_always_sees_constant_total_fuzz() {
    // Transfers keep the total constant; every audit_total over all accounts must see
    // exactly that total (snapshot isolation of the range read), under all modes and
    // many seeds. The checker also verifies serializability of the whole history.
    let src = std::fs::read_to_string(format!("{}/../../workloads/bank.sql", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let p = compile(&src);
    let n: i64 = 60;
    for seed in 1..=12u64 {
        for mode in ExecMode::all() {
            for rule in [ConflictRule::WoundWait, ConflictRule::WaitDie] {
                let cfg = ClusterConfig {
                    shards: 1 + (seed as usize % 5),
                    replicas: 2 + (seed as usize % 3),
                    mode,
                    rule,
                    seed,
                    table_ranges: vec![(0, 0, n - 1), (1, 0, 10_000_000)],
                    ..Default::default()
                };
                let mut s = Sim::new(p.clone(), cfg).unwrap();
                s.load((0..n).map(|i| (Key { table: 0, pk: i }, vec![Value::Int(i), Value::Int(100), Value::Null])));
                let mut gen = |r: &mut Rng, _c: usize| {
                    if r.chance(0.1) {
                        Call::new("audit_total", &[0, n - 1])
                    } else {
                        let a = r.skewed(n as u64, 0.6) as i64;
                        let b = r.below(n as u64) as i64;
                        Call::new("transfer", &[a, b, r.range_i64(1, 150)])
                    }
                };
                let rep = s.run_closed_loop(24, 4_000_000, 0, &mut gen);
                let label = format!("seed {seed} {} {:?}", mode.name(), rule);
                let audits: Vec<_> = rep.calls.iter().filter(|c| c.proc == "audit_total").collect();
                assert!(!audits.is_empty(), "{label}");
                for a in audits {
                    assert_eq!(a.output, vec![(n * 100).to_string()], "{label}: audit saw a torn total");
                }
                for c in &rep.calls {
                    if let Err((code, _)) = &c.result {
                        assert_eq!(*code, -20001, "{label}");
                    }
                }
                let r = xdb_checker::check(s.history());
                assert!(r.ok, "{label}: {:?}", r.violations.first().map(|v| v.to_string()));
                assert_eq!(s.pending_intents(), 0, "{label}");
            }
        }
    }
}

#[test]
fn checker_catches_a_deliberately_broken_engine() {
    // Mutation test: with the read-timestamp check disabled, writers may commit
    // "under" earlier readers. The checker (or the audit invariant) must notice.
    let src = std::fs::read_to_string(format!("{}/../../workloads/bank.sql", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let p = compile(&src);
    let n: i64 = 20;
    let mut caught = 0;
    for seed in 1..=5u64 {
        let cfg = ClusterConfig {
            shards: 3,
            mode: ExecMode::v1(),
            seed,
            table_ranges: vec![(0, 0, n - 1), (1, 0, 10_000_000)],
            fault_ignore_read_ts: true,
            ..Default::default()
        };
        let mut s = Sim::new(p.clone(), cfg).unwrap();
        s.load((0..n).map(|i| (Key { table: 0, pk: i }, vec![Value::Int(i), Value::Int(100), Value::Null])));
        let mut gen = |r: &mut Rng, _c: usize| {
            if r.chance(0.3) {
                Call::new("audit_total", &[0, n - 1])
            } else {
                Call::new("transfer", &[r.range_i64(0, n - 1), r.range_i64(0, n - 1), 1])
            }
        };
        s.run_closed_loop(24, 4_000_000, 0, &mut gen);
        let r = xdb_checker::check(s.history());
        if !r.ok {
            caught += 1;
            println!("seed {seed}: {} violations, e.g. {}", r.total_violations, r.violations[0]);
        }
    }
    assert!(caught >= 4, "checker missed the injected bug in {} of 5 runs", 5 - caught);
}
