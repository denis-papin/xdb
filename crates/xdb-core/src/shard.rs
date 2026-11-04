//! A shard leader: MVCC storage, write intents, read-timestamp cache and the
//! priority-based conflict rules (§5 of the design doc). One core serves its
//! micro-instructions one at a time, highest priority first.

use crate::config::ConflictRule;
use crate::eval::{eval, RowEnv};
use crate::types::*;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use xdb_ir::{errcode, ColId, Expr, Key, Row, TableId, Ts, TxnId};

#[derive(Clone, Debug)]
pub struct Version {
    pub ts: Ts,
    pub value: Option<Row>,
    pub writer: Option<TxnId>,
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

#[derive(Clone, Debug, Default)]
pub struct Record {
    /// Committed versions, ascending timestamp.
    pub versions: Vec<Version>,
    pub intent: Option<Intent>,
}

impl Record {
    fn at(&self, ts: Ts) -> Option<&Version> {
        self.versions.iter().rev().find(|v| v.ts <= ts)
    }
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
}

enum Step {
    Done(MiResult, Option<Ts>, bool, usize),
    Park(Key),
}

enum Conf {
    Go,
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
            Step::Done(result, push, wrote, rows) => {
                if self.blocked.get(&mi.txn.id).map_or(false, |b| b.0 == mi.seq) {
                    self.unblock(mi.txn.id);
                }
                if push.is_some() {
                    self.stats.pushes += 1;
                }
                Some(Done { mi_seq: mi.seq, txn: mi.txn, result, push_above: push, wrote, rows })
            }
        }
    }

    // ---------------------------------------------------------------- conflicts

    fn conflict(&mut self, key: Key, me: &TxnMeta, write: bool, g: &mut Global, out: &mut Vec<Out>) -> Conf {
        loop {
            let Some(int) = self.data.get(&key).and_then(|r| r.intent.as_ref()) else {
                return Conf::Go;
            };
            if int.txn == me.id {
                return Conf::Go;
            }
            let (h, its, hprio) = (int.txn, int.ts, int.prio);
            match g.status(h) {
                Status::Committed(cts) => self.resolve_key(key, h, cts),
                Status::Aborted => self.drop_intent(key, h),
                Status::Staging => {
                    if !write && its > me.read_ts {
                        return Conf::Go;
                    }
                    return Conf::Park;
                }
                Status::Running => {
                    if !write && its > me.read_ts {
                        return Conf::Go;
                    }
                    let me_older = me.prio < hprio;
                    match self.rule {
                        ConflictRule::WoundWait => {
                            if me_older {
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
                                self.drop_intent(key, h);
                            } else {
                                return Conf::Park;
                            }
                        }
                        ConflictRule::WaitDie => {
                            if me_older {
                                return Conf::Park;
                            }
                            self.stats.dies += 1;
                            return Conf::Die;
                        }
                    }
                }
            }
        }
    }

    fn resolve_key(&mut self, key: Key, h: TxnId, cts: Ts) {
        if let Some(rec) = self.data.get_mut(&key) {
            if rec.intent.as_ref().map_or(false, |i| i.txn == h) {
                let int = rec.intent.take().unwrap();
                let value = int.stack.last().and_then(|(_, v)| v.clone());
                rec.versions.push(Version { ts: cts, value, writer: Some(h) });
                debug_assert!(rec.versions.windows(2).all(|w| w[0].ts < w[1].ts));
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
        }
        if let Some(ks) = self.txn_keys.get_mut(&h) {
            ks.remove(&key);
        }
        self.wake(key);
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
        match self.conflict(key, me, false, g, out) {
            Conf::Park => return Step::Park(key),
            Conf::Die => return self.die(me, g),
            Conf::Go => {}
        }
        let (value, writer) = match self.data.get(&key) {
            None => (None, None),
            Some(rec) => match &rec.intent {
                Some(i) if i.txn == me.id => (i.stack.last().unwrap().1.clone(), Some(me.id)),
                _ => rec.at(me.read_ts).map(|v| (v.value.clone(), v.writer)).unwrap_or((None, None)),
            },
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
        match self.conflict(key, &me, true, g, out) {
            Conf::Park => return Step::Park(key),
            Conf::Die => return self.die(&me, g),
            Conf::Go => {}
        }
        let other_read = if self.fault_ignore_read_ts { None } else { self.max_other_read(key, me.id) };
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
            match self.conflict(k, &me, false, g, out) {
                Conf::Park => return Step::Park(k),
                Conf::Die => return self.die(&me, g),
                Conf::Go => {}
            }
            n += 1;
            let rec = &self.data[&k];
            let (val, writer) = match &rec.intent {
                Some(i) if i.txn == me.id => (i.stack.last().unwrap().1.clone(), Some(me.id)),
                _ => rec.at(me.read_ts).map(|v| (v.value.clone(), v.writer)).unwrap_or((None, None)),
            };
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
                match self.conflict(*k, &me, true, g, out) {
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
                if let Some(i) = &rec.intent {
                    if i.txn != me {
                        let bad = match g.status(i.txn) {
                            Status::Committed(c) => c > from && c <= to,
                            Status::Aborted => false,
                            _ => i.ts <= to,
                        };
                        if bad {
                            self.stats.refresh_fail += 1;
                            return false;
                        }
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

    pub fn resolve(&mut self, txn: TxnId, cts: Ts) {
        let keys: Vec<Key> = self.txn_keys.remove(&txn).map(|s| s.into_iter().collect()).unwrap_or_default();
        for k in keys {
            self.resolve_key(k, txn, cts);
        }
        self.cancelled.remove(&txn);
    }

    pub fn abort(&mut self, txn: TxnId) {
        let keys: Vec<Key> = self.txn_keys.remove(&txn).map(|s| s.into_iter().collect()).unwrap_or_default();
        for k in keys {
            self.drop_intent(k, txn);
        }
        self.cancel(txn, 0, u32::MAX);
        self.cancelled.remove(&txn);
        self.blocked.remove(&txn);
    }

    pub fn rollback_to(&mut self, txn: TxnId, from_seq: u32, to_seq: u32) {
        self.cancel(txn, from_seq, to_seq);
        self.cancelled.entry(txn).or_default().push((from_seq, to_seq));
        let keys: Vec<Key> = self.txn_keys.get(&txn).map(|s| s.iter().copied().collect()).unwrap_or_default();
        for k in keys {
            let mut emptied = false;
            if let Some(rec) = self.data.get_mut(&k) {
                if let Some(i) = &mut rec.intent {
                    if i.txn == txn {
                        i.stack.retain(|(s, _)| *s < from_seq || *s > to_seq);
                        if i.stack.is_empty() {
                            rec.intent = None;
                            emptied = true;
                        }
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
        self.data.values().filter(|r| r.intent.is_some()).count()
    }
}

enum WK<'a> {
    Lock,
    Update(&'a Vec<(ColId, Expr)>),
    Insert(&'a Row),
    Delete,
}
