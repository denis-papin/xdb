//! Benchmark harness of the xdb POC: workload setup, metric extraction and
//! result records (one CSV row per run). The experiments themselves are in
//! `src/main.rs`.

use std::collections::HashMap;
use std::fmt::Write as _;
use xdb_core::*;
use xdb_ir::{Key, Program, Row, Value};

// ============================================================================ workloads

/// Workload sources are embedded so that the binary runs from any directory
/// (and in the slim Docker runtime image).
pub const BANK_SQL: &str = include_str!("../../../workloads/bank.sql");
pub const BATCH_SQL: &str = include_str!("../../../workloads/batch.sql");
pub const TPCC_SQL: &str = include_str!("../../../workloads/tpcc_lite.sql");

/// Small two-row transfer appended to batch.sql (which has no transfer of its own),
/// as in xdb-core/tests/validation.rs.
pub const MV_SQL: &str = "\nCREATE OR REPLACE PROCEDURE mv(p_from IN NUMBER, p_to IN NUMBER, p_amt IN NUMBER) IS
BEGIN
  UPDATE account SET balance = balance - p_amt WHERE id = p_from;
  UPDATE account SET balance = balance + p_amt WHERE id = p_to;
END mv;
/\n";

pub fn compile(src: &str, name: &str) -> Program {
    xdb_compiler::compile(src).unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

pub fn tid(p: &Program, name: &str) -> u32 {
    p.schema.table(name).unwrap_or_else(|| panic!("table {name}")).id
}

pub const START_BAL: i64 = 1000;

/// Everything a run needs besides the workload.
#[derive(Clone, Debug)]
pub struct Setup {
    pub mode: ExecMode,
    pub rule: ConflictRule,
    pub shards: usize,
    pub replicas: usize,
    pub one_way_ns: u64,
    pub seed: u64,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            mode: ExecMode::v1(),
            rule: ConflictRule::WoundWait,
            shards: 3,
            replicas: 3,
            one_way_ns: NetConfig::default().one_way_ns,
            seed: 1,
        }
    }
}

impl Setup {
    fn cluster(&self, table_ranges: Vec<(u32, i64, i64)>) -> ClusterConfig {
        ClusterConfig {
            shards: self.shards,
            replicas: self.replicas,
            mode: self.mode,
            rule: self.rule,
            seed: self.seed,
            net: NetConfig { one_way_ns: self.one_way_ns, ..Default::default() },
            table_ranges,
            ..Default::default()
        }
    }
}

/// Bank cluster with `n_acc` accounts (ids 0..n_acc, range-sharded evenly).
pub fn bank_sim(s: &Setup, n_acc: i64) -> Sim {
    bank_sim_bal(s, n_acc, START_BAL)
}

/// Bank cluster whose accounts all start with balance `bal`.
pub fn bank_sim_bal(s: &Setup, n_acc: i64, bal: i64) -> Sim {
    let p = compile(BANK_SQL, "bank.sql");
    let acc = tid(&p, "account");
    let ledger = tid(&p, "ledger");
    let cfg = s.cluster(vec![(acc, 0, n_acc - 1), (ledger, 0, 10_000_000)]);
    let mut sim = Sim::new(p, cfg).expect("bank sim");
    sim.load((0..n_acc).map(|i| {
        (Key { table: acc, pk: i }, vec![Value::Int(i), Value::Int(bal), Value::Str(format!("o{i}"))])
    }));
    sim
}

/// The bank mix of validation.rs: 85% transfer (2% to self), 10% deposit,
/// 5% range audit of 41 accounts.
pub fn bank_call(r: &mut Rng, n_acc: i64) -> Call {
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
}

pub fn bank_total(sim: &Sim, n_acc: i64) -> i64 {
    let acc = tid(&sim.program, "account");
    (0..n_acc)
        .map(|i| sim.committed(Key { table: acc, pk: i }).map_or(0, |r| r[1].as_int().unwrap_or(0)))
        .sum()
}

pub const TPCC_CUSTOMERS: i64 = 100;
pub const TPCC_ITEMS: i64 = 1000;

pub fn tpcc_sim(s: &Setup, w: i64) -> Sim {
    let p = compile(TPCC_SQL, "tpcc_lite.sql");
    let t = |n| tid(&p, n);
    let cfg = s.cluster(vec![
        (t("warehouse"), 0, w - 1),
        (t("district"), 0, w * 10 - 1),
        (t("customer"), 0, (w * 10 - 1) * 3000 + TPCC_CUSTOMERS - 1),
        (t("item"), 1, TPCC_ITEMS),
        (t("stock"), 1, (w - 1) * 100_000 + TPCC_ITEMS),
        (t("orders"), 0, w * 10 * 1_000_000),
        (t("order_line"), 0, w * 10 * 1_000_000 * 16),
    ]);
    let int = Value::Int;
    let mut rows: Vec<(Key, Row)> = vec![];
    for wh in 0..w {
        rows.push((Key { table: t("warehouse"), pk: wh }, vec![int(wh), int(0)]));
        for d in 0..10 {
            let dk = wh * 10 + d;
            rows.push((Key { table: t("district"), pk: dk }, vec![int(dk), int(1), int(0)]));
            for c in 0..TPCC_CUSTOMERS {
                let ck = dk * 3000 + c;
                rows.push((Key { table: t("customer"), pk: ck }, vec![int(ck), int(0), int(0), int(0)]));
            }
        }
        for i in 1..=TPCC_ITEMS {
            let sk = wh * 100_000 + i;
            rows.push((Key { table: t("stock"), pk: sk }, vec![int(sk), int(50), int(0), int(0)]));
        }
    }
    for i in 1..=TPCC_ITEMS {
        rows.push((Key { table: t("item"), pk: i }, vec![int(i), int(i % 50 + 1)]));
    }
    let mut sim = Sim::new(p, cfg).expect("tpcc sim");
    sim.load(rows);
    sim
}

/// new_order with `lines` order lines (items given), or random when `items` is None.
pub fn new_order_call(w: i64, d: i64, c: i64, items: &[(i64, i64)]) -> Call {
    let mut a = vec![w, d, c];
    for l in 0..5 {
        match items.get(l) {
            Some(&(i, q)) => {
                a.push(i);
                a.push(q);
            }
            None => {
                a.push(0);
                a.push(0);
            }
        }
    }
    Call::new("new_order", &a)
}

/// 50% new_order (1..=5 lines, 1% invalid item rollback), 50% payment.
pub fn tpcc_call(r: &mut Rng, w: i64) -> Call {
    let wh = r.range_i64(0, w - 1);
    let d = r.range_i64(0, 9);
    let c = r.range_i64(0, TPCC_CUSTOMERS - 1);
    if r.chance(0.5) {
        let lines = r.range_i64(1, 5) as usize;
        let items: Vec<(i64, i64)> = (0..lines)
            .map(|_| (if r.chance(0.01) { -1 } else { r.range_i64(1, TPCC_ITEMS) }, r.range_i64(1, 10)))
            .collect();
        new_order_call(wh, d, c, &items)
    } else {
        Call::new("payment", &[wh, d, c, r.range_i64(1, 5000)])
    }
}

/// batch.sql + `mv`, `n_acc` accounts with ids 1..=n_acc, job_progress row 1 at 0.
pub fn batch_sim(s: &Setup, n_acc: i64) -> Sim {
    let p = compile(&(BATCH_SQL.to_string() + MV_SQL), "batch.sql+mv");
    let acc = tid(&p, "account");
    let jp = tid(&p, "job_progress");
    let cfg = s.cluster(vec![(acc, 1, n_acc), (jp, 0, 10)]);
    let mut sim = Sim::new(p, cfg).expect("batch sim");
    sim.load((1..=n_acc).map(|i| (Key { table: acc, pk: i }, vec![Value::Int(i), Value::Int(10_000), Value::Null])));
    sim.load([(Key { table: jp, pk: 1 }, vec![Value::Int(1), Value::Int(0)])]);
    sim
}

// ============================================================================ metrics

pub fn percentile_us(v: &mut [u64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_unstable();
    let idx = ((p / 100.0) * (v.len() - 1) as f64).round() as usize;
    v[idx.min(v.len() - 1)] as f64 / 1000.0
}

pub fn lat_us<'a>(calls: impl Iterator<Item = &'a CallStat>, p: f64) -> f64 {
    let mut v: Vec<u64> = calls.map(|c| c.latency_ns()).collect();
    percentile_us(&mut v, p)
}

pub fn mean_us<'a>(calls: impl Iterator<Item = &'a CallStat>) -> f64 {
    let v: Vec<u64> = calls.map(|c| c.latency_ns()).collect();
    if v.is_empty() {
        return f64::NAN;
    }
    v.iter().sum::<u64>() as f64 / v.len() as f64 / 1000.0
}

/// Result of the serializability check of one sim.
#[derive(Clone, Debug)]
pub struct Check {
    pub ok: bool,
    pub violations: usize,
    pub pending_intents: usize,
    pub first: Vec<String>,
}

pub fn check(sim: &Sim) -> Check {
    let r = xdb_checker::check(sim.history());
    Check {
        ok: r.ok,
        violations: r.total_violations,
        pending_intents: sim.pending_intents(),
        first: r.violations.iter().take(3).map(|v| v.to_string()).collect(),
    }
}

// ============================================================================ records

pub const COLUMNS: &[&str] = &[
    "experiment",
    "label",
    "workload",
    "mode",
    "rule",
    "shards",
    "replicas",
    "one_way_us",
    "clients",
    "accounts",
    "warehouses",
    "block",
    "with_batch",
    "seed",
    "duration_ms",
    "sim_ms",
    "calls",
    "ok_calls",
    "err_calls",
    "throughput",
    "mean_us",
    "p50_us",
    "p99_us",
    "max_us",
    "new_order_p50_us",
    "new_order_p99_us",
    "payment_p50_us",
    "payment_p99_us",
    "transfer_p99_us",
    "batch_us",
    "batch_restarts",
    "batch_overlap_pct",
    "restarts",
    "restart_wounded",
    "restart_died",
    "restart_refresh",
    "restart_stage",
    "txn_aborted",
    "txn_committed",
    "aborts_per_commit",
    "max_restarts",
    "messages",
    "mis",
    "one_pc",
    "deltas",
    "routed",
    "follower_mis",
    "refused",
    "escrow_waits",
    "min_balance",
    "invariant_ok",
    "serializable",
    "violations",
    "pending_intents",
    "wall_ms",
];

#[derive(Clone, Debug, Default)]
pub struct Rec {
    vals: HashMap<&'static str, String>,
}

impl Rec {
    pub fn new(experiment: &str, label: &str) -> Rec {
        let mut r = Rec::default();
        r.set("experiment", experiment);
        r.set("label", label);
        r
    }
    pub fn set(&mut self, k: &'static str, v: impl ToString) -> &mut Self {
        assert!(COLUMNS.contains(&k), "unknown column {k}");
        self.vals.insert(k, v.to_string());
        self
    }
    pub fn setf(&mut self, k: &'static str, v: f64) -> &mut Self {
        let s = if v.is_nan() { String::new() } else { format!("{v:.1}") };
        self.set(k, s)
    }
    pub fn get(&self, k: &str) -> &str {
        self.vals.get(k).map(|s| s.as_str()).unwrap_or("")
    }
    pub fn getf(&self, k: &str) -> f64 {
        self.get(k).parse().unwrap_or(f64::NAN)
    }
    pub fn setup(&mut self, s: &Setup) -> &mut Self {
        self.set("mode", s.mode.name())
            .set("rule", rule_name(s.rule))
            .set("shards", s.shards)
            .set("replicas", s.replicas)
            .setf("one_way_us", s.one_way_ns as f64 / 1000.0)
            .set("seed", s.seed)
    }
    pub fn check(&mut self, c: &Check) -> &mut Self {
        self.set("serializable", c.ok).set("violations", c.violations).set("pending_intents", c.pending_intents)
    }
    pub fn serializable(&self) -> bool {
        self.get("serializable") == "true"
    }
    /// Fill the generic metrics of a closed-loop run.
    pub fn report(&mut self, rep: &RunReport, duration_ns: u64) -> &mut Self {
        let calls = &rep.calls;
        let ok = rep.ok_calls();
        let restarts: u64 = calls.iter().map(|c| c.restarts as u64).sum();
        let st = &rep.stats;
        self.setf("duration_ms", duration_ns as f64 / 1e6)
            .setf("sim_ms", rep.sim_ns as f64 / 1e6)
            .set("calls", calls.len())
            .set("ok_calls", ok)
            .set("err_calls", calls.len() - ok)
            .set("throughput", format!("{:.0}", rep.throughput()))
            .setf("mean_us", mean_us(calls.iter()))
            .setf("p50_us", lat_us(calls.iter(), 50.0))
            .setf("p99_us", lat_us(calls.iter(), 99.0))
            .setf("max_us", lat_us(calls.iter(), 100.0))
            .set("restarts", restarts)
            .set("restart_wounded", st.restart_wounded)
            .set("restart_died", st.restart_died)
            .set("restart_refresh", st.restart_refresh)
            .set("restart_stage", st.restart_stage)
            .set("txn_aborted", st.txn_aborted)
            .set("txn_committed", st.txn_committed)
            .set("aborts_per_commit", format!("{:.3}", if ok == 0 { f64::NAN } else { restarts as f64 / ok as f64 }))
            .set("max_restarts", calls.iter().map(|c| c.restarts).max().unwrap_or(0))
            .set("messages", st.messages)
            .set("mis", st.mis)
            .set("one_pc", st.one_pc)
            .set("deltas", st.deltas_issued)
            .set("routed", st.routed)
            .set("follower_mis", st.follower_reads)
    }
}

pub fn rule_name(r: ConflictRule) -> &'static str {
    match r {
        ConflictRule::WoundWait => "wound-wait",
        ConflictRule::WaitDie => "wait-die",
    }
}

pub fn to_csv(recs: &[Rec]) -> String {
    let mut out = COLUMNS.join(",") + "\n";
    for r in recs {
        let line: Vec<String> = COLUMNS
            .iter()
            .map(|c| {
                let v = r.get(c);
                if v.contains(',') || v.contains('"') {
                    format!("\"{}\"", v.replace('"', "\"\""))
                } else {
                    v.to_string()
                }
            })
            .collect();
        out += &line.join(",");
        out.push('\n');
    }
    out
}

/// Markdown table of `cols` (column name, header) over `recs`.
pub fn md_table(recs: &[&Rec], cols: &[(&str, &str)]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "| {} |", cols.iter().map(|c| c.1).collect::<Vec<_>>().join(" | "));
    let _ = writeln!(s, "|{}", cols.iter().map(|_| "---|").collect::<String>());
    for r in recs {
        let _ = writeln!(s, "| {} |", cols.iter().map(|c| r.get(c.0).to_string()).collect::<Vec<_>>().join(" | "));
    }
    s
}
