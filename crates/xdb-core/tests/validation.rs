//! Validation of the POC: correctness (serializability checked by xdb-checker,
//! invariants) under every execution mode, plus latency sanity on an idle cluster.

use xdb_core::*;
use xdb_ir::{Key, Program, Row, Value};

fn workload(name: &str) -> Program {
    let path = format!("{}/../../workloads/{}", env!("CARGO_MANIFEST_DIR"), name);
    let src = std::fs::read_to_string(&path).unwrap();
    xdb_compiler::compile(&src).unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

fn tid(p: &Program, name: &str) -> u32 {
    p.schema.table(name).unwrap().id
}

const N_ACC: i64 = 300;
const START_BAL: i64 = 1000;

fn bank(mode: ExecMode, rule: ConflictRule, shards: usize, n_acc: i64, seed: u64) -> Sim {
    let p = workload("bank.sql");
    let acc = tid(&p, "account");
    let ledger = tid(&p, "ledger");
    let cfg = ClusterConfig {
        shards,
        replicas: 3,
        mode,
        rule,
        seed,
        table_ranges: vec![(acc, 0, n_acc - 1), (ledger, 0, 10_000_000)],
        ..Default::default()
    };
    let mut sim = Sim::new(p, cfg).unwrap();
    sim.load((0..n_acc).map(|i| {
        (Key { table: acc, pk: i }, vec![Value::Int(i), Value::Int(START_BAL), Value::Str(format!("o{i}"))])
    }));
    sim
}

fn balance(sim: &Sim, id: i64) -> i64 {
    let acc = tid(&sim.program, "account");
    match sim.committed(Key { table: acc, pk: id }) {
        Some(r) => r[1].as_int().unwrap(),
        None => panic!("account {id} missing"),
    }
}

fn assert_serializable(sim: &Sim, label: &str) {
    let r = xdb_checker::check(sim.history());
    if !r.ok {
        for v in r.violations.iter().take(5) {
            eprintln!("{label}: {v}");
        }
        panic!("{label}: {} violations", r.total_violations);
    }
    assert_eq!(sim.pending_intents(), 0, "{label}: intents left after quiesce");
}

fn bank_total(sim: &Sim, n_acc: i64) -> i64 {
    (0..n_acc).map(|i| balance(sim, i)).sum()
}

// ---------------------------------------------------------------- single calls

#[test]
fn transfer_latency_by_mode() {
    let mut lat = vec![];
    for mode in ExecMode::all() {
        let mut sim = bank(mode, ConflictRule::WoundWait, 3, N_ACC, 1);
        // accounts 10 and 150 live on shards 0 and 1; the call runs on node 2.
        let st = sim.call(2, Call::new("transfer", &[10, 150, 5]));
        assert!(st.result.is_ok(), "{:?}", st.result);
        assert_eq!(balance(&sim, 10), START_BAL - 5);
        assert_eq!(balance(&sim, 150), START_BAL + 5);
        assert_serializable(&sim, mode.name());
        println!("{:12} transfer latency {:7.1} us", mode.name(), st.latency_ns() as f64 / 1000.0);
        lat.push(st.latency_ns());
    }
    assert!(lat[0] > lat[1], "naive must be slower than v1");
    assert!(lat[3] < lat[1], "speculative must be faster than v1");
}

#[test]
fn insufficient_funds_undoes_speculative_writes() {
    for mode in ExecMode::all() {
        let mut sim = bank(mode, ConflictRule::WoundWait, 3, N_ACC, 2);
        let st = sim.call(0, Call::new("transfer", &[10, 150, START_BAL + 1]));
        assert_eq!(st.result.as_ref().unwrap_err().0, -20001, "{}", mode.name());
        assert_eq!(balance(&sim, 10), START_BAL);
        assert_eq!(balance(&sim, 150), START_BAL);
        assert_serializable(&sim, mode.name());
    }
}

#[test]
fn transfer_to_self_keeps_balance() {
    // The same-key bug of the first design draft (§3.4).
    for mode in ExecMode::all() {
        let mut sim = bank(mode, ConflictRule::WoundWait, 3, N_ACC, 3);
        let st = sim.call(1, Call::new("transfer", &[42, 42, 100]));
        assert!(st.result.is_ok());
        assert_eq!(balance(&sim, 42), START_BAL, "{}", mode.name());
        assert_serializable(&sim, mode.name());
    }
}

#[test]
fn missing_account_raises_no_data_found() {
    let mut sim = bank(ExecMode::speculative(), ConflictRule::WoundWait, 3, N_ACC, 4);
    let st = sim.call(0, Call::new("transfer", &[N_ACC + 5, 1, 1]));
    assert_eq!(st.result.unwrap_err().0, xdb_ir::errcode::NO_DATA_FOUND);
    assert_eq!(balance(&sim, 1), START_BAL);
    assert_serializable(&sim, "missing");
}

// ---------------------------------------------------------------- concurrency

fn run_bank(mode: ExecMode, rule: ConflictRule, shards: usize, n_acc: i64, clients: usize, seed: u64) -> (Sim, RunReport) {
    let mut sim = bank(mode, rule, shards, n_acc, seed);
    let mut gen = |r: &mut Rng, _c: usize| {
        let x = r.below(100);
        if x < 85 {
            let a = r.range_i64(0, n_acc - 1);
            let mut b = r.range_i64(0, n_acc - 1);
            if r.chance(0.02) {
                b = a;
            }
            Call::new("transfer", &[a, b, r.range_i64(1, 300)])
        } else if x < 95 {
            Call::new("deposit", &[r.range_i64(0, n_acc - 1), 0])
        } else {
            let lo = r.range_i64(0, n_acc - 1);
            Call::new("audit_total", &[lo, (lo + 40).min(n_acc - 1)])
        }
    };
    let rep = sim.run_closed_loop(clients, 20_000_000, 0, &mut gen);
    (sim, rep)
}

#[test]
fn concurrent_bank_is_serializable_all_modes() {
    for rule in [ConflictRule::WoundWait, ConflictRule::WaitDie] {
        for mode in ExecMode::all() {
            for shards in [1, 3, 5] {
                let label = format!("{}/{:?}/{} shards", mode.name(), rule, shards);
                let (sim, rep) = run_bank(mode, rule, shards, 100, 48, 11);
                assert!(rep.calls.len() > 100, "{label}: too few calls");
                for c in &rep.calls {
                    if let Err((code, m)) = &c.result {
                        assert_eq!(*code, -20001, "{label}: unexpected error {code} {m}");
                    }
                }
                assert_eq!(bank_total(&sim, 100), 100 * START_BAL, "{label}: money created or lost");
                assert_serializable(&sim, &label);
                println!(
                    "{label:32} calls {:5} thr {:8.0}/s p50 {:7.1}us p99 {:8.1}us restarts w{} d{} r{} s{}",
                    rep.calls.len(),
                    rep.throughput(),
                    rep.latency_us(50.0, None),
                    rep.latency_us(99.0, None),
                    rep.stats.restart_wounded,
                    rep.stats.restart_died,
                    rep.stats.restart_refresh,
                    rep.stats.restart_stage
                );
            }
        }
    }
}

#[test]
fn hotspot_makes_progress_without_deadlock() {
    for rule in [ConflictRule::WoundWait, ConflictRule::WaitDie] {
        for mode in [ExecMode::v1(), ExecMode::speculative()] {
            let label = format!("hot {}/{:?}", mode.name(), rule);
            let (sim, rep) = run_bank(mode, rule, 3, 4, 64, 5);
            // Every call submitted finished (the run drains), and many committed.
            assert!(rep.ok_calls() > 50, "{label}: only {} ok", rep.ok_calls());
            assert_eq!(bank_total(&sim, 4), 4 * START_BAL);
            assert_serializable(&sim, &label);
            let max_restarts = rep.calls.iter().map(|c| c.restarts).max().unwrap_or(0);
            println!("{label:28} ok {:5} p99 {:9.1}us max restarts {}", rep.ok_calls(), rep.latency_us(99.0, None), max_restarts);
        }
    }
}

// ---------------------------------------------------------------- blocks, batch

#[test]
fn batch_blocks_with_concurrent_transfers() {
    let p = workload("batch.sql");
    let acc = tid(&p, "account");
    let jp = tid(&p, "job_progress");
    // batch.sql has no transfer procedure: append bank's transfer by recompiling both.
    let src = std::fs::read_to_string(format!("{}/../../workloads/batch.sql", env!("CARGO_MANIFEST_DIR"))).unwrap()
        + "\nCREATE OR REPLACE PROCEDURE mv(p_from IN NUMBER, p_to IN NUMBER, p_amt IN NUMBER) IS
BEGIN
  UPDATE account SET balance = balance - p_amt WHERE id = p_from;
  UPDATE account SET balance = balance + p_amt WHERE id = p_to;
END mv;
/\n";
    let p = xdb_compiler::compile(&src).unwrap();
    for mode in ExecMode::all() {
        let cfg = ClusterConfig {
            shards: 3,
            mode,
            seed: 9,
            table_ranges: vec![(acc, 1, 600), (jp, 0, 10)],
            ..Default::default()
        };
        let mut sim = Sim::new(p.clone(), cfg).unwrap();
        sim.load((1..=600).map(|i| (Key { table: acc, pk: i }, vec![Value::Int(i), Value::Int(10_000), Value::Null])));
        sim.load([(Key { table: jp, pk: 1 }, vec![Value::Int(1), Value::Int(0)])]);
        let mut started = false;
        let mut gen = |r: &mut Rng, c: usize| {
            if c == 0 && !started {
                started = true;
                return Call::new("monthly_interest", &[1, 50]).with_class(2);
            }
            Call::new("mv", &[r.range_i64(1, 600), r.range_i64(1, 600), 7])
        };
        let rep = sim.run_closed_loop(16, 30_000_000, 0, &mut gen);
        let label = format!("batch {}", mode.name());
        let mi = rep.calls.iter().find(|c| c.proc == "monthly_interest").expect("batch ran");
        assert!(mi.result.is_ok(), "{label}: {:?}", mi.result);
        let last = sim.committed(Key { table: jp, pk: 1 }).unwrap()[1].as_int().unwrap();
        assert_eq!(last, 600, "{label}: progress");
        assert_serializable(&sim, &label);
        println!("{label:20} batch latency {:9.1}us restarts {}", mi.latency_ns() as f64 / 1000.0, mi.restarts);
    }
}

#[test]
fn exception_handler_rolls_back_to_savepoint() {
    let src = "
CREATE TABLE t (id NUMBER PRIMARY KEY, v NUMBER);
CREATE OR REPLACE PROCEDURE p(p_id IN NUMBER) IS
  v_n NUMBER := 0;
BEGIN
  UPDATE t SET v = v + 1 WHERE id = 1;
  BEGIN
    UPDATE t SET v = v + 100 WHERE id = 2;
    INSERT INTO t VALUES (p_id, 0);     -- duplicate when p_id exists
    v_n := 1;
  EXCEPTION
    WHEN DUP_VAL_ON_INDEX THEN
      v_n := 2;
  END;
  UPDATE t SET v = v + 1000 * v_n WHERE id = 3;
END p;
/";
    let p = xdb_compiler::compile(src).unwrap();
    for mode in ExecMode::all() {
        let cfg = ClusterConfig { shards: 3, mode, table_ranges: vec![(0, 0, 30)], ..Default::default() };
        let mut sim = Sim::new(p.clone(), cfg).unwrap();
        sim.load((1..=3).map(|i| (Key { table: 0, pk: i }, vec![Value::Int(i), Value::Int(0)])));
        // p_id = 2 exists: inner block rolled back, handler sets v_n = 2.
        let st = sim.call(0, Call::new("p", &[2]));
        assert!(st.result.is_ok(), "{:?}", st.result);
        let v = |sim: &Sim, k| sim.committed(Key { table: 0, pk: k }).map(|r: Row| r[1].as_int().unwrap());
        assert_eq!(v(&sim, 1), Some(1), "{}", mode.name());
        assert_eq!(v(&sim, 2), Some(0), "{}: savepoint rollback", mode.name());
        assert_eq!(v(&sim, 3), Some(2000), "{}", mode.name());
        // p_id = 20 is new: everything applies.
        let st = sim.call(1, Call::new("p", &[20]));
        assert!(st.result.is_ok());
        assert_eq!(v(&sim, 2), Some(100));
        assert_eq!(v(&sim, 3), Some(3000));
        assert_eq!(v(&sim, 20), Some(0));
        assert_serializable(&sim, mode.name());
    }
}

// ---------------------------------------------------------------- TPC-C lite

#[test]
fn tpcc_lite_serializable_and_consistent() {
    let p = workload("tpcc_lite.sql");
    let t = |n| tid(&p, n);
    const W: i64 = 2;
    const C: i64 = 30;
    const I: i64 = 100;
    for mode in ExecMode::all() {
        let cfg = ClusterConfig {
            shards: 3,
            mode,
            seed: 21,
            table_ranges: vec![
                (t("warehouse"), 0, W - 1),
                (t("district"), 0, W * 10 - 1),
                (t("customer"), 0, W * 10 * 3000),
                (t("item"), 1, I),
                (t("stock"), 0, W * 100_000),
                (t("orders"), 0, W * 10 * 1_000_000),
                (t("order_line"), 0, W * 10 * 1_000_000 * 16),
            ],
            ..Default::default()
        };
        let mut sim = Sim::new(p.clone(), cfg).unwrap();
        let int = |v: i64| Value::Int(v);
        let mut rows: Vec<(Key, Row)> = vec![];
        for w in 0..W {
            rows.push((Key { table: t("warehouse"), pk: w }, vec![int(w), int(0)]));
            for d in 0..10 {
                let dk = w * 10 + d;
                rows.push((Key { table: t("district"), pk: dk }, vec![int(dk), int(1), int(0)]));
                for c in 0..C {
                    let ck = dk * 3000 + c;
                    rows.push((Key { table: t("customer"), pk: ck }, vec![int(ck), int(0), int(0), int(0)]));
                }
            }
            for i in 1..=I {
                let sk = w * 100_000 + i;
                rows.push((Key { table: t("stock"), pk: sk }, vec![int(sk), int(50), int(0), int(0)]));
            }
        }
        for i in 1..=I {
            rows.push((Key { table: t("item"), pk: i }, vec![int(i), int(i % 50 + 1)]));
        }
        sim.load(rows);
        let mut gen = |r: &mut Rng, _c: usize| {
            let w = r.range_i64(0, W - 1);
            let d = r.range_i64(0, 9);
            let c = r.range_i64(0, C - 1);
            if r.chance(0.5) {
                let mut a = vec![w, d, c];
                let lines = r.range_i64(1, 5);
                for l in 0..5 {
                    if l < lines {
                        let item = if r.chance(0.01) { -1 } else { r.range_i64(1, I) };
                        a.push(item);
                        a.push(r.range_i64(1, 10));
                    } else {
                        a.push(0);
                        a.push(0);
                    }
                }
                Call::new("new_order", &a)
            } else {
                Call::new("payment", &[w, d, c, r.range_i64(1, 5000)])
            }
        };
        let rep = sim.run_closed_loop(32, 20_000_000, 0, &mut gen);
        let label = format!("tpcc {}", mode.name());
        for c in &rep.calls {
            if let Err((code, m)) = &c.result {
                assert_eq!(*code, -20002, "{label}: {code} {m}");
            }
        }
        assert_serializable(&sim, &label);
        // Consistency: warehouse ytd = sum of its districts' ytd; orders per district.
        let fs = xdb_checker::final_state(sim.history());
        for w in 0..W {
            let wy = fs[&Key { table: t("warehouse"), pk: w }][1].as_int().unwrap();
            let dy: i64 = (0..10).map(|d| fs[&Key { table: t("district"), pk: w * 10 + d }][2].as_int().unwrap()).sum();
            assert_eq!(wy, dy, "{label}: ytd");
            for d in 0..10 {
                let dk = w * 10 + d;
                let next = fs[&Key { table: t("district"), pk: dk }][1].as_int().unwrap();
                let n_orders = fs
                    .keys()
                    .filter(|k| k.table == t("orders") && k.pk / 1_000_000 == dk)
                    .count() as i64;
                assert_eq!(next - 1, n_orders, "{label}: orders of district {dk}");
            }
        }
        println!(
            "{label:20} calls {:5} thr {:8.0}/s new_order p50 {:7.1}us p99 {:8.1}us restarts w{} r{}",
            rep.calls.len(),
            rep.throughput(),
            rep.latency_us(50.0, Some("new_order")),
            rep.latency_us(99.0, Some("new_order")),
            rep.stats.restart_wounded,
            rep.stats.restart_refresh
        );
    }
}
