//! Deterministic discrete-event simulation of the cluster: coordinators executing
//! procedures as micro-instructions, shards, LAN latencies and replication.

use crate::code::{lower, Code, DataOp, Ins};
use crate::config::ClusterConfig;
use crate::eval::{eval, subst, Env, Slot, VarsEnv};
use crate::rng::Rng;
use crate::shard::{Mi, Shard, ShardStats};
use crate::types::*;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::rc::Rc;
use xdb_ir::*;

// ============================================================================ public API

#[derive(Clone, Debug)]
pub struct Call {
    pub proc: String,
    pub args: Vec<Value>,
    /// Priority class: 0 system, 1 interactive (default), 2 batch.
    pub class: u8,
}

impl Call {
    pub fn new(proc: &str, args: &[i64]) -> Self {
        Call { proc: proc.to_string(), args: args.iter().map(|a| Value::Int(*a)).collect(), class: 1 }
    }
    pub fn with_class(mut self, class: u8) -> Self {
        self.class = class;
        self
    }
}

#[derive(Clone, Debug)]
pub struct CallStat {
    pub client: Option<usize>,
    pub node: NodeId,
    pub proc: String,
    pub start_ns: u64,
    pub end_ns: u64,
    pub result: Result<(), (i32, String)>,
    /// Transaction-block restarts (wounds, refresh failures, ...).
    pub restarts: u32,
    pub output: Vec<String>,
}

impl CallStat {
    pub fn latency_ns(&self) -> u64 {
        self.end_ns - self.start_ns
    }
}

#[derive(Clone, Debug, Default)]
pub struct SimStats {
    pub calls_ok: u64,
    pub calls_err: u64,
    pub txn_committed: u64,
    pub txn_aborted: u64,
    pub restart_wounded: u64,
    pub restart_died: u64,
    pub restart_refresh: u64,
    pub restart_stage: u64,
    pub user_rollbacks: u64,
    pub messages: u64,
    pub mis: u64,
    pub lookahead_issued: u64,
    pub refreshes: u64,
    pub one_pc: u64,
    pub one_pc_fallback: u64,
    pub deltas_issued: u64,
    pub follower_reads: u64,
    pub routed: u64,
    pub restart_check: u64,
}

#[derive(Clone, Debug)]
pub struct RunReport {
    pub mode: String,
    pub sim_ns: u64,
    pub calls: Vec<CallStat>,
    pub stats: SimStats,
    pub shard_stats: Vec<ShardStats>,
}

impl RunReport {
    pub fn ok_calls(&self) -> usize {
        self.calls.iter().filter(|c| c.result.is_ok()).count()
    }
    /// Successful calls per simulated second.
    pub fn throughput(&self) -> f64 {
        if self.sim_ns == 0 {
            return 0.0;
        }
        self.ok_calls() as f64 / (self.sim_ns as f64 / 1e9)
    }
    /// Latency percentile (0..=100) in microseconds over all calls of `proc`
    /// (`None` = all procedures).
    pub fn latency_us(&self, p: f64, proc: Option<&str>) -> f64 {
        let mut v: Vec<u64> = self
            .calls
            .iter()
            .filter(|c| proc.map_or(true, |n| c.proc == n))
            .map(|c| c.latency_ns())
            .collect();
        if v.is_empty() {
            return 0.0;
        }
        v.sort_unstable();
        let idx = ((p / 100.0) * (v.len() - 1) as f64).round() as usize;
        v[idx.min(v.len() - 1)] as f64 / 1000.0
    }
}

// ============================================================================ internals

enum Event {
    Deliver { from: Ep, to: Ep, msg: Msg },
    ShardService(ShardId),
    Run(SessionId),
    Client(usize),
    FollowerService(ShardId, NodeId),
    FollowerRetry(ShardId, NodeId, Mi),
}

#[derive(Clone, Debug)]
struct Cursor {
    rows: Vec<Row>,
    pos: usize,
    for_update: bool,
    epoch: u32,
}

#[derive(Clone, Debug)]
struct Frame {
    def: usize,
    sp: u32,
}

#[derive(Clone, Debug)]
struct ExecState {
    pc: usize,
    vars: Vec<Slot>,
    cursors: Vec<Option<Cursor>>,
    frames: Vec<Frame>,
    output: Vec<String>,
    epoch: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    None,
    Op(u32),
    Commit,
    Backoff,
}

#[derive(Debug)]
#[allow(dead_code)]
struct Op {
    pc: usize,
    parts: usize,
    got: usize,
    ready: bool,
    cancelled: bool,
    retired: bool,
    for_update: bool,
    err: Option<i32>,
    key: Option<Key>,
    table: TableId,
    lo: i64,
    limit: Option<u64>,
    read_value: Option<Row>,
    applied: bool,
    scan_parts: Vec<(Vec<Row>, Vec<(i64, Option<TxnId>)>, i64)>,
    rows: Vec<Row>,
    dur_expected: usize,
    dur_got: usize,
    part_shards: Vec<ShardId>,
}

#[derive(Debug, PartialEq, Eq)]
enum Phase {
    None,
    Refresh { pending: usize, ok: bool },
    Stage { staged: bool },
    Prepare { pending: usize, ok: bool },
    Record { done: bool },
    /// A one-phase commit was sent with the last writes.
    OnePc,
}

struct Attempt {
    txn: TxnId,
    read_ts: Ts,
    write_ts: Ts,
    next_seq: u32,
    ops: HashMap<u32, Op>,
    at_pc: HashMap<usize, u32>,
    early: HashMap<usize, Slot>,
    shards: BTreeSet<ShardId>,
    intent_shards: BTreeSet<ShardId>,
    reads: Vec<(u32, ReadEvent)>,
    range_reads: Vec<(u32, RangeReadEvent)>,
    wlog: Vec<(u32, Key, Option<Row>)>,
    spans: Vec<(ShardId, Span)>,
    outbox: BTreeMap<ShardId, Vec<(u32, ShardOp)>>,
    phase: Phase,
    deferred_err: Option<(i32, String)>,
    tried_1pc: bool,
    /// Read-only attempt served by follower replicas at a closed timestamp.
    follower: bool,
}

impl Attempt {
    fn new(txn: TxnId, ts: Ts) -> Self {
        Attempt {
            txn,
            read_ts: ts,
            write_ts: ts,
            next_seq: 1,
            ops: HashMap::new(),
            at_pc: HashMap::new(),
            early: HashMap::new(),
            shards: BTreeSet::new(),
            intent_shards: BTreeSet::new(),
            reads: vec![],
            range_reads: vec![],
            wlog: vec![],
            spans: vec![],
            outbox: BTreeMap::new(),
            phase: Phase::None,
            deferred_err: None,
            tried_1pc: false,
            follower: false,
        }
    }
    fn outstanding(&self) -> usize {
        self.ops.values().filter(|o| !o.ready && !o.cancelled).count()
    }
    fn all_durable(&self) -> bool {
        self.ops.values().all(|o| o.cancelled || o.dur_got >= o.dur_expected)
    }
}

struct Session {
    sid: SessionId,
    node: NodeId,
    client: Option<usize>,
    start: u64,
    code: Rc<Code>,
    class: u8,
    st: ExecState,
    snap: ExecState,
    block_prio: Option<Priority>,
    att: Option<Attempt>,
    wait: Wait,
    restarts: u32,
    out_committed: Vec<String>,
    finished: bool,
}

struct Remote {
    parts: Vec<(ShardId, ShardOp)>,
    key: Option<Key>,
    table: TableId,
    lo: i64,
    limit: Option<u64>,
    for_update: bool,
}

enum Prep {
    NoData,
    Rowcount0,
    Remote(Remote),
}

pub struct Sim {
    pub cfg: ClusterConfig,
    pub program: Program,
    codes: Vec<Rc<Code>>,
    now: u64,
    ev_seq: u64,
    heap: BinaryHeap<Reverse<(u64, u64)>>,
    events: HashMap<u64, Event>,
    chan_last: HashMap<(Ep, Ep), u64>,
    shards: Vec<Shard>,
    g: Global,
    hlc: Vec<Ts>,
    rng: Rng,
    wl_rng: Rng,
    sessions: Vec<Option<Session>>,
    next_txn: TxnId,
    seq_state: HashMap<(NodeId, SeqId), u64>,
    history: History,
    hist_index: HashMap<TxnId, usize>,
    early_patches: HashMap<TxnId, Vec<(Key, Option<Row>)>>,
    /// Follower-read queues (one core per replica), keyed by (shard, node).
    followers: HashMap<(ShardId, NodeId), (std::collections::VecDeque<Mi>, bool)>,
    follower_seq: u32,
    calls: Vec<CallStat>,
    closed: Option<(u64, u64)>,
    pub stats: SimStats,
    /// Safety net: panic (with diagnostics) after this many events.
    pub event_limit: u64,
    events_done: u64,
}

impl Sim {
    pub fn new(program: Program, cfg: ClusterConfig) -> Result<Sim, String> {
        assert!(cfg.shards >= 1 && cfg.replicas >= 1);
        let codes = program
            .procedures
            .iter()
            .map(|p| lower(p, cfg.mode.implicit_sfu, cfg.mode.escrow).map(Rc::new))
            .collect::<Result<Vec<_>, _>>()?;
        let shards = (0..cfg.shards)
            .map(|i| {
                let mut s = Shard::new(i, cfg.rule);
                s.fault_ignore_read_ts = cfg.fault_ignore_read_ts;
                s.checks = program.schema.tables.iter().filter(|t| !t.checks.is_empty()).map(|t| (t.id, t.checks.clone())).collect();
                s
            })
            .collect();
        let nodes = cfg.shards;
        Ok(Sim {
            rng: Rng::new(cfg.seed),
            wl_rng: Rng::new(cfg.seed.wrapping_mul(31).wrapping_add(7)),
            cfg,
            program,
            codes,
            now: 0,
            ev_seq: 0,
            heap: BinaryHeap::new(),
            events: HashMap::new(),
            chan_last: HashMap::new(),
            shards,
            g: Global::default(),
            hlc: (0..nodes).map(|n| Ts { wall: 0, logical: 0, node: n as u16 }).collect(),
            sessions: vec![],
            next_txn: 1,
            seq_state: HashMap::new(),
            history: History::default(),
            hist_index: HashMap::new(),
            early_patches: HashMap::new(),
            followers: HashMap::new(),
            follower_seq: 0,
            calls: vec![],
            closed: None,
            stats: SimStats::default(),
            event_limit: 200_000_000,
            events_done: 0,
        })
    }

    pub fn nodes(&self) -> usize {
        self.cfg.shards
    }
    pub fn now_ns(&self) -> u64 {
        self.now
    }
    pub fn history(&self) -> &History {
        &self.history
    }
    pub fn shard_stats(&self) -> Vec<ShardStats> {
        self.shards.iter().map(|s| s.stats.clone()).collect()
    }
    /// Latest committed row of a key (after the simulation has quiesced).
    pub fn committed(&self, key: Key) -> Option<Row> {
        self.shards[self.shard_of(key)].latest(&key).cloned()
    }
    /// Intents still present (should be 0 once quiesced).
    pub fn pending_intents(&self) -> usize {
        self.shards.iter().map(|s| s.pending_intents()).sum()
    }

    /// Human-readable state of the 5 oldest live sessions and of every intent.
    pub fn debug_dump(&self) -> String {
        let mut out = String::new();
        let mut live: Vec<&Session> = self.sessions.iter().flatten().collect();
        live.sort_by_key(|s| s.block_prio);
        let holders: HashSet<TxnId> = self
            .shards
            .iter()
            .flat_map(|sh| sh.data.values().filter_map(|r| r.intent.as_ref().map(|i| i.txn)))
            .collect();
        let pick: Vec<&&Session> = live
            .iter()
            .enumerate()
            .filter(|(i, s)| *i < 5 || s.att.as_ref().map_or(false, |a| holders.contains(&a.txn)))
            .map(|(_, s)| s)
            .collect();
        for s in pick {
            let att = s.att.as_ref().map(|a| {
                let pending: Vec<_> = a.ops.iter().filter(|(_, o)| !o.ready && !o.cancelled).map(|(q, o)| (*q, o.key)).collect();
                format!(
                    "txn {} status {:?} phase {:?} pending {:?} restarts {}",
                    a.txn,
                    self.g.status(a.txn),
                    a.phase,
                    pending,
                    s.restarts
                )
            });
            out += &format!("session {} prio {:?} pc {} wait {:?} {:?}\n", s.sid, s.block_prio, s.st.pc, s.wait, att);
        }
        for sh in &self.shards {
            for (k, r) in &sh.data {
                if let Some(i) = &r.intent {
                    out += &format!(
                        "shard {} key {:?} intent txn {} prio {:?} status {:?}\n",
                        sh.id,
                        k,
                        i.txn,
                        i.prio,
                        self.g.status(i.txn)
                    );
                }
            }
            out += &format!("shard {} {}\n", sh.id, sh.debug_queues());
        }
        out
    }

    /// Load initial rows (before any transaction).
    pub fn load(&mut self, rows: impl IntoIterator<Item = (Key, Row)>) {
        for (k, r) in rows {
            let s = self.shard_of(k);
            self.shards[s].load(k, r.clone());
            self.history.initial.push((k, r));
        }
    }

    /// Run one call to completion on an otherwise idle cluster.
    pub fn call(&mut self, node: NodeId, call: Call) -> CallStat {
        let n = self.calls.len();
        self.submit(node, &call, None);
        self.drain(None);
        self.calls[n].clone()
    }

    /// Closed loop: `clients` clients, each issuing a new call as soon as the previous
    /// one finishes (+ `think_ns`), during `duration_ns`; then drain.
    pub fn run_closed_loop(
        &mut self,
        clients: usize,
        duration_ns: u64,
        think_ns: u64,
        gen: &mut dyn FnMut(&mut Rng, usize) -> Call,
    ) -> RunReport {
        let first = self.calls.len();
        let start = self.now;
        self.closed = Some((start + duration_ns, think_ns));
        for c in 0..clients {
            let t = self.now + self.rng.below(20_000);
            self.schedule(t, Event::Client(c));
        }
        self.drain(Some(gen));
        self.closed = None;
        let end = self.calls[first..].iter().map(|c| c.end_ns).max().unwrap_or(start);
        RunReport {
            mode: self.cfg.mode.name(),
            sim_ns: end.saturating_sub(start).max(1),
            calls: self.calls[first..].to_vec(),
            stats: self.stats.clone(),
            shard_stats: self.shard_stats(),
        }
    }

    // ------------------------------------------------------------------ sharding

    fn table_range(&self, t: TableId) -> (i64, i64) {
        self.cfg
            .table_ranges
            .iter()
            .find(|r| r.0 == t)
            .map(|r| (r.1, r.2))
            .unwrap_or(self.cfg.default_range)
    }

    /// Lower bound of each shard's part of the table range (shard 0 starts at -inf).
    fn bounds(&self, t: TableId) -> Vec<i64> {
        let (lo, hi) = self.table_range(t);
        let n = self.cfg.shards as i128;
        let span = (hi as i128 - lo as i128 + 1).max(1);
        (0..self.cfg.shards)
            .map(|i| if i == 0 { i64::MIN } else { (lo as i128 + span * i as i128 / n) as i64 })
            .collect()
    }

    pub fn shard_of(&self, k: Key) -> ShardId {
        let b = self.bounds(k.table);
        b.iter().rposition(|lo| k.pk >= *lo).unwrap_or(0)
    }

    fn shards_for_range(&self, t: TableId, lo: i64, hi: i64) -> Vec<(ShardId, i64, i64)> {
        if lo > hi {
            return vec![];
        }
        let b = self.bounds(t);
        let mut out = vec![];
        for i in 0..b.len() {
            let s_lo = b[i];
            let s_hi = if i + 1 < b.len() { b[i + 1] - 1 } else { i64::MAX };
            let a = lo.max(s_lo);
            let z = hi.min(s_hi);
            if a <= z {
                out.push((i, a, z));
            }
        }
        out
    }

    // ------------------------------------------------------------------ events & net

    fn schedule(&mut self, t: u64, e: Event) {
        let id = self.ev_seq;
        self.ev_seq += 1;
        self.events.insert(id, e);
        self.heap.push(Reverse((t, id)));
    }

    fn node_of(&self, e: Ep) -> NodeId {
        match e {
            Ep::Coord(n) => n,
            Ep::Shard(s) => s % self.nodes(),
            Ep::Follower(_, n) => n,
        }
    }

    fn net_latency(&mut self, from: Ep, to: Ep) -> u64 {
        if self.node_of(from) == self.node_of(to) {
            self.cfg.net.local_ns
        } else {
            self.cfg.net.one_way_ns + self.rng.exp(self.cfg.net.jitter_mean_ns as f64) as u64
        }
    }

    fn repl_delay(&mut self) -> u64 {
        let r = self.cfg.replicas;
        let mut t = vec![self.cfg.net.fsync_ns];
        for _ in 1..r {
            let a = self.cfg.net.one_way_ns + self.rng.exp(self.cfg.net.jitter_mean_ns as f64) as u64;
            let b = self.cfg.net.one_way_ns + self.rng.exp(self.cfg.net.jitter_mean_ns as f64) as u64;
            t.push(a + b + self.cfg.net.fsync_ns);
        }
        t.sort_unstable();
        t[r / 2]
    }

    fn send(&mut self, from: Ep, to: Ep, msg: Msg, extra: u64, after_repl: bool) {
        self.stats.messages += 1;
        let lat = self.net_latency(from, to);
        let repl = if after_repl { self.repl_delay() } else { 0 };
        let mut t = self.now + extra + repl + lat;
        if matches!(to, Ep::Shard(_) | Ep::Follower(..)) {
            // Channels to shards are FIFO (statement order of a transaction).
            let last = self.chan_last.entry((from, to)).or_insert(0);
            if t <= *last {
                t = *last + 1;
            }
            *last = t;
        }
        self.schedule(t, Event::Deliver { from, to, msg });
    }

    fn kick(&mut self, s: ShardId) {
        if !self.shards[s].busy && self.shards[s].has_ready() {
            self.shards[s].busy = true;
            let now = self.now;
            self.schedule(now, Event::ShardService(s));
        }
    }

    fn drain(&mut self, mut gen: Option<&mut dyn FnMut(&mut Rng, usize) -> Call>) {
        while let Some(Reverse((t, id))) = self.heap.pop() {
            self.events_done += 1;
            if self.events_done > self.event_limit {
                let live = self.sessions.iter().flatten().count();
                panic!(
                    "event limit reached at t={}us: live sessions {live}, stats {:?}, shards {:?}\n{}",
                    t / 1000,
                    self.stats,
                    self.shard_stats(),
                    self.debug_dump()
                );
            }
            self.now = t;
            let ev = self.events.remove(&id).unwrap();
            match ev {
                Event::Deliver { from, to, msg } => self.deliver(from, to, msg),
                Event::ShardService(s) => self.service(s),
                Event::Run(sid) => self.run(sid),
                Event::FollowerService(s, n) => self.follower_service(s, n),
                Event::FollowerRetry(s, n, mi) => {
                    self.followers.entry((s, n)).or_default().0.push_back(mi);
                    self.kick_follower(s, n);
                }
                Event::Client(c) => {
                    if let (Some((end, _)), Some(g)) = (self.closed, gen.as_mut()) {
                        if self.now < end {
                            let call = g(&mut self.wl_rng, c);
                            let node = c % self.nodes();
                            self.submit(node, &call, Some(c));
                        }
                    }
                }
            }
        }
    }

    /// Closed timestamp: with follower reads on, no write commits at or below
    /// `now - follower_read_lag`.
    fn closed_ts(&self) -> Ts {
        let lag = self.cfg.follower_read_lag_ns;
        if !self.cfg.mode.follower_reads || self.now <= lag {
            return Ts::default();
        }
        Ts { wall: self.now - lag, logical: u32::MAX, node: u16::MAX }
    }

    /// Nodes holding a replica of shard `s`: the leader's node then the next ones.
    fn replica_nodes(&self, s: ShardId) -> Vec<NodeId> {
        let n = self.nodes();
        (0..self.cfg.replicas.min(n)).map(|k| (s + k) % n).collect()
    }

    /// Where a follower read of shard `s` from `node` goes: the replica on `node` if it
    /// is a follower, else the first follower; None if the shard has no follower.
    fn follower_for(&self, s: ShardId, node: NodeId) -> Option<NodeId> {
        let r = self.replica_nodes(s);
        let followers = &r[1..];
        if followers.contains(&node) {
            Some(node)
        } else {
            followers.first().copied()
        }
    }

    fn kick_follower(&mut self, s: ShardId, n: NodeId) {
        let q = self.followers.entry((s, n)).or_default();
        if !q.1 && !q.0.is_empty() {
            q.1 = true;
            let now = self.now;
            self.schedule(now, Event::FollowerService(s, n));
        }
    }

    fn follower_service(&mut self, s: ShardId, n: NodeId) {
        let Some(mi) = self.followers.get_mut(&(s, n)).and_then(|q| q.0.pop_front()) else {
            if let Some(q) = self.followers.get_mut(&(s, n)) {
                q.1 = false;
            }
            return;
        };
        self.stats.mis += 1;
        self.stats.follower_reads += 1;
        let res = self.shards[s].follower_read(&mi.op, mi.txn.read_ts, &self.g);
        self.apply_patches(s);
        self.kick(s);
        let cost = match res {
            None => {
                // Blocked by a pending write at or below the read timestamp: retry.
                let t = self.now + 20_000;
                self.schedule(t, Event::FollowerRetry(s, n, mi));
                self.cfg.cpu.mi_ns
            }
            Some((result, rows)) => {
                let cost = self.cfg.cpu.mi_ns + rows as u64 * self.cfg.cpu.scan_row_ns;
                let to = Ep::Coord(mi.txn.node);
                self.send(
                    Ep::Follower(s, n),
                    to,
                    Msg::C(ToCoord::MiResp {
                        sid: mi.txn.sid,
                        txn: mi.txn.id,
                        seq: mi.seq,
                        shard: s,
                        result,
                        push_above: None,
                        durable: false,
                    }),
                    cost,
                    false,
                );
                cost
            }
        };
        let t = self.now + cost;
        self.schedule(t, Event::FollowerService(s, n));
    }

    fn service(&mut self, s: ShardId) {
        let Some(mi) = self.shards[s].next(&self.g) else {
            self.shards[s].busy = false;
            return;
        };
        self.stats.mis += 1;
        self.shards[s].closed_ts = self.closed_ts();
        // Commit messages (stage, prepare, resolve, and a one-phase commit riding in
        // the writes' batch) are not charged shard CPU; only data MIs are.
        let commit_msg = matches!(mi.op, ShardOp::Commit1PC);
        let mut out = vec![];
        let done = self.shards[s].process(mi, &mut self.g, &mut out);
        let rows = done.as_ref().map_or(0, |d| d.rows);
        let cost = if commit_msg { 0 } else { self.cfg.cpu.mi_ns + rows as u64 * self.cfg.cpu.scan_row_ns };
        for o in out {
            self.send(Ep::Shard(s), o.to, o.msg, o.extra_ns, o.after_replication);
        }
        self.apply_patches(s);
        if let Some(d) = done.filter(|d| !d.silent) {
            let durable = d.wrote && self.cfg.mode.parallel_commit;
            let to = Ep::Coord(d.txn.node);
            self.send(
                Ep::Shard(s),
                to,
                Msg::C(ToCoord::MiResp {
                    sid: d.txn.sid,
                    txn: d.txn.id,
                    seq: d.mi_seq,
                    shard: s,
                    result: d.result,
                    push_above: d.push_above,
                    durable,
                }),
                cost,
                false,
            );
            if durable {
                self.send(
                    Ep::Shard(s),
                    to,
                    Msg::C(ToCoord::Durable { sid: d.txn.sid, txn: d.txn.id, seq: d.mi_seq }),
                    cost,
                    true,
                );
            }
        }
        let t = self.now + cost;
        self.schedule(t, Event::ShardService(s));
    }

    /// Final values of delta (escrow) writes are known only when the shard applies
    /// them: copy them into the history record of their transaction.
    fn apply_patches(&mut self, s: ShardId) {
        if self.shards[s].patches.is_empty() {
            return;
        }
        for (txn, key, value) in std::mem::take(&mut self.shards[s].patches) {
            let Some(&i) = self.hist_index.get(&txn) else {
                // One-phase commit: the shard applied it before the coordinator heard.
                self.early_patches.entry(txn).or_default().push((key, value));
                continue;
            };
            if let Some(w) = self.history.txns[i].writes.iter_mut().find(|w| w.key == key) {
                w.value = value;
            }
        }
    }

    fn deliver(&mut self, from: Ep, to: Ep, msg: Msg) {
        match (to, msg) {
            (Ep::Shard(s), Msg::S(m)) => self.shard_msg(s, from, m),
            (Ep::Follower(s, n), Msg::S(ToShard::Batch { txn, mis })) => {
                let q = self.followers.entry((s, n)).or_default();
                for (seq, op) in mis {
                    q.0.push_back(Mi { txn: txn.clone(), seq, op });
                }
                self.kick_follower(s, n);
            }
            (Ep::Coord(_), Msg::C(m)) => self.coord_msg(m),
            _ => unreachable!("misrouted message"),
        }
    }

    fn shard_msg(&mut self, s: ShardId, from: Ep, m: ToShard) {
        let me = Ep::Shard(s);
        match m {
            ToShard::Batch { txn, mis } => {
                for (seq, op) in mis {
                    self.shards[s].enqueue(Mi { txn: txn.clone(), seq, op });
                }
            }
            ToShard::Refresh { txn, sid, spans, from: f, to, .. } => {
                let ok = self.shards[s].refresh(txn, &spans, f, to, &self.g);
                self.send(me, from, Msg::C(ToCoord::RefreshResp { sid, txn, ok }), 0, false);
            }
            ToShard::Stage { txn, sid, .. } => {
                let ok = self.shards[s].stage(txn, &mut self.g);
                self.send(me, from, Msg::C(ToCoord::StageResp { sid, txn, ok }), 0, ok);
            }
            ToShard::Prepare { txn, sid, .. } => {
                let ok = self.shards[s].stage(txn, &mut self.g);
                self.send(me, from, Msg::C(ToCoord::PrepareResp { sid, txn, ok }), 0, ok);
            }
            ToShard::CommitRecord { txn, sid, .. } => {
                self.send(me, from, Msg::C(ToCoord::CommitRecordResp { sid, txn }), 0, true);
            }
            ToShard::Resolve { txn, commit_ts } => self.shards[s].resolve(txn, commit_ts),
            ToShard::Abort { txn } => self.shards[s].abort(txn),
            ToShard::RollbackTo { txn, from_seq, to_seq } => self.shards[s].rollback_to(txn, from_seq, to_seq),
            ToShard::Finish1PC { txn, ts, sid, node } => {
                self.g.set(txn, Status::Committed(ts));
                self.shards[s].resolve(txn, ts);
                self.send(me, Ep::Coord(node), Msg::C(ToCoord::OnePcResp { sid, txn, ts }), 0, false);
            }
        }
        self.apply_patches(s);
        self.kick(s);
    }

    // ------------------------------------------------------------------ HLC, sequences

    fn hlc_tick(&mut self, node: NodeId, floor: Option<Ts>) -> Ts {
        let mut last = self.hlc[node];
        if let Some(f) = floor {
            if f > last {
                last = f;
            }
        }
        let t = if self.now > last.wall {
            Ts { wall: self.now, logical: 0, node: node as u16 }
        } else {
            Ts { wall: last.wall, logical: last.logical + 1, node: node as u16 }
        };
        self.hlc[node] = t;
        t
    }

    fn seq_next(&mut self, node: NodeId, seq: SeqId) -> i64 {
        let n = self.nodes() as i64;
        let c = self.seq_state.entry((node, seq)).or_insert(0);
        let v = *c as i64 * n + node as i64 + 1;
        *c += 1;
        v
    }

    // ------------------------------------------------------------------ sessions

    fn submit(&mut self, node: NodeId, call: &Call, client: Option<usize>) -> Option<SessionId> {
        let idx = self.program.procedures.iter().position(|p| p.name.eq_ignore_ascii_case(&call.proc));
        let err = |msg: &str| CallStat {
            client,
            node,
            proc: call.proc.clone(),
            start_ns: self.now,
            end_ns: self.now,
            result: Err((errcode::VALUE_ERROR, msg.to_string())),
            restarts: 0,
            output: vec![],
        };
        let Some(idx) = idx else {
            let st = err("unknown procedure");
            self.calls.push(st);
            return None;
        };
        let code = self.codes[idx].clone();
        if call.args.len() != code.params.len() {
            let st = err("wrong number of arguments");
            self.calls.push(st);
            return None;
        }
        let mut vars: Vec<Slot> = code
            .var_types
            .iter()
            .map(|t| match t {
                VarType::Scalar => Slot::S(Value::Null),
                VarType::Row(_) => Slot::R(None),
            })
            .collect();
        for (p, v) in code.params.iter().zip(&call.args) {
            vars[*p as usize] = Slot::S(v.clone());
        }
        let mut node = node;
        if self.cfg.mode.routing {
            // Leaseholder routing: run the call on the node leading the most of its
            // keys (counting statements whose key depends on the parameters only);
            // ties keep the client's node, else the earliest key's node.
            let mut votes: Vec<(NodeId, usize, usize)> = vec![]; // (node, count, first position)
            // Scratch copy: assignments computed from parameters (e.g. a composite key)
            // are evaluated so that later keys can vote too. A heuristic: no branches.
            let mut scratch = vars.clone();
            for (pos, ins) in code.ins.iter().enumerate() {
                if let Ins::Assign { var, expr } = ins {
                    if let Ok(v) = eval(expr, &VarsEnv { vars: &scratch, fwd: None }) {
                        scratch[*var as usize] = Slot::S(v);
                    }
                    continue;
                }
                let Ins::Data(op) = ins else { continue };
                let Ok(Prep::Remote(r)) = self.prepare(op, &VarsEnv { vars: &scratch, fwd: None }) else { continue };
                if r.key.is_none() {
                    continue;
                }
                let sh = r.parts[0].0;
                // A read-only call served by followers goes where a follower replica is.
                let n = match (self.cfg.mode.follower_reads && code.read_only).then(|| self.follower_for(sh, node)).flatten() {
                    Some(f) => f,
                    None => self.node_of(Ep::Shard(sh)),
                };
                match votes.iter_mut().find(|v| v.0 == n) {
                    Some(v) => v.1 += 1,
                    None => votes.push((n, 1, pos)),
                }
            }
            if let Some(max) = votes.iter().map(|v| v.1).max() {
                let best: Vec<&(NodeId, usize, usize)> = votes.iter().filter(|v| v.1 == max).collect();
                if !best.iter().any(|v| v.0 == node) {
                    let target = best.iter().min_by_key(|v| v.2).unwrap().0;
                    self.stats.routed += 1;
                    node = target;
                }
            }
        }
        let st = ExecState {
            pc: 0,
            vars,
            cursors: vec![None; code.n_cursors],
            frames: vec![],
            output: vec![],
            epoch: 0,
        };
        let sid = self.sessions.len();
        self.sessions.push(Some(Session {
            sid,
            node,
            client,
            start: self.now,
            code,
            class: call.class,
            snap: st.clone(),
            st,
            block_prio: None,
            att: None,
            wait: Wait::None,
            restarts: 0,
            out_committed: vec![],
            finished: false,
        }));
        let t = self.now + self.cfg.cpu.call_overhead_ns;
        self.schedule(t, Event::Run(sid));
        Some(sid)
    }

    fn run(&mut self, sid: SessionId) {
        let Some(mut s) = self.sessions[sid].take() else { return };
        self.step(&mut s);
        if !s.finished {
            self.sessions[sid] = Some(s);
        }
    }

    fn finish(&mut self, s: &mut Session, result: Result<(), (i32, String)>) {
        s.finished = true;
        if result.is_ok() {
            self.stats.calls_ok += 1;
        } else {
            self.stats.calls_err += 1;
        }
        self.calls.push(CallStat {
            client: s.client,
            node: s.node,
            proc: s.code.name.clone(),
            start_ns: s.start,
            end_ns: self.now,
            result,
            restarts: s.restarts,
            output: std::mem::take(&mut s.out_committed),
        });
        if let (Some(c), Some((_, think))) = (s.client, self.closed) {
            let t = self.now + think;
            self.schedule(t, Event::Client(c));
        }
    }

    fn ensure_attempt(&mut self, s: &mut Session) {
        if s.att.is_some() {
            return;
        }
        let mut read_ts = self.hlc_tick(s.node, None);
        let prio = *s.block_prio.get_or_insert(Priority { class: s.class, ts: read_ts });
        let follower = self.cfg.mode.follower_reads && s.code.read_only && self.now > self.cfg.follower_read_lag_ns;
        if follower {
            // Unique timestamp at the closed time; writers are pushed above it.
            self.follower_seq += 1;
            read_ts = Ts { wall: self.now - self.cfg.follower_read_lag_ns, logical: (1 << 31) + self.follower_seq, node: s.node as u16 };
        }
        let id = self.next_txn;
        self.next_txn += 1;
        self.g.begin(id, TxnState { status: Status::Running, prio, node: s.node, sid: s.sid, read_ts });
        let mut a = Attempt::new(id, read_ts);
        a.follower = follower;
        s.att = Some(a);
    }

    fn flush(&mut self, s: &mut Session) {
        let Some(a) = s.att.as_mut() else { return };
        if a.outbox.is_empty() {
            return;
        }
        let meta = TxnMeta {
            id: a.txn,
            prio: s.block_prio.unwrap(),
            read_ts: a.read_ts,
            write_ts: a.write_ts,
            node: s.node,
            sid: s.sid,
        };
        let follower = a.follower;
        let outbox = std::mem::take(&mut a.outbox);
        for (shard, mis) in outbox {
            let to = match follower.then(|| self.follower_for(shard, s.node)).flatten() {
                Some(n) => Ep::Follower(shard, n),
                None => Ep::Shard(shard),
            };
            self.send(Ep::Coord(s.node), to, Msg::S(ToShard::Batch { txn: meta.clone(), mis }), 0, false);
        }
    }

    fn step(&mut self, s: &mut Session) {
        loop {
            if s.finished {
                return;
            }
            if let Some(e) = s.att.as_mut().and_then(|a| a.deferred_err.take()) {
                self.fail(s, e);
                return;
            }
            match s.wait {
                Wait::Op(seq) => {
                    let ready = s
                        .att
                        .as_ref()
                        .map_or(true, |a| a.ops.get(&seq).map_or(true, |o| o.ready || o.cancelled));
                    if !ready {
                        self.lookahead(s);
                        break;
                    }
                    s.wait = Wait::None;
                }
                Wait::Commit => {
                    if self.advance_commit(s) {
                        continue;
                    }
                    break;
                }
                Wait::Backoff => s.wait = Wait::None,
                Wait::None => {}
            }
            if !self.exec(s) {
                break;
            }
        }
        if !s.finished {
            self.flush(s);
        }
    }

    // ------------------------------------------------------------------ interpreter

    fn exec(&mut self, s: &mut Session) -> bool {
        let code = s.code.clone();
        let pc = s.st.pc;
        match &code.ins[pc] {
            Ins::Assign { var, expr } => {
                let early = s.att.as_mut().and_then(|a| a.early.remove(&pc));
                let v = match early {
                    Some(Slot::S(v)) => Ok(v),
                    _ => eval(expr, &VarsEnv { vars: &s.st.vars, fwd: None }),
                };
                match v {
                    Ok(v) => {
                        s.st.vars[*var as usize] = Slot::S(v);
                        s.st.pc += 1;
                        true
                    }
                    Err(c) => self.raise(s, c, String::new()),
                }
            }
            Ins::SeqNext { seq, into } => {
                let early = s.att.as_mut().and_then(|a| a.early.remove(&pc));
                let v = match early {
                    Some(v) => v,
                    None => Slot::S(Value::Int(self.seq_next(s.node, *seq))),
                };
                s.st.vars[*into as usize] = v;
                s.st.pc += 1;
                true
            }
            Ins::Output(e) => match eval(e, &VarsEnv { vars: &s.st.vars, fwd: None }) {
                Ok(v) => {
                    s.st.output.push(match v {
                        Value::Null => String::new(),
                        Value::Int(i) => i.to_string(),
                        Value::Str(x) => x,
                    });
                    s.st.pc += 1;
                    true
                }
                Err(c) => self.raise(s, c, String::new()),
            },
            Ins::Branch { cond, jump_if, target, pop, .. } => {
                match eval(cond, &VarsEnv { vars: &s.st.vars, fwd: None }) {
                    Ok(v) => {
                        if v.is_true() == *jump_if {
                            self.jump(s, *target, *pop);
                        } else {
                            s.st.pc += 1;
                        }
                        true
                    }
                    Err(c) => self.raise(s, c, String::new()),
                }
            }
            Ins::Jump { target, pop } => {
                self.jump(s, *target, *pop);
                true
            }
            Ins::Raise { code: c, msg } => self.raise(s, *c, msg.clone()),
            Ins::PushHandler { def } => {
                self.ensure_attempt(s);
                let sp = s.att.as_ref().unwrap().next_seq;
                s.st.frames.push(Frame { def: *def, sp });
                s.st.pc += 1;
                true
            }
            Ins::PopHandler => {
                s.st.frames.pop();
                s.st.pc += 1;
                true
            }
            Ins::CursorNext { cursor, row_var, end } => {
                let epoch = s.st.epoch;
                let Some(c) = s.st.cursors[*cursor].as_mut() else {
                    return self.raise(s, errcode::VALUE_ERROR, "cursor not open".into());
                };
                if c.for_update && c.epoch != epoch {
                    return self.raise(s, errcode::FETCH_OUT_OF_SEQUENCE, "fetch out of sequence".into());
                }
                if c.pos < c.rows.len() {
                    let row = c.rows[c.pos].clone();
                    c.pos += 1;
                    s.st.vars[*row_var as usize] = Slot::R(Some(row));
                    s.st.pc += 1;
                } else {
                    s.st.pc = *end;
                }
                true
            }
            Ins::Data(op) => self.exec_data(s, pc, op),
            Ins::Commit | Ins::End => {
                s.wait = Wait::Commit;
                true
            }
            Ins::Rollback => {
                if s.att.is_some() {
                    self.stats.user_rollbacks += 1;
                    self.abort_attempt(s, false);
                }
                s.block_prio = None;
                s.st.pc += 1;
                s.st.epoch += 1;
                s.st.output.clear();
                s.snap = s.st.clone();
                true
            }
        }
    }

    fn jump(&mut self, s: &mut Session, target: usize, pop: usize) {
        for _ in 0..pop {
            s.st.frames.pop();
        }
        if let Some(a) = s.att.as_ref() {
            debug_assert!(
                a.at_pc.keys().all(|p| *p >= target),
                "lookahead issued an op that is skipped by a jump"
            );
        }
        s.st.pc = target;
    }

    /// Raise an exception at pc. Returns true if a handler caught it.
    fn raise(&mut self, s: &mut Session, code: i32, msg: String) -> bool {
        if let Some(a) = s.att.as_mut() {
            a.early.clear();
        }
        let handlers = s.code.clone();
        while let Some(f) = s.st.frames.pop() {
            let arm = handlers.handlers[f.def].arms.iter().find(|(c, _)| c.is_none() || *c == Some(code));
            if let Some(&(_, pc)) = arm {
                self.rollback_to(s, f.sp);
                s.st.pc = pc;
                s.wait = Wait::None;
                return true;
            }
        }
        self.fail(s, (code, msg));
        false
    }

    fn rollback_to(&mut self, s: &mut Session, sp: u32) {
        let Some(a) = s.att.as_mut() else { return };
        a.at_pc.clear();
        a.early.clear();
        if sp >= a.next_seq {
            return;
        }
        let to = a.next_seq - 1;
        for (seq, o) in a.ops.iter_mut() {
            if *seq >= sp {
                o.cancelled = true;
            }
        }
        let me = a.txn;
        a.wlog.retain(|(q, _, _)| *q < sp);
        a.reads.retain(|(q, r)| *q < sp || r.observed != Some(me));
        a.range_reads.retain(|(q, r)| *q < sp || r.rows.iter().all(|(_, w)| *w != Some(me)));
        let txn = a.txn;
        let shards: Vec<ShardId> = a.shards.iter().copied().collect();
        for sh in shards {
            self.send(
                Ep::Coord(s.node),
                Ep::Shard(sh),
                Msg::S(ToShard::RollbackTo { txn, from_seq: sp, to_seq: to }),
                0,
                false,
            );
        }
    }

    fn fail(&mut self, s: &mut Session, err: (i32, String)) {
        if s.att.is_some() {
            self.abort_attempt(s, false);
        }
        self.finish(s, Err(err));
    }

    fn abort_attempt(&mut self, s: &mut Session, restart: bool) {
        let Some(a) = s.att.take() else { return };
        if !matches!(self.g.status(a.txn), Status::Committed(_)) {
            self.g.set(a.txn, Status::Aborted);
        }
        for sh in a.shards.iter().copied() {
            self.send(Ep::Coord(s.node), Ep::Shard(sh), Msg::S(ToShard::Abort { txn: a.txn }), 0, false);
        }
        self.stats.txn_aborted += 1;
        self.history.txns.push(TxnRecord {
            id: a.txn,
            outcome: Outcome::Aborted,
            commit_ts: a.read_ts,
            reads: a.reads.into_iter().map(|x| x.1).collect(),
            range_reads: a.range_reads.into_iter().map(|x| x.1).collect(),
            writes: vec![],
        });
        if restart {
            s.st = s.snap.clone();
            s.restarts += 1;
            s.wait = Wait::None;
        }
    }

    fn restart(&mut self, s: &mut Session) {
        self.abort_attempt(s, true);
        let delay = self.backoff(s.restarts);
        s.wait = Wait::Backoff;
        let t = self.now + delay;
        self.schedule(t, Event::Run(s.sid));
    }

    fn backoff(&mut self, restarts: u32) -> u64 {
        let base = self.cfg.restart_backoff_ns as f64;
        if base == 0.0 {
            return 0;
        }
        let mult = (1u64 << restarts.saturating_sub(1).min(6)) as f64;
        self.rng.exp(base * mult) as u64
    }

    // ------------------------------------------------------------------ data ops

    fn can_pipeline(&self, s: &Session, op: &DataOp) -> bool {
        self.cfg.mode.pipelining && op.pipelinable() && s.st.frames.is_empty()
    }

    fn exec_data(&mut self, s: &mut Session, pc: usize, op: &DataOp) -> bool {
        self.ensure_attempt(s);
        let pipe = self.can_pipeline(s, op);
        let a = s.att.as_mut().unwrap();
        if let Some(&seq) = a.at_pc.get(&pc) {
            let ready = a.ops[&seq].ready;
            if ready {
                a.at_pc.remove(&pc);
                return self.retire(s, pc, seq);
            }
            if pipe {
                a.at_pc.remove(&pc);
                a.ops.get_mut(&seq).unwrap().retired = true;
                s.st.pc += 1;
                return true;
            }
            s.wait = Wait::Op(seq);
            return true;
        }
        let prep = self.prepare(op, &VarsEnv { vars: &s.st.vars, fwd: None });
        match prep {
            Err(c) => self.raise(s, c, String::new()),
            Ok(Prep::NoData) => self.raise(s, errcode::NO_DATA_FOUND, "no data found".into()),
            Ok(Prep::Rowcount0) => {
                let rc = match op {
                    DataOp::Update { rowcount_into, .. } | DataOp::Delete { rowcount_into, .. } => *rowcount_into,
                    _ => None,
                };
                if let Some(v) = rc {
                    s.st.vars[v as usize] = Slot::S(Value::Int(0));
                }
                s.st.pc += 1;
                true
            }
            Ok(Prep::Remote(r)) => {
                let a = s.att.as_mut().unwrap();
                let seq = issue(a, pc, r);
                if a.ops[&seq].ready {
                    return self.retire(s, pc, seq);
                }
                if pipe {
                    a.ops.get_mut(&seq).unwrap().retired = true;
                    s.st.pc += 1;
                } else {
                    a.at_pc.insert(pc, seq);
                    s.wait = Wait::Op(seq);
                }
                true
            }
        }
    }

    fn eval_key(&self, table: TableId, e: &Expr, env: &dyn Env) -> Result<Option<Key>, i32> {
        match eval(e, env)? {
            Value::Null => Ok(None),
            Value::Int(pk) => Ok(Some(Key { table, pk })),
            Value::Str(x) => x.trim().parse().map(|pk| Some(Key { table, pk })).map_err(|_| errcode::VALUE_ERROR),
        }
    }

    fn eval_bound(e: &Expr, env: &dyn Env) -> Result<Option<i64>, i32> {
        match eval(e, env)? {
            Value::Null => Ok(None),
            Value::Int(i) => Ok(Some(i)),
            Value::Str(_) => Err(errcode::VALUE_ERROR),
        }
    }

    fn prepare(&self, op: &DataOp, env: &dyn Env) -> Result<Prep, i32> {
        let one = |key: Key, sop: ShardOp, fu: bool| {
            Prep::Remote(Remote {
                parts: vec![(self.shard_of(key), sop)],
                key: Some(key),
                table: key.table,
                lo: key.pk,
                limit: None,
                for_update: fu,
            })
        };
        Ok(match op {
            DataOp::Get { table, key, for_update, .. } => match self.eval_key(*table, key, env)? {
                None => Prep::NoData,
                Some(k) => one(k, ShardOp::Get { key: k, for_update: *for_update }, *for_update),
            },
            DataOp::Update { table, key, sets, .. } => match self.eval_key(*table, key, env)? {
                None => Prep::Rowcount0,
                Some(k) => {
                    let sets = sets
                        .iter()
                        .map(|(c, e)| Ok((*c, subst(e, env, None)?)))
                        .collect::<Result<Vec<_>, i32>>()?;
                    match self.cfg.mode.escrow.then(|| as_delta(&sets)).flatten() {
                        Some(adds) => one(k, ShardOp::Delta { key: k, adds }, false),
                        None => one(k, ShardOp::Update { key: k, sets }, false),
                    }
                }
            },
            DataOp::Delete { table, key, .. } => match self.eval_key(*table, key, env)? {
                None => Prep::Rowcount0,
                Some(k) => one(k, ShardOp::Delete { key: k }, false),
            },
            DataOp::Insert { table, row } => {
                let row = row.iter().map(|e| eval(e, env)).collect::<Result<Row, i32>>()?;
                let pkc = self.program.schema.tables[*table as usize].pk_col as usize;
                let pk = match row.get(pkc) {
                    Some(Value::Int(i)) => *i,
                    _ => return Err(errcode::VALUE_ERROR),
                };
                let k = Key { table: *table, pk };
                one(k, ShardOp::Insert { key: k, row }, false)
            }
            DataOp::Aggregate { table, range, .. } => {
                let lo = Self::eval_bound(&range.lo, env)?;
                let hi = Self::eval_bound(&range.hi, env)?;
                self.scan_prep(*table, lo, hi, None, None, false)
            }
            DataOp::ScanOpen { table, range, filter, limit, for_update, row_var, .. } => {
                let lo = Self::eval_bound(&range.lo, env)?;
                let hi = Self::eval_bound(&range.hi, env)?;
                let filter = filter.as_ref().map(|f| subst(f, env, Some(*row_var))).transpose()?;
                let limit = match limit {
                    None => None,
                    Some(l) => match eval(l, env)? {
                        Value::Null => None,
                        Value::Int(n) => Some(n.max(0) as u64),
                        Value::Str(_) => return Err(errcode::VALUE_ERROR),
                    },
                };
                self.scan_prep(*table, lo, hi, filter, limit, *for_update)
            }
        })
    }

    fn scan_prep(
        &self,
        table: TableId,
        lo: Option<i64>,
        hi: Option<i64>,
        filter: Option<Expr>,
        limit: Option<u64>,
        for_update: bool,
    ) -> Prep {
        let (lo, hi) = match (lo, hi) {
            (Some(a), Some(b)) => (a, b),
            _ => (1, 0), // NULL bound: no rows
        };
        let parts = self
            .shards_for_range(table, lo, hi)
            .into_iter()
            .map(|(s, a, z)| {
                (s, ShardOp::Scan { table, lo: a, hi: z, filter: filter.clone(), limit, for_update })
            })
            .collect();
        Prep::Remote(Remote { parts, key: None, table, lo, limit, for_update })
    }

    fn retire(&mut self, s: &mut Session, pc: usize, seq: u32) -> bool {
        let code = s.code.clone();
        let a = s.att.as_mut().unwrap();
        let o = a.ops.get_mut(&seq).unwrap();
        o.retired = true;
        if let Some(c) = o.err {
            return self.raise(s, c, String::new());
        }
        let Ins::Data(op) = &code.ins[pc] else { unreachable!() };
        match op {
            DataOp::Get { into, .. } => match o.read_value.take() {
                None => return self.raise(s, errcode::NO_DATA_FOUND, "no data found".into()),
                Some(row) => s.st.vars[*into as usize] = Slot::R(Some(row)),
            },
            DataOp::Update { rowcount_into, .. } | DataOp::Delete { rowcount_into, .. } => {
                if let Some(v) = rowcount_into {
                    s.st.vars[*v as usize] = Slot::S(Value::Int(o.applied as i64));
                }
            }
            DataOp::Insert { .. } => {}
            DataOp::Aggregate { func, into, .. } => {
                let v = match func {
                    AggFunc::Count => Value::Int(o.rows.len() as i64),
                    AggFunc::Sum(c) => {
                        let mut sum: Option<i64> = None;
                        for r in &o.rows {
                            if let Value::Int(x) = r[*c as usize] {
                                sum = Some(sum.unwrap_or(0).saturating_add(x));
                            }
                        }
                        sum.map(Value::Int).unwrap_or(Value::Null)
                    }
                };
                s.st.vars[*into as usize] = Slot::S(v);
            }
            DataOp::ScanOpen { cursor, for_update, .. } => {
                let rows = std::mem::take(&mut o.rows);
                s.st.cursors[*cursor] = Some(Cursor { rows, pos: 0, for_update: *for_update, epoch: s.st.epoch });
            }
        }
        s.st.pc += 1;
        true
    }

    // ------------------------------------------------------------------ lookahead (MI idea)

    fn lookahead(&mut self, s: &mut Session) {
        if !self.cfg.mode.lookahead {
            return;
        }
        let code = s.code.clone();
        let pc = s.st.pc;
        let Some(mut a) = s.att.take() else { return };
        let mut fwd: HashMap<VarId, Option<Slot>> = HashMap::new();
        for w in &code.writes[pc] {
            fwd.insert(*w, None);
        }
        let mut blocked_tables: HashSet<TableId> = HashSet::new();
        let mut i = pc + 1;
        let mut n = 0;
        while i < code.ins.len() && n < self.cfg.lookahead_window {
            n += 1;
            let avail = code.reads[i].iter().all(|v| !matches!(fwd.get(v), Some(None)));
            match &code.ins[i] {
                Ins::Assign { var, expr } => {
                    if let Some(v) = a.early.get(&i) {
                        fwd.insert(*var, Some(v.clone()));
                    } else if avail {
                        match eval(expr, &VarsEnv { vars: &s.st.vars, fwd: Some(&fwd) }) {
                            Ok(v) => {
                                a.early.insert(i, Slot::S(v.clone()));
                                fwd.insert(*var, Some(Slot::S(v)));
                            }
                            Err(_) => break,
                        }
                    } else {
                        fwd.insert(*var, None);
                    }
                    i += 1;
                }
                Ins::SeqNext { seq, into } => {
                    let v = match a.early.get(&i) {
                        Some(v) => v.clone(),
                        None => {
                            let x = Slot::S(Value::Int(self.seq_next(s.node, *seq)));
                            a.early.insert(i, x.clone());
                            x
                        }
                    };
                    fwd.insert(*into, Some(v));
                    i += 1;
                }
                Ins::Output(_) | Ins::PopHandler => i += 1,
                Ins::Data(op) => {
                    let t = op.table();
                    if !a.at_pc.contains_key(&i) {
                        let mut issued = false;
                        if avail && !blocked_tables.contains(&t) {
                            if let Ok(Prep::Remote(r)) =
                                self.prepare(op, &VarsEnv { vars: &s.st.vars, fwd: Some(&fwd) })
                            {
                                let seq = issue(&mut a, i, r);
                                a.at_pc.insert(i, seq);
                                self.stats.lookahead_issued += 1;
                                issued = true;
                            }
                        }
                        if !issued {
                            // Later ops on this table may touch the same key: keep order.
                            blocked_tables.insert(t);
                        }
                    }
                    for w in &code.writes[i] {
                        fwd.insert(*w, None);
                    }
                    i += 1;
                }
                Ins::Branch { cond, jump_if, target, pop, guard } => {
                    if *pop > 0 {
                        break;
                    }
                    if avail {
                        match eval(cond, &VarsEnv { vars: &s.st.vars, fwd: Some(&fwd) }) {
                            Ok(v) => {
                                if v.is_true() == *jump_if {
                                    if *target <= i {
                                        break;
                                    }
                                    i = *target;
                                } else {
                                    i += 1;
                                }
                            }
                            Err(_) => break,
                        }
                    } else if self.cfg.mode.speculation && *guard && !*jump_if && *target > i {
                        // Predict: the guard does not raise.
                        i = *target;
                    } else {
                        break;
                    }
                }
                Ins::Jump { target, pop } => {
                    if *pop > 0 || *target <= i {
                        break;
                    }
                    i = *target;
                }
                _ => break,
            }
        }
        s.att = Some(a);
    }

    // ------------------------------------------------------------------ responses

    fn coord_msg(&mut self, m: ToCoord) {
        let sid = match &m {
            ToCoord::MiResp { sid, .. }
            | ToCoord::Durable { sid, .. }
            | ToCoord::RefreshResp { sid, .. }
            | ToCoord::StageResp { sid, .. }
            | ToCoord::PrepareResp { sid, .. }
            | ToCoord::CommitRecordResp { sid, .. }
            | ToCoord::Wounded { sid, .. }
            | ToCoord::OnePcResp { sid, .. } => *sid,
        };
        let Some(mut s) = self.sessions.get_mut(sid).and_then(|x| x.take()) else { return };
        let txn = match &m {
            ToCoord::MiResp { txn, .. }
            | ToCoord::Durable { txn, .. }
            | ToCoord::RefreshResp { txn, .. }
            | ToCoord::StageResp { txn, .. }
            | ToCoord::PrepareResp { txn, .. }
            | ToCoord::CommitRecordResp { txn, .. }
            | ToCoord::Wounded { txn, .. }
            | ToCoord::OnePcResp { txn, .. } => *txn,
        };
        if s.att.as_ref().map(|a| a.txn) != Some(txn) {
            self.sessions[sid] = Some(s);
            return;
        }
        let mut run = true;
        match m {
            ToCoord::MiResp { seq, shard, result, push_above, durable, .. } => {
                run = self.on_result(&mut s, seq, shard, result, push_above, durable);
            }
            ToCoord::Durable { seq, .. } => {
                let a = s.att.as_mut().unwrap();
                if let Some(o) = a.ops.get_mut(&seq) {
                    o.dur_got += 1;
                }
                run = s.wait == Wait::Commit;
            }
            ToCoord::RefreshResp { ok, .. } => {
                let a = s.att.as_mut().unwrap();
                if let Phase::Refresh { pending, ok: all } = &mut a.phase {
                    *pending -= 1;
                    *all &= ok;
                }
            }
            ToCoord::StageResp { ok, .. } => {
                if ok {
                    s.att.as_mut().unwrap().phase = Phase::Stage { staged: true };
                } else {
                    self.stats.restart_stage += 1;
                    self.restart(&mut s);
                    run = false;
                }
            }
            ToCoord::PrepareResp { ok, .. } => {
                let a = s.att.as_mut().unwrap();
                if let Phase::Prepare { pending, ok: all } = &mut a.phase {
                    *pending -= 1;
                    *all &= ok;
                }
            }
            ToCoord::CommitRecordResp { .. } => {
                s.att.as_mut().unwrap().phase = Phase::Record { done: true };
            }
            ToCoord::OnePcResp { ts, .. } => {
                self.stats.one_pc += 1;
                self.finalize(&mut s, ts);
            }
            ToCoord::Wounded { .. } => {
                let st = s.att.as_ref().map(|a| &a.phase);
                if matches!(st, Some(Phase::None) | Some(Phase::Refresh { .. }) | Some(Phase::OnePc)) {
                    self.stats.restart_wounded += 1;
                    self.restart(&mut s);
                }
                run = false;
            }
        }
        let sid = s.sid;
        self.sessions[sid] = Some(s);
        if run {
            self.run(sid);
        }
    }

    /// Record an MI result. Returns whether the session should run.
    fn on_result(
        &mut self,
        s: &mut Session,
        seq: u32,
        shard: ShardId,
        result: MiResult,
        push: Option<Ts>,
        durable: bool,
    ) -> bool {
        if matches!(result, MiResult::Aborted) {
            self.stats.restart_died += 1;
            self.restart(s);
            return false;
        }
        if let MiResult::OnePc { ok } = result {
            // The one-phase commit could not apply (pushed, or a write failed):
            // fall back to the normal commit path; the intents are in place.
            debug_assert!(!ok);
            let a = s.att.as_mut().unwrap();
            if a.phase == Phase::OnePc {
                a.phase = Phase::None;
            }
            self.stats.one_pc_fallback += 1;
            return s.wait == Wait::Commit;
        }
        let node = s.node;
        let new_ts = {
            let a = s.att.as_ref().unwrap();
            match push {
                Some(p) if a.write_ts <= p => Some(p),
                _ => None,
            }
        };
        if let Some(p) = new_ts {
            let t = self.hlc_tick(node, Some(p));
            s.att.as_mut().unwrap().write_ts = t;
        }
        let a = s.att.as_mut().unwrap();
        let me = a.txn;
        let has_reads = !a.spans.is_empty()
            || a.ops.values().any(|x| {
                !x.cancelled
                    && matches!(
                        s.code.ins[x.pc],
                        Ins::Data(DataOp::Get { .. } | DataOp::Aggregate { .. } | DataOp::ScanOpen { .. })
                    )
            });
        let Some(o) = a.ops.get_mut(&seq) else { return false };
        if o.cancelled {
            return false;
        }
        if durable {
            o.dur_expected += 1;
        }
        match result {
            MiResult::Aborted | MiResult::OnePc { .. } => unreachable!(),
            MiResult::Delta { applied } => {
                // Final value known only at commit: patched into the history then.
                self.stats.deltas_issued += 1;
                a.wlog.push((seq, o.key.unwrap(), None));
                a.intent_shards.insert(shard);
                o.applied = applied;
            }
            MiResult::Err(CHECK_STALE) if has_reads => {
                // The CHECK failed on data newer than our snapshot: our reads are stale,
                // retry with a fresh snapshot (it will then fail or pass for real).
                self.stats.restart_check += 1;
                self.restart(s);
                return false;
            }
            MiResult::Err(c) => {
                o.err = Some(if c == CHECK_STALE { errcode::CHECK_VIOLATION } else { c });
            }
            MiResult::Read { value, writer } => {
                let key = o.key.unwrap();
                a.reads.push((seq, ReadEvent { key, observed: writer, value: value.clone() }));
                if o.for_update {
                    a.intent_shards.insert(shard);
                    a.wlog.push((seq, key, value.clone()));
                } else {
                    a.spans.push((shard, Span::Point(key)));
                }
                o.read_value = value;
            }
            MiResult::Write { new_value, applied, prev, prev_writer } => {
                let key = o.key.unwrap();
                a.reads.push((seq, ReadEvent { key, observed: prev_writer, value: prev }));
                a.wlog.push((seq, key, new_value));
                a.intent_shards.insert(shard);
                o.applied = applied;
            }
            MiResult::Scan { rows, seen, hi_eff } => {
                if o.for_update && !rows.is_empty() {
                    a.intent_shards.insert(shard);
                }
                o.scan_parts.push((rows, seen, hi_eff));
            }
        }
        o.got += 1;
        if o.got >= o.parts {
            o.ready = true;
            if o.key.is_none() {
                // Merge a (possibly multi-shard) scan.
                let pkc = self.program.schema.tables[o.table as usize].pk_col as usize;
                let pk = |r: &Row| match r[pkc] {
                    Value::Int(i) => i,
                    _ => 0,
                };
                let mut rows: Vec<Row> = o.scan_parts.iter().flat_map(|p| p.0.clone()).collect();
                rows.sort_by_key(|r| pk(r));
                let mut hi_eff = o.scan_parts.iter().map(|p| p.2).max().unwrap_or(o.lo - 1);
                if let Some(l) = o.limit {
                    if rows.len() as u64 >= l {
                        rows.truncate(l as usize);
                        hi_eff = rows.last().map(|r| pk(r)).unwrap_or(o.lo.saturating_sub(1));
                    }
                }
                let mut seen: Vec<(i64, Option<TxnId>)> =
                    o.scan_parts.iter().flat_map(|p| p.1.clone()).filter(|(k, _)| *k <= hi_eff).collect();
                seen.sort_by_key(|x| x.0);
                if o.for_update {
                    // Every row a shard locked (even if cut by the limit) is an intent.
                    let table = o.table;
                    for p in &o.scan_parts {
                        for r in &p.0 {
                            a.wlog.push((seq, Key { table, pk: pk(r) }, Some(r.clone())));
                        }
                    }
                }
                o.scan_parts.clear();
                if hi_eff >= o.lo {
                    let table = o.table;
                    let lo = o.lo;
                    a.range_reads.push((seq, RangeReadEvent { table, lo, hi: hi_eff, rows: seen }));
                    let shards = if self.cfg.mode.fast_path { o.part_shards.clone() } else { a.shards.iter().copied().collect() };
                    for sh in shards {
                        a.spans.push((sh, Span::Range(table, lo, hi_eff)));
                    }
                }
                let o = a.ops.get_mut(&seq).unwrap();
                o.rows = rows;
            }
            let o = a.ops.get_mut(&seq).unwrap();
            if o.retired && o.err.is_some() {
                a.deferred_err = Some((o.err.unwrap(), String::new()));
            }
        }
        let _ = me;
        true
    }

    // ------------------------------------------------------------------ commit

    /// Drive the commit of the current block. Returns true when the interpreter can
    /// continue (committed, or restarted from the block start).
    fn advance_commit(&mut self, s: &mut Session) -> bool {
        if s.att.is_none() {
            self.after_commit(s);
            return true;
        }
        loop {
            let a = s.att.as_mut().unwrap();
            match &a.phase {
                Phase::OnePc => return false,
                Phase::None => {
                    if self.cfg.mode.fast_path && !a.tried_1pc {
                        a.tried_1pc = true;
                        if let Some(sh) = one_pc_shard(a) {
                            let seq = a.next_seq;
                            a.next_seq += 1;
                            a.outbox.get_mut(&sh).unwrap().push((seq, ShardOp::Commit1PC));
                            a.phase = Phase::OnePc;
                            return false;
                        }
                    }
                    if a.outstanding() > 0 {
                        return false;
                    }
                    if a.intent_shards.is_empty() {
                        let ts = a.read_ts;
                        self.finalize(s, ts);
                        return true;
                    }
                    if a.write_ts > a.read_ts {
                        self.stats.refreshes += 1;
                        let mut by_shard: BTreeMap<ShardId, Vec<Span>> = BTreeMap::new();
                        for (sh, sp) in &a.spans {
                            by_shard.entry(*sh).or_default().push(sp.clone());
                        }
                        if by_shard.is_empty() {
                            a.read_ts = a.write_ts;
                            continue;
                        }
                        a.phase = Phase::Refresh { pending: by_shard.len(), ok: true };
                        let (txn, from, to) = (a.txn, a.read_ts, a.write_ts);
                        for (sh, spans) in by_shard {
                            self.send(
                                Ep::Coord(s.node),
                                Ep::Shard(sh),
                                Msg::S(ToShard::Refresh { txn, sid: s.sid, node: s.node, spans, from, to }),
                                0,
                                false,
                            );
                        }
                        return false;
                    }
                    self.start_stage(s);
                    return false;
                }
                Phase::Refresh { pending, ok } => {
                    if *pending > 0 {
                        return false;
                    }
                    if !*ok {
                        self.stats.restart_refresh += 1;
                        self.abort_attempt(s, true);
                        return true;
                    }
                    a.read_ts = a.write_ts;
                    let (id, ts) = (a.txn, a.read_ts);
                    self.g.set_read_ts(id, ts);
                    a.phase = Phase::None;
                    self.start_stage(s);
                    return false;
                }
                Phase::Stage { staged } => {
                    if *staged && a.all_durable() {
                        let ts = a.write_ts;
                        self.finalize(s, ts);
                        return true;
                    }
                    return false;
                }
                Phase::Prepare { pending, ok } => {
                    if *pending > 0 {
                        return false;
                    }
                    if !*ok {
                        self.stats.restart_stage += 1;
                        self.abort_attempt(s, true);
                        return true;
                    }
                    let rec = *a.intent_shards.iter().next().unwrap();
                    a.phase = Phase::Record { done: false };
                    let txn = a.txn;
                    self.send(
                        Ep::Coord(s.node),
                        Ep::Shard(rec),
                        Msg::S(ToShard::CommitRecord { txn, sid: s.sid, node: s.node }),
                        0,
                        false,
                    );
                    return false;
                }
                Phase::Record { done } => {
                    if *done {
                        let ts = a.write_ts;
                        self.finalize(s, ts);
                        return true;
                    }
                    return false;
                }
            }
        }
    }

    fn start_stage(&mut self, s: &mut Session) {
        let a = s.att.as_mut().unwrap();
        let txn = a.txn;
        if self.cfg.mode.parallel_commit {
            let rec = *a.intent_shards.iter().next().unwrap();
            a.phase = Phase::Stage { staged: false };
            self.send(Ep::Coord(s.node), Ep::Shard(rec), Msg::S(ToShard::Stage { txn, sid: s.sid, node: s.node }), 0, false);
        } else {
            let shards: Vec<ShardId> = a.intent_shards.iter().copied().collect();
            a.phase = Phase::Prepare { pending: shards.len(), ok: true };
            for sh in shards {
                self.send(Ep::Coord(s.node), Ep::Shard(sh), Msg::S(ToShard::Prepare { txn, sid: s.sid, node: s.node }), 0, false);
            }
        }
    }

    fn finalize(&mut self, s: &mut Session, ts: Ts) {
        let a = s.att.take().unwrap();
        self.g.set(a.txn, Status::Committed(ts));
        self.stats.txn_committed += 1;
        let mut last: BTreeMap<Key, (u32, Option<Row>)> = BTreeMap::new();
        for (seq, k, v) in a.wlog {
            match last.get(&k) {
                Some((q, _)) if *q > seq => {}
                _ => {
                    last.insert(k, (seq, v));
                }
            }
        }
        self.history.txns.push(TxnRecord {
            id: a.txn,
            outcome: Outcome::Committed,
            commit_ts: ts,
            reads: a.reads.into_iter().map(|x| x.1).collect(),
            range_reads: a.range_reads.into_iter().map(|x| x.1).collect(),
            writes: last.into_iter().map(|(key, (_, value))| WriteEvent { key, value }).collect(),
        });
        self.hist_index.insert(a.txn, self.history.txns.len() - 1);
        if let Some(p) = self.early_patches.remove(&a.txn) {
            let rec = self.history.txns.last_mut().unwrap();
            for (key, value) in p {
                if let Some(w) = rec.writes.iter_mut().find(|w| w.key == key) {
                    w.value = value;
                }
            }
        }
        for sh in a.shards {
            self.send(Ep::Coord(s.node), Ep::Shard(sh), Msg::S(ToShard::Resolve { txn: a.txn, commit_ts: ts }), 0, false);
        }
        self.after_commit(s);
    }

    fn after_commit(&mut self, s: &mut Session) {
        s.block_prio = None;
        s.st.epoch += 1;
        s.out_committed.append(&mut s.st.output);
        s.wait = Wait::None;
        let at_end = matches!(s.code.ins[s.st.pc], Ins::End);
        if at_end {
            self.finish(s, Ok(()));
            return;
        }
        s.st.pc += 1;
        s.snap = s.st.clone();
    }
}

/// Escrow: an UPDATE whose every assignment is `c = c + k` / `c = c - k` / `c = k + c`
/// with a known integer `k` becomes a commutative delta.
fn as_delta(sets: &[(ColId, Expr)]) -> Option<Vec<(ColId, i64)>> {
    sets.iter()
        .map(|(c, e)| match e {
            Expr::Binary(op, a, b) => match (op, a.as_ref(), b.as_ref()) {
                (BinOp::Add, Expr::OldCol(x), Expr::Const(Value::Int(k)))
                | (BinOp::Add, Expr::Const(Value::Int(k)), Expr::OldCol(x))
                    if x == c =>
                {
                    Some((*c, *k))
                }
                (BinOp::Sub, Expr::OldCol(x), Expr::Const(Value::Int(k))) if x == c => k.checked_neg().map(|n| (*c, n)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The shard for a one-phase commit, if every write still pending is unsent and goes
/// to one shard that also holds every earlier intent, and nothing pushed the
/// transaction (no refresh needed).
fn one_pc_shard(a: &Attempt) -> Option<ShardId> {
    if a.write_ts != a.read_ts || a.deferred_err.is_some() || a.outbox.len() != 1 {
        return None;
    }
    let (&sh, unsent) = a.outbox.iter().next().unwrap();
    if a.intent_shards.iter().any(|s| *s != sh) {
        return None;
    }
    let unsent: HashSet<u32> = unsent.iter().map(|(q, _)| *q).collect();
    let all_writes = a.outbox[&sh].iter().all(|(_, op)| {
        matches!(op, ShardOp::Update { .. } | ShardOp::Insert { .. } | ShardOp::Delete { .. } | ShardOp::Delta { .. })
    });
    let pending_unsent = a.ops.iter().filter(|(_, o)| !o.ready && !o.cancelled).all(|(q, _)| unsent.contains(q));
    (all_writes && pending_unsent).then_some(sh)
}

fn issue(a: &mut Attempt, pc: usize, r: Remote) -> u32 {
    let seq = a.next_seq;
    a.next_seq += 1;
    let parts = r.parts.len();
    a.ops.insert(
        seq,
        Op {
            pc,
            parts,
            got: 0,
            ready: parts == 0,
            cancelled: false,
            retired: false,
            for_update: r.for_update,
            err: None,
            key: r.key,
            table: r.table,
            lo: r.lo,
            limit: r.limit,
            read_value: None,
            applied: false,
            scan_parts: vec![],
            rows: vec![],
            dur_expected: 0,
            dur_got: 0,
            part_shards: r.parts.iter().map(|p| p.0).collect(),
        },
    );
    for (sh, op) in r.parts {
        a.shards.insert(sh);
        a.outbox.entry(sh).or_default().push((seq, op));
    }
    seq
}
