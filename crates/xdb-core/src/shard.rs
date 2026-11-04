//! A shard leader: MVCC storage, write intents, read-timestamp cache and the
//! priority-based conflict rules (§5 of the design doc). One core serves its
//! micro-instructions one at a time, highest priority first.
//!
//! Experiment: commutative "delta" intents (escrow, O'Neil 1986). `c = c + x` updates
//! from different transactions do not conflict with each other; they conflict with
//! reads and normal writes like intents do, and are applied in commit-timestamp order.

use crate::config::ConflictRule;
use crate::eval::{eval, RowEnv};
use crate::types::*;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use xdb_ir::{errcode, ColId, Expr, Key, Row, TableId, Ts, TxnId, Value};

pub type Delta = Vec<(ColId, i64)>;

#[derive(Clone, Debug)]
pub struct Version {
    pub ts: Ts,
    pub value: Option<Row>,
    pub writer: Option<TxnId>,
    /// Set when this version was produced by a delta: its value is recomputed if an
    /// earlier delta commits later (retroactively, below it).
    pub delta: Option<Delta>,
}

#[derive(Clone, Debug)]
pub struct Intent {
    pub txn: TxnId,
    /// Lower bound of the writer's commit timestamp.
    pub ts: Ts,
    pub prio: Priority,
    /// Provisional values by statement sequence number (savepoint rollback pops).
    pub stack: Vec<(u32, Option<Row>)>,
}

#[derive(Clone, Debug)]
pub struct DeltaIntent {
    pub txn: TxnId,
    pub ts: Ts,
    pub prio: Priority,
    pub stack: Vec<(u32, Delta)>,
}

#[derive(Clone, Debug, Default)]
pub struct Record {
    /// Committed versions, ascending timestamp.
    pub versions: Vec<Version>,
    pub intent: Option<Intent>,
    pub deltas: Vec<DeltaIntent>,
}

impl Record {
    fn at(&self, ts: Ts) -> Option<&Version> {
        self.versions.iter().rev().find(|v| v.ts <= ts)
    }
}

fn merge(stack: &[(u32, Delta)]) -> Delta {
    let mut m: BTreeMap<ColId, i64> = BTreeMap::new();
    for (_, d) in stack {
        for (c, x) in d {
            *m.entry(*c).or_insert(0) += x;
        }
    }
    m.into_iter().collect()
}

fn apply_delta(row: &Option<Row>, d: &[(ColId, i64)]) -> Option<Row> {
    row.as_ref().map(|r| {
        let mut r = r.clone();
        for (c, x) in d {
            if let Value::Int(v) = r[*c as usize] {
                r[*c as usize] = Value::Int(v.saturating_add(*x));
            }
        }
        r
    })
}

#[derive(Clone, Debug)]
pub struct Mi {
    pub txn: TxnMeta,
    pub seq: u32,
    pub op: ShardOp,
}

struct Ready(Mi);
impl Ready {
    fn key(&self) -> (Priority, TxnId, u32) {
        (self.0.txn.prio, self.0.txn.id, self.0.seq)
    }
}
impl PartialEq for Ready {
    fn eq(&self, o: &Self) -> bool {
        self.key() == o.key()
    }
}
impl Eq for Ready {}
impl PartialOrd for Ready {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Ready {
    // BinaryHeap is a max-heap: reverse so the oldest priority pops first.
    fn cmp(&self, o: &Self) -> Ordering {
        o.key().cmp(&self.key())
    }
}

#[derive(Clone, Debug, Default)]
pub struct ShardStats {
    pub processed: u64,
    pub parks: u64,
    pub wounds: u64,
    pub dies: u64,
    pub pushes: u64,
    pub refresh_fail: u64,
    pub deltas: u64,
    pub one_pc_ok: u64,
    pub one_pc_fail: u64,
}

pub struct Done {
    pub mi_seq: u32,
    pub txn: TxnMeta,
    pub result: MiResult,
    pub push_above: Option<Ts>,
    /// The MI placed an intent that must be replicated before commit.
    pub wrote: bool,
    /// Rows scanned (for CPU cost).
    pub rows: usize,
    /// No response to send now (a one-phase commit answers after replication).
    pub silent: bool,
}

enum Step {
    Done(MiResult, Option<Ts>, bool, usize),
    Silent,
    Park(Key),
}

enum Conf {
    Go,
    Park,
    Die,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Read,
    Write,
    Delta,
}

enum Act {
    Retry,
    Skip,
    Park,
    Die,
}

pub struct Shard {
    pub id: ShardId,
    pub data: BTreeMap<Key, Record>,
    /// Top read timestamps per key, at most 2 distinct readers.
    tscache: HashMap<Key, Vec<(Ts, TxnId)>>,
    intervals: Vec<(TableId, i64, i64, Ts, TxnId)>,
    txn_keys: HashMap<TxnId, BTreeSet<Key>>,
    parked: HashMap<Key, Vec<Mi>>,
    /// A transaction with a parked MI here: (seq of the parked head, later MIs held).
    blocked: HashMap<TxnId, (u32, Vec<Mi>)>,
    ready: BinaryHeap<Ready>,
    cancelled: HashMap<TxnId, Vec<(u32, u32)>>,
    /// Highest timestamp a transaction's writes were pushed above on this shard.
    txn_push: HashMap<TxnId, Ts>,
    /// Transactions whose write failed here (blocks a one-phase commit).
    txn_err: HashSet<TxnId>,
    /// Final values of delta writes, for the history: (writer, key, value).
    pub patches: Vec<(TxnId, Key, Option<Row>)>,
    pub busy: bool,
    pub stats: ShardStats,
    rule: ConflictRule,
    pub fault_ignore_read_ts: bool,
}

impl Shard {
    pub fn new(id: ShardId, rule: ConflictRule) -> Self {
        Shard {
            id,
            data: BTreeMap::new(),
            tscache: HashMap::new(),
            intervals: Vec::new(),
            txn_keys: HashMap::new(),
            parked: HashMap::new(),
            blocked: HashMap::new(),
            ready: BinaryHeap::new(),
            cancelled: HashMap::new(),
            txn_push: HashMap::new(),
            txn_err: HashSet::new(),
            patches: Vec::new(),
            busy: false,
            stats: ShardStats::default(),
            rule,
            fault_ignore_read_ts: false,
        }
    }

    pub fn load(&mut self, key: Key, row: Row) {
        self.data.entry(key).or_default().versions.push(Version {
            ts: Ts::default(),
            value: Some(row),
            writer: None,
            delta: None,
        });
    }

    pub fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    pub fn enqueue(&mut self, mi: Mi) {
        if let Some((_, held)) = self.blocked.get_mut(&mi.txn.id) {
            held.push(mi);
        } else {
            self.ready.push(Ready(mi));
        }
    }

    fn is_cancelled(&self, mi: &Mi) -> bool {
        self.cancelled
            .get(&mi.txn.id)
            .map_or(false, |rs| rs.iter().any(|(a, b)| mi.seq >= *a && mi.seq <= *b))
    }

    /// Next MI to serve, or None if idle.
    pub fn next(&mut self, g: &Global) -> Option<Mi> {
        while let Some(Ready(mi)) = self.ready.pop() {
            if g.status(mi.txn.id) == Status::Aborted || self.is_cancelled(&mi) {
                // A dropped head must not leave its transaction blocked here.
                if self.blocked.get(&mi.txn.id).map_or(false, |b| b.0 == mi.seq) {
                    self.unblock(mi.txn.id);
                }
                continue;
            }
            if let Some((head, held)) = self.blocked.get_mut(&mi.txn.id) {
                if *head != mi.seq {
                    held.push(mi);
                    continue;
                }
            }
            return Some(mi);
        }
        None
    }

    fn wake(&mut self, key: Key) {
        if let Some(list) = self.parked.remove(&key) {
            for mi in list {
                self.ready.push(Ready(mi));
            }
        }
    }

    fn unblock(&mut self, txn: TxnId) {
        if let Some((_, held)) = self.blocked.remove(&txn) {
            for mi in held {
                self.ready.push(Ready(mi));
            }
        }
    }

    /// Serve one MI. Returns None when it parked behind a conflicting intent.
    pub fn process(&mut self, mi: Mi, g: &mut Global, out: &mut Vec<Out>) -> Option<Done> {
        self.stats.processed += 1;
        let step = match &mi.op {
            ShardOp::Get { key, for_update: false } => self.get(&mi, *key, g, out),
            ShardOp::Get { key, for_update: true } => self.write_like(&mi, *key, WK::Lock, g, out),
            ShardOp::Update { key, sets } => self.write_like(&mi, *key, WK::Update(sets), g, out),
            ShardOp::Insert { key, row } => self.write_like(&mi, *key, WK::Insert(row), g, out),
            ShardOp::Delete { key } => self.write_like(&mi, *key, WK::Delete, g, out),
            ShardOp::Delta { key, adds } => self.delta(&mi, *key, adds, g, out),
            ShardOp::Commit1PC => self.one_pc(&mi, g, out),
            ShardOp::Scan { table, lo, hi, filter, limit, for_update } => {
                self.scan(&mi, *table, *lo, *hi, filter.as_ref(), *limit, *for_update, g, out)
            }
        };
        match step {
            Step::Park(key) => {
                self.stats.parks += 1;
                self.blocked.entry(mi.txn.id).or_insert((mi.seq, Vec::new()));
                self.parked.entry(key).or_default().push(mi);
                None
            }
            Step::Done(..) | Step::Silent => {
                if self.blocked.get(&mi.txn.id).map_or(false, |b| b.0 == mi.seq) {
                    self.unblock(mi.txn.id);
                }
                let (result, push, wrote, rows, silent) = match step {
                    Step::Done(r, p, w, n) => (r, p, w, n, false),
                    _ => (MiResult::OnePc { ok: true }, None, false, 0, true),
                };
                if let Some(p) = push {
                    self.stats.pushes += 1;
                    let e = self.txn_push.entry(mi.txn.id).or_insert(p);
                    if p > *e {
                        *e = p;
                    }
                }
                if matches!(result, MiResult::Err(_)) {
                    self.txn_err.insert(mi.txn.id);
                }
                Some(Done { mi_seq: mi.seq, txn: mi.txn, result, push_above: push, wrote, rows, silent })
            }
        }
    }

    // ---------------------------------------------------------------- conflicts

    fn wound(&mut self, h: TxnId, g: &mut Global, out: &mut Vec<Out>) {
        self.stats.wounds += 1;
        g.set(h, Status::Aborted);
        if let Some(hs) = g.txns.get(&h) {
            out.push(Out {
                to: Ep::Coord(hs.node),
                msg: Msg::C(ToCoord::Wounded { sid: hs.sid, txn: h }),
                extra_ns: 0,
                after_replication: false,
            });
        }
    }

    /// Decide what to do about another transaction `h`'s intent (or delta) on `key`.
    #[allow(clippy::too_many_arguments)]
    fn decide(
        &mut self,
        key: Key,
        h: TxnId,
        its: Ts,
        hprio: Priority,
        me: &TxnMeta,
        write: bool,
        g: &mut Global,
        out: &mut Vec<Out>,
    ) -> Act {
        match g.status(h) {
            Status::Committed(cts) => {
                self.resolve_key(key, h, cts);
                Act::Retry
            }
            Status::Aborted => {
                self.drop_intent(key, h);
                Act::Retry
            }
            Status::Staging => {
                if !write && its > me.read_ts {
                    Act::Skip
                } else {
                    Act::Park
                }
            }
            Status::Running => {
                if !write && its > me.read_ts {
                    return Act::Skip;
                }
                let me_older = me.prio < hprio;
                match (self.rule, me_older) {
                    (ConflictRule::WoundWait, true) => {
                        self.wound(h, g, out);
                        self.drop_intent(key, h);
                        Act::Retry
                    }
                    (ConflictRule::WoundWait, false) => Act::Park,
                    (ConflictRule::WaitDie, true) => Act::Park,
                    (ConflictRule::WaitDie, false) => {
                        self.stats.dies += 1;
                        Act::Die
                    }
                }
            }
        }
    }

    fn conflict(&mut self, key: Key, me: &TxnMeta, mode: Mode, g: &mut Global, out: &mut Vec<Out>) -> Conf {
        'outer: loop {
            let Some(rec) = self.data.get(&key) else { return Conf::Go };
            let full = rec.intent.as_ref().filter(|i| i.txn != me.id).map(|i| (i.txn, i.ts, i.prio));
            let others: Vec<(TxnId, Ts, Priority)> = if mode == Mode::Delta {
                vec![]
            } else {
                rec.deltas.iter().filter(|d| d.txn != me.id).map(|d| (d.txn, d.ts, d.prio)).collect()
            };
            for (h, its, hp) in full.into_iter().chain(others) {
                match self.decide(key, h, its, hp, me, mode != Mode::Read, g, out) {
                    Act::Retry => continue 'outer,
                    Act::Skip => {}
                    Act::Park => return Conf::Park,
                    Act::Die => return Conf::Die,
                }
            }
            return Conf::Go;
        }
    }

    fn resolve_key(&mut self, key: Key, h: TxnId, cts: Ts) {
        if let Some(rec) = self.data.get_mut(&key) {
            if rec.intent.as_ref().map_or(false, |i| i.txn == h) {
                let int = rec.intent.take().unwrap();
                let value = int.stack.last().and_then(|(_, v)| v.clone());
                debug_assert!(rec.versions.last().map_or(true, |v| v.ts < cts));
                rec.versions.push(Version { ts: cts, value, writer: Some(h), delta: None });
            }
            if let Some(pos) = rec.deltas.iter().position(|d| d.txn == h) {
                let d = rec.deltas.remove(pos);
                let sum = merge(&d.stack);
                let at = rec.versions.partition_point(|v| v.ts < cts);
                let base = if at > 0 { rec.versions[at - 1].value.clone() } else { None };
                let value = apply_delta(&base, &sum);
                rec.versions.insert(at, Version { ts: cts, value: value.clone(), writer: Some(h), delta: Some(sum) });
                self.patches.push((h, key, value));
                // Later delta versions committed earlier in real time: recompute.
                for i in at + 1..rec.versions.len() {
                    let Some(dv) = rec.versions[i].delta.clone() else { break };
                    let v = apply_delta(&rec.versions[i - 1].value, &dv);
                    rec.versions[i].value = v.clone();
                    if let Some(w) = rec.versions[i].writer {
                        self.patches.push((w, key, v));
                    }
                }
            }
        }
        if let Some(ks) = self.txn_keys.get_mut(&h) {
            ks.remove(&key);
        }
        self.wake(key);
    }

    fn drop_intent(&mut self, key: Key, h: TxnId) {
        if let Some(rec) = self.data.get_mut(&key) {
            if rec.intent.as_ref().map_or(false, |i| i.txn == h) {
                rec.intent = None;
            }
            rec.deltas.retain(|d| d.txn != h);
        }
        if let Some(ks) = self.txn_keys.get_mut(&h) {
            ks.remove(&key);
        }
        self.wake(key);
    }

    /// Turn my own pending delta on `key` into a normal intent (before a normal write).
    /// The caller has already resolved every other transaction's delta on the key.
    fn materialize(&mut self, key: Key, me: TxnId) {
        let Some(rec) = self.data.get_mut(&key) else { return };
        let Some(pos) = rec.deltas.iter().position(|d| d.txn == me) else { return };
        let d = rec.deltas.remove(pos);
        let mut v = rec.versions.last().and_then(|x| x.value.clone());
        let mut stack = vec![];
        for (seq, dd) in &d.stack {
            v = apply_delta(&v, dd);
            stack.push((*seq, v.clone()));
        }
        rec.intent = Some(Intent { txn: me, ts: d.ts, prio: d.prio, stack });
    }

    /// What `me` sees in `rec` at `ts`: own intent, else committed version + own delta.
    fn view(rec: &Record, me: TxnId, ts: Ts) -> (Option<Row>, Option<TxnId>) {
        if let Some(i) = rec.intent.as_ref().filter(|i| i.txn == me) {
            return (i.stack.last().unwrap().1.clone(), Some(me));
        }
        let (v, w) = rec.at(ts).map(|v| (v.value.clone(), v.writer)).unwrap_or((None, None));
        match rec.deltas.iter().find(|d| d.txn == me) {
            Some(d) => (apply_delta(&v, &merge(&d.stack)), Some(me)),
            None => (v, w),
        }
    }

    // ---------------------------------------------------------------- ts cache

    fn bump(&mut self, key: Key, ts: Ts, txn: TxnId) {
        let e = self.tscache.entry(key).or_default();
        if let Some(x) = e.iter_mut().find(|(_, t)| *t == txn) {
            if ts > x.0 {
                x.0 = ts;
            }
        } else {
            e.push((ts, txn));
        }
        e.sort_by(|a, b| b.0.cmp(&a.0));
        e.truncate(2);
    }

    fn bump_range(&mut self, table: TableId, lo: i64, hi: i64, ts: Ts, txn: TxnId, g: &Global) {
        if self.intervals.len() > 4096 {
            if let Some(min) = g.min_active_read_ts() {
                self.intervals.retain(|i| i.3 >= min);
            }
        }
        self.intervals.push((table, lo, hi, ts, txn));
    }

    fn max_other_read(&self, key: Key, me: TxnId) -> Option<Ts> {
        if self.fault_ignore_read_ts {
            return None;
        }
        let p = self
            .tscache
            .get(&key)
            .and_then(|e| e.iter().find(|(_, t)| *t != me).map(|x| x.0));
        let r = self
            .intervals
            .iter()
            .filter(|i| i.0 == key.table && i.1 <= key.pk && key.pk <= i.2 && i.4 != me)
            .map(|i| i.3)
            .max();
        p.max(r)
    }

    // ---------------------------------------------------------------- ops

    fn get(&mut self, mi: &Mi, key: Key, g: &mut Global, out: &mut Vec<Out>) -> Step {
        let me = &mi.txn;
        match self.conflict(key, me, Mode::Read, g, out) {
            Conf::Park => return Step::Park(key),
            Conf::Die => return self.die(me, g),
            Conf::Go => {}
        }
        let (value, writer) = match self.data.get(&key) {
            None => (None, None),
            Some(rec) => Self::view(rec, me.id, me.read_ts),
        };
        self.bump(key, me.read_ts, me.id);
        Step::Done(MiResult::Read { value, writer }, None, false, 1)
    }

    fn die(&mut self, me: &TxnMeta, g: &mut Global) -> Step {
        g.set(me.id, Status::Aborted);
        Step::Done(MiResult::Aborted, None, false, 0)
    }

    fn write_like(&mut self, mi: &Mi, key: Key, kind: WK, g: &mut Global, out: &mut Vec<Out>) -> Step {
        let me = mi.txn.clone();
        match self.conflict(key, &me, Mode::Write, g, out) {
            Conf::Park => return Step::Park(key),
            Conf::Die => return self.die(&me, g),
            Conf::Go => {}
        }
        self.materialize(key, me.id);
        let other_read = self.max_other_read(key, me.id);
        let rec = self.data.entry(key).or_default();
        let own = rec.intent.as_ref().map_or(false, |i| i.txn == me.id);
        let (cur, cur_writer, latest_ts) = if own {
            (rec.intent.as_ref().unwrap().stack.last().unwrap().1.clone(), Some(me.id), None)
        } else {
            match rec.versions.last() {
                Some(v) => (v.value.clone(), v.writer, Some(v.ts)),
                None => (None, None, None),
            }
        };
        let mut push: Option<Ts> = None;
        if !own {
            if let Some(lt) = latest_ts {
                if lt >= me.write_ts {
                    push = push.max(Some(lt));
                }
            }
            if let Some(rt) = other_read {
                if rt >= me.write_ts {
                    push = push.max(Some(rt));
                }
            }
        }
        let (new, applied) = match kind {
            WK::Lock => (cur.clone(), false),
            WK::Delete => (None, cur.is_some()),
            WK::Insert(row) => {
                if cur.is_some() {
                    return Step::Done(MiResult::Err(errcode::DUP_VAL_ON_INDEX), None, false, 1);
                }
                (Some(row.clone()), true)
            }
            WK::Update(sets) => match &cur {
                None => (None, false),
                Some(row) => {
                    let mut r = row.clone();
                    for (c, e) in sets {
                        match eval(e, &RowEnv { row }) {
                            Ok(v) => r[*c as usize] = v,
                            Err(code) => return Step::Done(MiResult::Err(code), None, false, 1),
                        }
                    }
                    (Some(r), true)
                }
            },
        };
        let its = match push {
            Some(p) => me.write_ts.max(bumped(p)),
            None => me.write_ts,
        };
        let rec = self.data.get_mut(&key).unwrap();
        match &mut rec.intent {
            Some(i) if i.txn == me.id => i.stack.push((mi.seq, new.clone())),
            slot => {
                *slot = Some(Intent { txn: me.id, ts: its, prio: me.prio, stack: vec![(mi.seq, new.clone())] })
            }
        }
        self.txn_keys.entry(me.id).or_default().insert(key);
        let result = match kind {
            WK::Lock => MiResult::Read { value: cur, writer: cur_writer },
            _ => MiResult::Write { new_value: new, applied, prev: cur, prev_writer: cur_writer },
        };
        Step::Done(result, push, true, 1)
    }

    /// Escrow: add `adds` to integer columns without reading the row.
    fn delta(&mut self, mi: &Mi, key: Key, adds: &Delta, g: &mut Global, out: &mut Vec<Out>) -> Step {
        let me = mi.txn.clone();
        match self.conflict(key, &me, Mode::Delta, g, out) {
            Conf::Park => return Step::Park(key),
            Conf::Die => return self.die(&me, g),
            Conf::Go => {}
        }
        let other_read = self.max_other_read(key, me.id);
        let rec = self.data.entry(key).or_default();
        if let Some(i) = rec.intent.as_mut().filter(|i| i.txn == me.id) {
            // Already a normal writer of this row: just update the provisional value.
            let top = i.stack.last().unwrap().1.clone();
            let new = apply_delta(&top, adds);
            i.stack.push((mi.seq, new.clone()));
            return Step::Done(
                MiResult::Write { new_value: new, applied: top.is_some(), prev: top, prev_writer: Some(me.id) },
                None,
                true,
                1,
            );
        }
        let own_delta = rec.deltas.iter().any(|d| d.txn == me.id);
        let latest = rec.versions.last().map(|v| (v.ts, v.value.is_some()));
        if !own_delta && !latest.map_or(false, |l| l.1) {
            // Missing row: behave like a normal UPDATE (0 rows, absence locked).
            return self.write_like(mi, key, WK::Update(&vec![]), g, out);
        }
        let mut push: Option<Ts> = None;
        if !own_delta {
            if let Some((lt, _)) = latest {
                if lt >= me.write_ts {
                    push = Some(lt);
                }
            }
            if let Some(rt) = other_read {
                if rt >= me.write_ts {
                    push = push.max(Some(rt));
                }
            }
        }
        let its = match push {
            Some(p) => me.write_ts.max(bumped(p)),
            None => me.write_ts,
        };
        let rec = self.data.get_mut(&key).unwrap();
        match rec.deltas.iter_mut().find(|d| d.txn == me.id) {
            Some(d) => d.stack.push((mi.seq, adds.clone())),
            None => rec.deltas.push(DeltaIntent { txn: me.id, ts: its, prio: me.prio, stack: vec![(mi.seq, adds.clone())] }),
        }
        self.txn_keys.entry(me.id).or_default().insert(key);
        self.stats.deltas += 1;
        Step::Done(MiResult::Delta { applied: true }, push, true, 1)
    }

    /// One-phase commit of a transaction whose writes are all on this shard and were
    /// sent in the same batch. Processed after them (same transaction, higher seq).
    fn one_pc(&mut self, mi: &Mi, g: &mut Global, out: &mut Vec<Out>) -> Step {
        let me = &mi.txn;
        let err = self.txn_err.remove(&me.id);
        let pushed = self.txn_push.get(&me.id).map_or(false, |p| *p >= me.write_ts);
        let ok = !err && !pushed && me.write_ts == me.read_ts && g.status(me.id) == Status::Running;
        if !ok {
            self.stats.one_pc_fail += 1;
            return Step::Done(MiResult::OnePc { ok: false }, None, false, 0);
        }
        self.stats.one_pc_ok += 1;
        // No wound from now on; commit once the batch is replicated.
        g.set(me.id, Status::Staging);
        out.push(Out {
            to: Ep::Shard(self.id),
            msg: Msg::S(ToShard::Finish1PC { txn: me.id, ts: me.write_ts, sid: me.sid, node: me.node }),
            extra_ns: 0,
            after_replication: true,
        });
        Step::Silent
    }

    #[allow(clippy::too_many_arguments)]
    fn scan(
        &mut self,
        mi: &Mi,
        table: TableId,
        lo: i64,
        hi: i64,
        filter: Option<&Expr>,
        limit: Option<u64>,
        for_update: bool,
        g: &mut Global,
        out: &mut Vec<Out>,
    ) -> Step {
        let me = mi.txn.clone();
        let keys: Vec<Key> = if lo > hi {
            vec![]
        } else {
            self.data
                .range(Key { table, pk: lo }..=Key { table, pk: hi })
                .map(|(k, _)| *k)
                .collect()
        };
        let mut rows = vec![];
        let mut seen = vec![];
        let mut locks = vec![];
        let mut hi_eff = hi;
        let mut n = 0usize;
        for k in keys {
            match self.conflict(k, &me, Mode::Read, g, out) {
                Conf::Park => return Step::Park(k),
                Conf::Die => return self.die(&me, g),
                Conf::Go => {}
            }
            n += 1;
            let (val, writer) = Self::view(&self.data[&k], me.id, me.read_ts);
            if let Some(row) = val {
                seen.push((k.pk, writer));
                let pass = match filter {
                    None => true,
                    Some(f) => match eval(f, &RowEnv { row: &row }) {
                        Ok(v) => v.is_true(),
                        Err(code) => return Step::Done(MiResult::Err(code), None, false, n),
                    },
                };
                if pass {
                    rows.push(row);
                    locks.push(k);
                    if limit.map_or(false, |l| rows.len() as u64 >= l) {
                        hi_eff = k.pk;
                        break;
                    }
                }
            }
        }
        if limit == Some(0) {
            hi_eff = lo.saturating_sub(1);
        }
        let mut push = None;
        let mut wrote = false;
        if for_update && !locks.is_empty() {
            // All conflicts must be resolved before any lock is placed.
            for k in &locks {
                match self.conflict(*k, &me, Mode::Write, g, out) {
                    Conf::Park => return Step::Park(*k),
                    Conf::Die => return self.die(&me, g),
                    Conf::Go => {}
                }
            }
            for k in &locks {
                if let Step::Done(_, p, _, _) = self.write_like(mi, *k, WK::Lock, g, out) {
                    push = push.max(p);
                }
            }
            wrote = true;
        }
        if hi_eff >= lo {
            self.bump_range(table, lo, hi_eff, me.read_ts, me.id, g);
        }
        Step::Done(MiResult::Scan { rows, seen, hi_eff }, push, wrote, n)
    }

    // ---------------------------------------------------------------- txn messages

    /// Re-validate read spans for a commit-timestamp move `from -> to`.
    pub fn refresh(&mut self, me: TxnId, spans: &[Span], from: Ts, to: Ts, g: &Global) -> bool {
        for s in spans {
            let keys: Vec<Key> = match s {
                Span::Point(k) => vec![*k],
                Span::Range(t, lo, hi) => {
                    if lo > hi {
                        vec![]
                    } else {
                        self.data
                            .range(Key { table: *t, pk: *lo }..=Key { table: *t, pk: *hi })
                            .map(|(k, _)| *k)
                            .collect()
                    }
                }
            };
            for k in keys {
                let Some(rec) = self.data.get(&k) else { continue };
                if rec.versions.iter().any(|v| v.ts > from && v.ts <= to && v.writer != Some(me)) {
                    self.stats.refresh_fail += 1;
                    return false;
                }
                let pending = rec
                    .intent
                    .iter()
                    .map(|i| (i.txn, i.ts))
                    .chain(rec.deltas.iter().map(|d| (d.txn, d.ts)))
                    .filter(|(t, _)| *t != me);
                for (t, its) in pending {
                    let bad = match g.status(t) {
                        Status::Committed(c) => c > from && c <= to,
                        Status::Aborted => false,
                        _ => its <= to,
                    };
                    if bad {
                        self.stats.refresh_fail += 1;
                        return false;
                    }
                }
            }
        }
        for s in spans {
            match s {
                Span::Point(k) => self.bump(*k, to, me),
                Span::Range(t, lo, hi) => self.bump_range(*t, *lo, *hi, to, me, g),
            }
        }
        true
    }

    /// Mark the transaction record staged (parallel commit) or prepared (2PC).
    pub fn stage(&mut self, txn: TxnId, g: &mut Global) -> bool {
        match g.status(txn) {
            Status::Running => {
                g.set(txn, Status::Staging);
                true
            }
            Status::Staging => true,
            _ => false,
        }
    }

    fn forget(&mut self, txn: TxnId) {
        self.cancelled.remove(&txn);
        self.txn_push.remove(&txn);
        self.txn_err.remove(&txn);
    }

    pub fn resolve(&mut self, txn: TxnId, cts: Ts) {
        let keys: Vec<Key> = self.txn_keys.remove(&txn).map(|s| s.into_iter().collect()).unwrap_or_default();
        for k in keys {
            self.resolve_key(k, txn, cts);
        }
        self.forget(txn);
    }

    pub fn abort(&mut self, txn: TxnId) {
        let keys: Vec<Key> = self.txn_keys.remove(&txn).map(|s| s.into_iter().collect()).unwrap_or_default();
        for k in keys {
            self.drop_intent(k, txn);
        }
        self.cancel(txn, 0, u32::MAX);
        self.forget(txn);
        self.blocked.remove(&txn);
    }

    pub fn rollback_to(&mut self, txn: TxnId, from_seq: u32, to_seq: u32) {
        self.cancel(txn, from_seq, to_seq);
        self.cancelled.entry(txn).or_default().push((from_seq, to_seq));
        let keys: Vec<Key> = self.txn_keys.get(&txn).map(|s| s.iter().copied().collect()).unwrap_or_default();
        let gone = |s: u32| s >= from_seq && s <= to_seq;
        for k in keys {
            let mut emptied = false;
            if let Some(rec) = self.data.get_mut(&k) {
                if let Some(i) = &mut rec.intent {
                    if i.txn == txn {
                        i.stack.retain(|(s, _)| !gone(*s));
                        if i.stack.is_empty() {
                            rec.intent = None;
                            emptied = true;
                        }
                    }
                }
                if let Some(pos) = rec.deltas.iter().position(|d| d.txn == txn) {
                    rec.deltas[pos].stack.retain(|(s, _)| !gone(*s));
                    if rec.deltas[pos].stack.is_empty() {
                        rec.deltas.remove(pos);
                        emptied = true;
                    }
                }
            }
            if emptied {
                self.txn_keys.get_mut(&txn).unwrap().remove(&k);
                self.wake(k);
            }
        }
    }

    /// Remove queued MIs of `txn` with seq in `[a, b]`.
    fn cancel(&mut self, txn: TxnId, a: u32, b: u32) {
        let hit = |mi: &Mi| mi.txn.id == txn && mi.seq >= a && mi.seq <= b;
        let mut head_gone = false;
        let head = self.blocked.get(&txn).map(|x| x.0);
        for list in self.parked.values_mut() {
            list.retain(|mi| {
                let h = hit(mi);
                if h && Some(mi.seq) == head {
                    head_gone = true;
                }
                !h
            });
        }
        self.parked.retain(|_, l| !l.is_empty());
        if let Some((h, held)) = self.blocked.get_mut(&txn) {
            held.retain(|mi| !hit(mi));
            if *h >= a && *h <= b {
                head_gone = true;
            }
        }
        if head_gone {
            self.unblock(txn);
        }
    }

    /// Committed value of a key at the latest version (for tests / final state).
    pub fn latest(&self, key: &Key) -> Option<&Row> {
        self.data.get(key).and_then(|r| r.versions.last()).and_then(|v| v.value.as_ref())
    }

    pub fn debug_queues(&self) -> String {
        let parked: Vec<_> = self
            .parked
            .iter()
            .map(|(k, l)| (k.pk, l.iter().map(|m| (m.txn.id, m.seq)).collect::<Vec<_>>()))
            .collect();
        let blocked: Vec<_> = self.blocked.iter().map(|(t, (h, held))| (*t, *h, held.len())).collect();
        format!("ready {} parked {:?} blocked {:?} busy {}", self.ready.len(), parked, blocked, self.busy)
    }

    pub fn pending_intents(&self) -> usize {
        self.data.values().filter(|r| r.intent.is_some() || !r.deltas.is_empty()).count()
    }
}

enum WK<'a> {
    Lock,
    Update(&'a Vec<(ColId, Expr)>),
    Insert(&'a Row),
    Delete,
}
