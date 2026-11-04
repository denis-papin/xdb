//! xdb-bench: runs the POC experiments and writes `results.md` and `results.csv`.
//!
//! Usage: xdb-bench [--out <dir>] [--quick] [--only <1..6>[,..]]
//!
//! Every run is a fresh deterministic simulation; every run that executes
//! transactions is checked by xdb-checker. Exit code 1 if any run is not
//! serializable (or breaks a workload invariant).

use std::fmt::Write as _;
use std::time::Instant;
use xdb_bench::*;
use xdb_core::*;
use xdb_ir::Key;

struct Opts {
    out: String,
    quick: bool,
    only: Vec<u32>,
}

impl Opts {
    /// Simulated closed-loop duration (ns).
    fn dur(&self, full_ms: u64) -> u64 {
        let ms = if self.quick { (full_ms / 10).max(2) } else { full_ms };
        ms * 1_000_000
    }
    fn idle_reps(&self) -> usize {
        if self.quick { 5 } else { 20 }
    }
}

fn parse_args() -> Opts {
    let mut o = Opts { out: "bench".into(), quick: false, only: vec![] };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => o.out = it.next().expect("--out <dir>"),
            "--quick" => o.quick = true,
            "--only" => {
                o.only = it.next().expect("--only <list>").split(',').map(|x| x.trim().parse().expect("number")).collect()
            }
            "-h" | "--help" => {
                println!("xdb-bench [--out <dir>] [--quick] [--only 1,2,..]");
                std::process::exit(0);
            }
            x => {
                eprintln!("unknown argument {x}");
                std::process::exit(2);
            }
        }
    }
    o
}

const BANK_ACC_IDLE: i64 = 300;
const CLIENTS: usize = 64;
const SEED: u64 = 42;

fn finish(rec: &mut Rec, sim: &Sim, t0: Instant) {
    let c = check(sim);
    if !c.ok {
        eprintln!("NOT SERIALIZABLE: {} {}: {} violations", rec.get("experiment"), rec.get("label"), c.violations);
        for v in &c.first {
            eprintln!("  {v}");
        }
    }
    rec.check(&c).set("wall_ms", t0.elapsed().as_millis());
    eprintln!(
        "  [{}] {:40} thr {:>8} p50 {:>8} p99 {:>9} ser {} ({} ms)",
        rec.get("experiment"),
        rec.get("label"),
        rec.get("throughput"),
        rec.get("p50_us"),
        rec.get("p99_us"),
        rec.get("serializable"),
        rec.get("wall_ms")
    );
}

// ---------------------------------------------------------------- shared runs

/// `reps` sequential calls of `call` on an idle cluster from `node`.
fn idle(exp: &str, label: &str, setup: &Setup, mut sim: Sim, node: usize, call: &Call, reps: usize, workload: &str) -> Rec {
    let t0 = Instant::now();
    let stats: Vec<CallStat> = (0..reps).map(|_| sim.call(node, call.clone())).collect();
    let mut rec = Rec::new(exp, label);
    rec.setup(setup)
        .set("workload", workload)
        .set("clients", 1)
        .set("calls", stats.len())
        .set("ok_calls", stats.iter().filter(|c| c.result.is_ok()).count())
        .set("err_calls", stats.iter().filter(|c| c.result.is_err()).count())
        .setf("mean_us", mean_us(stats.iter()))
        .setf("p50_us", lat_us(stats.iter(), 50.0))
        .setf("max_us", lat_us(stats.iter(), 100.0))
        .set("restarts", stats.iter().map(|c| c.restarts as u64).sum::<u64>())
        .set("invariant_ok", stats.iter().all(|c| c.result.is_ok()));
    finish(&mut rec, &sim, t0);
    rec
}

fn idle_transfer(exp: &str, setup: &Setup, reps: usize) -> Rec {
    // Accounts 10 and 150 of 300 are on shards 0 and 1; the call runs on node 2.
    let sim = bank_sim(setup, BANK_ACC_IDLE);
    let mut r = idle(exp, &format!("transfer idle {}", setup.mode.name()), setup, sim, 2, &Call::new("transfer", &[10, 150, 5]), reps, "bank");
    r.set("accounts", BANK_ACC_IDLE);
    r
}

fn bank_run(exp: &str, label: &str, setup: &Setup, n_acc: i64, clients: usize, dur: u64) -> Rec {
    let t0 = Instant::now();
    let mut sim = bank_sim(setup, n_acc);
    let mut gen = |r: &mut Rng, _c: usize| bank_call(r, n_acc);
    let rep = sim.run_closed_loop(clients, dur, 0, &mut gen);
    let mut rec = Rec::new(exp, label);
    rec.setup(setup).set("workload", "bank").set("clients", clients).set("accounts", n_acc).report(&rep, dur);
    let unexpected = rep.calls.iter().any(|c| matches!(&c.result, Err((code, _)) if *code != -20001));
    let total_ok = bank_total(&sim, n_acc) == n_acc * START_BAL;
    rec.set("invariant_ok", total_ok && !unexpected);
    finish(&mut rec, &sim, t0);
    rec
}

// ---------------------------------------------------------------- experiments

fn exp1(o: &Opts, recs: &mut Vec<Rec>) {
    eprintln!("== 1. idle latency per mode");
    for mode in ExecMode::all() {
        let s = Setup { mode, seed: SEED, ..Default::default() };
        recs.push(idle_transfer("1-idle", &s, o.idle_reps()));
        // TPC-C lite, 1 warehouse: 5-line new_order over items on all 3 stock shards.
        let no = new_order_call(0, 0, 7, &[(1, 5), (250, 5), (500, 5), (750, 5), (999, 5)]);
        let mut r = idle("1-idle", &format!("new_order idle {}", mode.name()), &s, tpcc_sim(&s, 1), 0, &no, o.idle_reps(), "tpcc_lite");
        r.set("warehouses", 1);
        recs.push(r);
        let pay = Call::new("payment", &[0, 5, 7, 100]);
        let mut r = idle("1-idle", &format!("payment idle {}", mode.name()), &s, tpcc_sim(&s, 1), 0, &pay, o.idle_reps(), "tpcc_lite");
        r.set("warehouses", 1);
        recs.push(r);
    }
}

fn exp2(o: &Opts, recs: &mut Vec<Rec>) {
    eprintln!("== 2. cluster size");
    for shards in 1..=5 {
        for replicas in [2, 3, 4] {
            for mode in [ExecMode::naive(), ExecMode::v1(), ExecMode::speculative()] {
                let s = Setup { mode, shards, replicas, seed: SEED, ..Default::default() };
                let label = format!("bank {shards}s x {replicas}r {}", mode.name());
                recs.push(bank_run("2-cluster", &label, &s, 10_000, CLIENTS, o.dur(100)));
            }
        }
    }
}

fn exp3(o: &Opts, recs: &mut Vec<Rec>) {
    eprintln!("== 3. contention sweep");
    for n_acc in [10_000, 1000, 100, 10] {
        for rule in [ConflictRule::WoundWait, ConflictRule::WaitDie] {
            for mode in [ExecMode::v1(), ExecMode::speculative()] {
                let s = Setup { mode, rule, seed: SEED, ..Default::default() };
                let label = format!("bank {n_acc} acc {} {}", mode.name(), rule_name(rule));
                recs.push(bank_run("3-contention", &label, &s, n_acc, CLIENTS, o.dur(100)));
            }
        }
    }
}

fn exp4(o: &Opts, recs: &mut Vec<Rec>) {
    eprintln!("== 4. TPC-C lite");
    for w in [1, 4] {
        for mode in ExecMode::all() {
            let t0 = Instant::now();
            let s = Setup { mode, seed: SEED, ..Default::default() };
            let mut sim = tpcc_sim(&s, w);
            let mut gen = |r: &mut Rng, _c: usize| tpcc_call(r, w);
            let dur = o.dur(100);
            let rep = sim.run_closed_loop(CLIENTS, dur, 0, &mut gen);
            let mut rec = Rec::new("4-tpcc", &format!("tpcc {w}w {}", mode.name()));
            rec.setup(&s).set("workload", "tpcc_lite").set("clients", CLIENTS).set("warehouses", w).report(&rep, dur);
            let by = |p: &'static str| rep.calls.iter().filter(move |c| c.proc == p);
            rec.setf("new_order_p50_us", lat_us(by("new_order"), 50.0))
                .setf("new_order_p99_us", lat_us(by("new_order"), 99.0))
                .setf("payment_p50_us", lat_us(by("payment"), 50.0))
                .setf("payment_p99_us", lat_us(by("payment"), 99.0));
            // Invariants: only the 1% invalid-item rollback may fail; warehouse ytd =
            // sum of its districts' ytd; next_o_id - 1 = orders of the district.
            let unexpected = rep.calls.iter().any(|c| matches!(&c.result, Err((code, _)) if *code != -20002));
            let fs = xdb_checker::final_state(sim.history());
            let t = |n| tid(&sim.program, n);
            let int = |k: Key, col: usize| fs.get(&k).and_then(|r| r[col].as_int()).unwrap_or(i64::MIN);
            let mut inv = !unexpected;
            for wh in 0..w {
                let wy = int(Key { table: t("warehouse"), pk: wh }, 1);
                let dy: i64 = (0..10).map(|d| int(Key { table: t("district"), pk: wh * 10 + d }, 2)).sum();
                inv &= wy == dy;
                for d in 0..10 {
                    let dk = wh * 10 + d;
                    let next = int(Key { table: t("district"), pk: dk }, 1);
                    let n = fs.keys().filter(|k| k.table == t("orders") && k.pk / 1_000_000 == dk).count() as i64;
                    inv &= next - 1 == n;
                }
            }
            rec.set("invariant_ok", inv);
            finish(&mut rec, &sim, t0);
            recs.push(rec);
        }
    }
}

fn exp5(o: &Opts, recs: &mut Vec<Rec>) {
    eprintln!("== 5. network sensitivity");
    for one_way_us in [5u64, 25, 50] {
        for mode in [ExecMode::v1(), ExecMode::speculative()] {
            let s = Setup { mode, one_way_ns: one_way_us * 1000, seed: SEED, ..Default::default() };
            let mut r = idle_transfer("5-network", &s, o.idle_reps());
            r.set("label", format!("transfer idle {one_way_us}us {}", mode.name()));
            recs.push(r);
            let label = format!("bank 10000 acc {one_way_us}us {}", mode.name());
            recs.push(bank_run("5-network", &label, &s, 10_000, CLIENTS, o.dur(100)));
        }
    }
}

const BATCH_ACC_FULL: i64 = 5000;
const BATCH_ACC_QUICK: i64 = 1000;
const OLTP_CLIENTS: usize = 32;

fn exp6(o: &Opts, recs: &mut Vec<Rec>) {
    let n_acc = if o.quick { BATCH_ACC_QUICK } else { BATCH_ACC_FULL };
    eprintln!("== 6. batch vs OLTP");
    for mode in [ExecMode::v1(), ExecMode::speculative()] {
        for block in [0i64, 10, 100, 1000] {
            // block 0 = baseline without the batch.
            let t0 = Instant::now();
            let s = Setup { mode, seed: SEED, ..Default::default() };
            let mut sim = batch_sim(&s, n_acc);
            let with_batch = block > 0;
            let batch_client = OLTP_CLIENTS; // the extra client
            let mut started = false;
            let mut gen = |r: &mut Rng, c: usize| {
                if with_batch && c == batch_client && !started {
                    started = true;
                    return Call::new("monthly_interest", &[1, block]).with_class(2);
                }
                Call::new("mv", &[r.range_i64(1, n_acc), r.range_i64(1, n_acc), 7])
            };
            let clients = OLTP_CLIENTS + with_batch as usize;
            let dur = o.dur(250);
            let rep = sim.run_closed_loop(clients, dur, 0, &mut gen);
            let label = if with_batch { format!("batch block {block} {}", mode.name()) } else { format!("no batch {}", mode.name()) };
            let mut rec = Rec::new("6-batch", &label);
            rec.setup(&s)
                .set("workload", "batch+mv")
                .set("clients", clients)
                .set("accounts", n_acc)
                .set("with_batch", with_batch)
                .report(&rep, dur);
            let batch = rep.calls.iter().find(|c| c.proc == "monthly_interest");
            // Transfers of the 32 OLTP clients that started while the batch ran.
            let window_end = batch.map_or(u64::MAX, |b| b.end_ns);
            let oltp = || {
                rep.calls
                    .iter()
                    .filter(|c| c.proc == "mv" && c.client.map_or(false, |x| x < OLTP_CLIENTS) && c.start_ns < window_end)
            };
            rec.setf("transfer_p99_us", lat_us(oltp(), 99.0)).setf("p50_us", lat_us(oltp(), 50.0));
            // Transfer throughput while the batch ran (or over the whole run for the baseline).
            let w_end = window_end.min(oltp().map(|c| c.end_ns).max().unwrap_or(0));
            let n_ok = oltp().filter(|c| c.result.is_ok()).count();
            rec.set("throughput", format!("{:.0}", n_ok as f64 / (w_end.max(1) as f64 / 1e9)));
            let mut inv = rep.calls.iter().all(|c| c.result.is_ok());
            if let Some(b) = batch {
                rec.setf("batch_us", b.latency_ns() as f64 / 1000.0).set("block", block).set("batch_restarts", b.restarts);
                // Share of the batch duration during which the OLTP clients were still issuing calls.
                rec.setf("batch_overlap_pct", 100.0 * (dur.saturating_sub(b.start_ns) as f64 / b.latency_ns().max(1) as f64).min(1.0));
                let jp = tid(&sim.program, "job_progress");
                inv &= sim.committed(Key { table: jp, pk: 1 }).and_then(|r| r[1].as_int()) == Some(n_acc);
            }
            rec.set("invariant_ok", inv);
            finish(&mut rec, &sim, t0);
            recs.push(rec);
        }
    }
}

// ---------------------------------------------------------------- report

fn section(md: &mut String, title: &str, expl: &str, recs: &[Rec], exp: &str, cols: &[(&str, &str)]) {
    let rows: Vec<&Rec> = recs.iter().filter(|r| r.get("experiment") == exp).collect();
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(md, "## {title}\n\n{expl}\n");
    md.push_str(&md_table(&rows, cols));
    md.push('\n');
}

fn write_md(o: &Opts, recs: &[Rec], wall_s: f64) -> String {
    let mut md = String::new();
    let all_ser = recs.iter().all(|r| r.serializable());
    let all_inv = recs.iter().all(|r| r.get("invariant_ok") != "false");
    let _ = writeln!(md, "# xdb POC benchmark results\n");
    let _ = writeln!(
        md,
        "Generated by `xdb-bench{}` in {:.1} s wall time. {} runs; all serializable (xdb-checker): **{}**; all workload invariants hold: **{}**.\n",
        if o.quick { " --quick" } else { "" },
        wall_s,
        recs.len(),
        if all_ser { "yes" } else { "NO" },
        if all_inv { "yes" } else { "NO" }
    );
    let n = NetConfig::default();
    let c = CpuConfig::default();
    let _ = writeln!(
        md,
        "Simulated cluster (deterministic, seed {SEED}): {} us one-way latency, {} us mean jitter, {} us fsync, {} us per micro-instruction, {} us coordinator overhead per call. \
Commit waits for a majority of replicas (leader included): 2 replicas = both, 3 = 2, 4 = 3. Latencies are simulated microseconds; throughput is committed calls per simulated second. \
`restarts` = transaction-block restarts (wounded / died / refresh / stage); `aborts/commit` = restarts per successful call. Full per-run data: `results.csv`.\n",
        n.one_way_ns / 1000,
        n.jitter_mean_ns / 1000,
        n.fsync_ns / 1000,
        c.mi_ns / 1000,
        c.call_overhead_ns / 1000
    );

    let idle_cols = [("label", "call"), ("mode", "mode"), ("calls", "calls"), ("mean_us", "mean us"), ("p50_us", "p50 us"), ("max_us", "max us"), ("serializable", "serializable")];
    section(
        &mut md,
        "1. Idle latency per mode",
        "Sequential single calls on an idle 3-shard x 3-replica cluster (transfer between accounts on shards 0 and 1, called from node 2; new_order with 5 lines; payment); the doc (§9) estimates transfer at 430 / 200 / 150 us for naive / v1 / MI.",
        recs,
        "1-idle",
        &idle_cols,
    );

    let thr_cols = [
        ("shards", "shards"),
        ("replicas", "replicas"),
        ("mode", "mode"),
        ("calls", "calls"),
        ("throughput", "calls/s"),
        ("p50_us", "p50 us"),
        ("p99_us", "p99 us"),
        ("restarts", "restarts"),
        ("serializable", "serializable"),
    ];
    section(
        &mut md,
        "2. Cluster size",
        "Bank mix (85% transfer, 10% deposit, 5% 41-row audit) over 10,000 accounts, 64 closed-loop clients; 2 replicas commit only when both acks arrive (majority of 2), so they are not cheaper than 3 (the replica count only changes which replication-delay sample the commit waits for).",
        recs,
        "2-cluster",
        &thr_cols,
    );

    let cont_cols = [
        ("accounts", "accounts"),
        ("mode", "mode"),
        ("rule", "rule"),
        ("throughput", "calls/s"),
        ("p50_us", "p50 us"),
        ("p99_us", "p99 us"),
        ("aborts_per_commit", "aborts/commit"),
        ("max_restarts", "max restarts"),
        ("restart_wounded", "wounded"),
        ("restart_died", "died"),
        ("serializable", "serializable"),
    ];
    section(
        &mut md,
        "3. Contention sweep",
        "Bank mix, 3 shards x 3 replicas, 64 clients; fewer accounts = more conflicts (10 accounts is a hotspot); compares v1 vs speculative and wound-wait vs wait-die.",
        recs,
        "3-contention",
        &cont_cols,
    );

    let tpcc_cols = [
        ("warehouses", "warehouses"),
        ("mode", "mode"),
        ("throughput", "calls/s"),
        ("new_order_p50_us", "new_order p50"),
        ("new_order_p99_us", "new_order p99"),
        ("payment_p50_us", "payment p50"),
        ("payment_p99_us", "payment p99"),
        ("restarts", "restarts"),
        ("serializable", "serializable"),
    ];
    section(
        &mut md,
        "4. TPC-C lite",
        "50% new_order (1-5 lines, 1% invalid-item rollback) / 50% payment, 3 shards x 3 replicas, 64 clients; 1 warehouse = 1 hot warehouse row (every payment) and 10 hot district rows, 4 warehouses = 4 and 40; parallel = speculative here because the only guard (`v_i = -1`) depends on arguments only.",
        recs,
        "4-tpcc",
        &tpcc_cols,
    );

    let net_cols = [
        ("label", "run"),
        ("one_way_us", "one-way us"),
        ("mode", "mode"),
        ("mean_us", "mean us"),
        ("throughput", "calls/s"),
        ("p50_us", "p50 us"),
        ("p99_us", "p99 us"),
        ("serializable", "serializable"),
    ];
    section(
        &mut md,
        "5. Network sensitivity",
        "Idle transfer latency (mean of sequential calls) and bank throughput (10,000 accounts, 64 clients) with 5 us (RDMA-like), 25 us and 50 us one-way latency, v1 vs speculative.",
        recs,
        "5-network",
        &net_cols,
    );

    let batch_cols = [
        ("label", "run"),
        ("mode", "mode"),
        ("block", "block"),
        ("batch_us", "batch duration us"),
        ("batch_restarts", "batch restarts"),
        ("batch_overlap_pct", "OLTP overlap %"),
        ("throughput", "transfers/s"),
        ("p50_us", "transfer p50 us"),
        ("transfer_p99_us", "transfer p99 us"),
        ("serializable", "serializable"),
    ];
    section(
        &mut md,
        "6. Batch vs OLTP",
        "monthly_interest over 5,000 accounts (1,000 with --quick) (class 2 = batch priority, COMMIT every `block` rows) alongside 32 clients of two-row transfers (class 1); transfer latency is measured over transfers started while the batch ran, and `no batch` is the baseline (doc §5.5/§7.4); v1 = speculative here because neither procedure has a guard or independent reads to reorder.",
        recs,
        "6-batch",
        &batch_cols,
    );
    md
}

fn main() {
    let o = parse_args();
    let t0 = Instant::now();
    let mut recs: Vec<Rec> = vec![];
    let exps: [(u32, fn(&Opts, &mut Vec<Rec>)); 6] = [(1, exp1), (2, exp2), (3, exp3), (4, exp4), (5, exp5), (6, exp6)];
    for (n, f) in exps {
        if o.only.is_empty() || o.only.contains(&n) {
            f(&o, &mut recs);
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    std::fs::create_dir_all(&o.out).expect("create output dir");
    let md = write_md(&o, &recs, wall);
    std::fs::write(format!("{}/results.md", o.out), md).expect("write results.md");
    std::fs::write(format!("{}/results.csv", o.out), to_csv(&recs)).expect("write results.csv");
    let bad_ser: Vec<&Rec> = recs.iter().filter(|r| !r.serializable()).collect();
    let bad_inv: Vec<&Rec> = recs.iter().filter(|r| r.get("invariant_ok") == "false").collect();
    eprintln!("{} runs in {wall:.1} s, results in {}/results.{{md,csv}}", recs.len(), o.out);
    for r in &bad_ser {
        eprintln!("NOT SERIALIZABLE: {} {}", r.get("experiment"), r.get("label"));
    }
    for r in &bad_inv {
        eprintln!("INVARIANT BROKEN: {} {}", r.get("experiment"), r.get("label"));
    }
    if !bad_ser.is_empty() || !bad_inv.is_empty() {
        std::process::exit(1);
    }
}
