//! Independent serializability checker for an xdb [`History`].
//!
//! The database claims that committed transactions are serializable in
//! `commit_ts` order and that every read of a committed transaction is valid as
//! of its `commit_ts`. This crate does not trust that claim; it verifies it with
//! three independent passes:
//!
//! 1. well-formedness and aborted reads (G1a),
//! 2. a timestamp-order replay that recomputes what every read should have seen
//!    (stale reads, lost updates, phantoms, ...),
//! 3. an Adya direct serialization graph (ww / wr / rw edges, version order =
//!    `commit_ts` order of committed writers) searched for cycles with Tarjan's
//!    SCC algorithm (G0 / G1c / G2).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use xdb_ir::{History, Key, Outcome, Row, TableId, Ts, TxnId, Value};

/// Maximum number of violations kept in [`CheckReport::violations`].
/// All violations are counted in [`CheckReport::total_violations`].
pub const MAX_STORED_VIOLATIONS: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckReport {
    pub ok: bool,
    /// The first [`MAX_STORED_VIOLATIONS`] violations found.
    pub violations: Vec<Violation>,
    pub stats: CheckStats,
    /// Number of violations found (stored or not).
    pub total_violations: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckStats {
    pub committed: usize,
    pub aborted: usize,
    /// Point reads (all transactions).
    pub reads: usize,
    /// Range reads (all transactions).
    pub range_reads: usize,
    /// Write events (all transactions).
    pub writes: usize,
    /// Distinct (from, to, kind) edges of the dependency graph.
    pub dsg_edges: usize,
}

/// Kind of a dependency edge of the direct serialization graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepKind {
    /// Write-write: `to` installed the next version after `from`'s.
    Ww,
    /// Write-read: `to` read the version installed by `from`.
    Wr,
    /// Read-write anti-dependency: `from` read a version, `to` installed the next one.
    Rw,
}

impl fmt::Display for DepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DepKind::Ww => "ww",
            DepKind::Wr => "wr",
            DepKind::Rw => "rw",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DepEdge {
    pub from: TxnId,
    pub to: TxnId,
    pub kind: DepKind,
    /// One key that induces the edge.
    pub key: Key,
}

/// Adya anomaly class of a dependency cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CycleClass {
    /// Only ww edges.
    G0,
    /// ww and wr edges, at least one wr.
    G1c,
    /// At least one rw anti-dependency.
    G2,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    /// Two transaction records share the same id (only the first one is analysed).
    DuplicateTxnId { id: TxnId },
    /// Two committed transactions share the same `commit_ts`.
    DuplicateCommitTs { ts: Ts, first: TxnId, second: TxnId },
    /// A read refers to a transaction id that is not in the history.
    UnknownObserved { reader: TxnId, key: Key, observed: TxnId },
    /// A transaction lists the same key twice in its writes.
    DuplicateWrite { txn: TxnId, key: Key },
    /// A range read whose rows are not sorted, duplicated or outside `[lo, hi]`.
    MalformedRangeRead { reader: TxnId, table: TableId, lo: i64, hi: i64, reason: String },
    /// G1a: a read observed a write of an aborted transaction.
    AbortedRead { reader: TxnId, key: Key, writer: TxnId },
    /// A transaction claims to read its own write on a key it never writes.
    OwnReadWithoutWrite { reader: TxnId, key: Key },
    /// A point read does not match the state at the reader's `commit_ts`.
    ReadMismatch {
        reader: TxnId,
        commit_ts: Ts,
        key: Key,
        expected_writer: Option<TxnId>,
        expected_value: Option<Row>,
        observed_writer: Option<TxnId>,
        observed_value: Option<Row>,
    },
    /// A range read does not match the state at the reader's `commit_ts` on one key.
    /// `None` = row absent, `Some(w)` = row present, written by `w` (`None` = initial).
    RangeMismatch {
        reader: TxnId,
        commit_ts: Ts,
        table: TableId,
        lo: i64,
        hi: i64,
        pk: i64,
        expected: Option<Option<TxnId>>,
        observed: Option<Option<TxnId>>,
    },
    /// A cycle in the dependency graph (one per strongly connected component).
    Cycle {
        class: CycleClass,
        /// The concrete cycle: `edges[i].to == edges[i + 1].from`, last `to` == first `from`.
        edges: Vec<DepEdge>,
        /// Number of transactions in the strongly connected component.
        scc_size: usize,
    },
}

fn fmt_writer(w: Option<TxnId>) -> String {
    match w {
        Some(id) => format!("T{id}"),
        None => "initial".to_string(),
    }
}

fn fmt_row(r: &Option<Row>) -> String {
    match r {
        None => "absent".to_string(),
        Some(row) => {
            let vals: Vec<String> = row.iter().map(|v: &Value| v.to_string()).collect();
            format!("({})", vals.join(", "))
        }
    }
}

fn fmt_presence(p: &Option<Option<TxnId>>) -> String {
    match p {
        None => "absent".to_string(),
        Some(w) => format!("present (written by {})", fmt_writer(*w)),
    }
}

fn fmt_key(k: &Key) -> String {
    format!("t{}/{}", k.table, k.pk)
}

fn fmt_ts(ts: &Ts) -> String {
    format!("{}.{}@n{}", ts.wall, ts.logical, ts.node)
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::DuplicateTxnId { id } => write!(f, "duplicate transaction id T{id}"),
            Violation::DuplicateCommitTs { ts, first, second } => write!(
                f,
                "committed T{first} and T{second} share commit_ts {}",
                fmt_ts(ts)
            ),
            Violation::UnknownObserved { reader, key, observed } => write!(
                f,
                "T{reader} read {} observing unknown transaction T{observed}",
                fmt_key(key)
            ),
            Violation::DuplicateWrite { txn, key } => {
                write!(f, "T{txn} lists key {} twice in its writes", fmt_key(key))
            }
            Violation::MalformedRangeRead { reader, table, lo, hi, reason } => write!(
                f,
                "T{reader} range read t{table}[{lo}, {hi}] is malformed: {reason}"
            ),
            Violation::AbortedRead { reader, key, writer } => write!(
                f,
                "G1a: T{reader} read {} written by aborted T{writer}",
                fmt_key(key)
            ),
            Violation::OwnReadWithoutWrite { reader, key } => write!(
                f,
                "T{reader} claims to read its own write on {} but never writes it",
                fmt_key(key)
            ),
            Violation::ReadMismatch {
                reader,
                commit_ts,
                key,
                expected_writer,
                expected_value,
                observed_writer,
                observed_value,
            } => write!(
                f,
                "T{reader} (commit_ts {}) read {}: expected {} from {}, observed {} from {}",
                fmt_ts(commit_ts),
                fmt_key(key),
                fmt_row(expected_value),
                fmt_writer(*expected_writer),
                fmt_row(observed_value),
                fmt_writer(*observed_writer)
            ),
            Violation::RangeMismatch { reader, commit_ts, table, lo, hi, pk, expected, observed } => {
                let what = match (expected, observed) {
                    (None, Some(_)) => "phantom/ghost row",
                    (Some(_), None) => "missing row",
                    _ => "wrong version",
                };
                write!(
                    f,
                    "T{reader} (commit_ts {}) range read t{table}[{lo}, {hi}] key {pk}: {what}: expected {}, observed {}",
                    fmt_ts(commit_ts),
                    fmt_presence(expected),
                    fmt_presence(observed)
                )
            }
            Violation::Cycle { class, edges, scc_size } => {
                write!(f, "{class:?} cycle (scc of {scc_size} txns):")?;
                for e in edges {
                    write!(f, " T{} -{}({})->", e.from, e.kind, fmt_key(&e.key))?;
                }
                if let Some(e) = edges.first() {
                    write!(f, " T{}", e.from)?;
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

struct Sink {
    stored: Vec<Violation>,
    total: usize,
}

impl Sink {
    fn push(&mut self, v: Violation) {
        self.total += 1;
        if self.stored.len() < MAX_STORED_VIOLATIONS {
            self.stored.push(v);
        }
    }
}

/// Indices into `h.txns` of the committed transactions (first record of each id)
/// sorted by `(commit_ts, position)`.
fn committed_order(h: &History) -> Vec<usize> {
    let mut seen: BTreeSet<TxnId> = BTreeSet::new();
    let mut order: Vec<usize> = Vec::new();
    for (i, t) in h.txns.iter().enumerate() {
        if seen.insert(t.id) && t.outcome == Outcome::Committed {
            order.push(i);
        }
    }
    order.sort_by(|&a, &b| h.txns[a].commit_ts.cmp(&h.txns[b].commit_ts).then(a.cmp(&b)));
    order
}

/// Final committed state: initial rows with committed writes applied in commit_ts order.
pub fn final_state(h: &History) -> BTreeMap<Key, Row> {
    let mut state: BTreeMap<Key, Row> = h.initial.iter().cloned().collect();
    for i in committed_order(h) {
        for w in &h.txns[i].writes {
            match &w.value {
                Some(row) => {
                    state.insert(w.key, row.clone());
                }
                None => {
                    state.remove(&w.key);
                }
            }
        }
    }
    state
}

#[derive(Clone, Copy)]
struct Entry<'a> {
    value: Option<&'a Row>,
    writer: Option<TxnId>,
}

/// Committed versions of one key, in version (= commit_ts) order.
#[derive(Default)]
struct Versions {
    /// Rank (position in commit order) of each writer.
    ranks: Vec<u32>,
    /// Whether each write is a delete.
    deletes: Vec<bool>,
    /// `last_absent[p]` = latest version position `<= p` at which the row is absent,
    /// or -1. Position 0 = initial version, position `i + 1` = `ranks[i]`.
    last_absent: Vec<i64>,
}

#[derive(Clone, Copy)]
struct RawEdge {
    from: u32,
    to: u32,
    kind: DepKind,
    key: Key,
}

pub fn check(h: &History) -> CheckReport {
    let mut sink = Sink { stored: Vec::new(), total: 0 };
    let mut stats = CheckStats::default();

    // ---- 1. well-formedness ------------------------------------------------
    // id -> index of first record.
    let mut by_id: HashMap<TxnId, usize> = HashMap::with_capacity(h.txns.len());
    for (i, t) in h.txns.iter().enumerate() {
        match t.outcome {
            Outcome::Committed => stats.committed += 1,
            Outcome::Aborted => stats.aborted += 1,
        }
        stats.reads += t.reads.len();
        stats.range_reads += t.range_reads.len();
        stats.writes += t.writes.len();
        if by_id.contains_key(&t.id) {
            sink.push(Violation::DuplicateTxnId { id: t.id });
        } else {
            by_id.insert(t.id, i);
        }
    }
    let first_record = |i: usize| by_id.get(&h.txns[i].id) == Some(&i);

    let order = committed_order(h);
    for w in order.windows(2) {
        let (a, b) = (&h.txns[w[0]], &h.txns[w[1]]);
        if a.commit_ts == b.commit_ts {
            sink.push(Violation::DuplicateCommitTs { ts: a.commit_ts, first: a.id, second: b.id });
        }
    }
    let mut rank_of: HashMap<TxnId, u32> = HashMap::with_capacity(order.len());
    for (r, &i) in order.iter().enumerate() {
        rank_of.insert(h.txns[i].id, r as u32);
    }

    // Is the observed id acceptable as a source of a version (known, not aborted,
    // or the reader itself)? Reports UnknownObserved / AbortedRead otherwise.
    let classify = |reader: TxnId, key: Key, obs: Option<TxnId>, sink: &mut Sink| -> bool {
        match obs {
            None => true,
            Some(o) if o == reader => true,
            Some(o) => match by_id.get(&o) {
                None => {
                    sink.push(Violation::UnknownObserved { reader, key, observed: o });
                    false
                }
                Some(&j) if h.txns[j].outcome == Outcome::Aborted => {
                    sink.push(Violation::AbortedRead { reader, key, writer: o });
                    false
                }
                Some(_) => true,
            },
        }
    };

    // Sorted write keys of each analysed txn; duplicates reported.
    let mut write_keys: Vec<Vec<Key>> = Vec::with_capacity(h.txns.len());
    for (i, t) in h.txns.iter().enumerate() {
        if !first_record(i) {
            write_keys.push(Vec::new());
            continue;
        }
        let mut ks: Vec<Key> = t.writes.iter().map(|w| w.key).collect();
        ks.sort_unstable();
        for w in ks.windows(2) {
            if w[0] == w[1] {
                sink.push(Violation::DuplicateWrite { txn: t.id, key: w[0] });
            }
        }
        ks.dedup();
        write_keys.push(ks);
    }

    // G1a / unknown for aborted txns too (they are not replayed).
    for (i, t) in h.txns.iter().enumerate() {
        if !first_record(i) || t.outcome == Outcome::Committed {
            continue;
        }
        for r in &t.reads {
            classify(t.id, r.key, r.observed, &mut sink);
        }
        for rr in &t.range_reads {
            for &(pk, obs) in &rr.rows {
                classify(t.id, Key { table: rr.table, pk }, obs, &mut sink);
            }
        }
    }

    // ---- 2. timestamp-order replay -----------------------------------------
    let mut state: BTreeMap<Key, Entry> = BTreeMap::new();
    for (k, row) in &h.initial {
        state.insert(*k, Entry { value: Some(row), writer: None });
    }
    // Sorted, validated observed rows per (txn index, range read index), reused by the DSG.
    let mut range_rows: HashMap<(usize, usize), Vec<(i64, Option<TxnId>)>> = HashMap::new();

    for &ti in &order {
        let t = &h.txns[ti];
        let writes = &write_keys[ti];
        let writes_key = |k: &Key| writes.binary_search(k).is_ok();

        for r in &t.reads {
            if !classify(t.id, r.key, r.observed, &mut sink) {
                continue;
            }
            if r.observed == Some(t.id) {
                if !writes_key(&r.key) {
                    sink.push(Violation::OwnReadWithoutWrite { reader: t.id, key: r.key });
                }
                continue;
            }
            let e = state.get(&r.key).copied().unwrap_or(Entry { value: None, writer: None });
            if r.value.as_ref() != e.value || r.observed != e.writer {
                sink.push(Violation::ReadMismatch {
                    reader: t.id,
                    commit_ts: t.commit_ts,
                    key: r.key,
                    expected_writer: e.writer,
                    expected_value: e.value.cloned(),
                    observed_writer: r.observed,
                    observed_value: r.value.clone(),
                });
            }
        }

        for (ri, rr) in t.range_reads.iter().enumerate() {
            let table = rr.table;
            // Validate shape.
            let mut reasons: Vec<String> = Vec::new();
            for w in rr.rows.windows(2) {
                if w[0].0 >= w[1].0 {
                    reasons.push(format!("rows not strictly increasing at pk {}", w[1].0));
                    break;
                }
            }
            if let Some(&(pk, _)) = rr.rows.iter().find(|(pk, _)| *pk < rr.lo || *pk > rr.hi) {
                reasons.push(format!("pk {pk} outside the range"));
            }
            if !reasons.is_empty() {
                sink.push(Violation::MalformedRangeRead {
                    reader: t.id,
                    table,
                    lo: rr.lo,
                    hi: rr.hi,
                    reason: reasons.join("; "),
                });
            }
            let mut rows = rr.rows.clone();
            rows.sort_by_key(|&(pk, _)| pk);
            rows.dedup_by_key(|&mut (pk, _)| pk);

            // Keys to ignore on both sides (bad observed, already reported).
            let mut ignored: Vec<i64> = Vec::new();
            // Observed rows to compare (not self-observed, not self-written).
            let mut observed: Vec<(i64, Option<TxnId>)> = Vec::with_capacity(rows.len());
            for &(pk, obs) in &rows {
                let key = Key { table, pk };
                if !classify(t.id, key, obs, &mut sink) {
                    ignored.push(pk);
                    continue;
                }
                if obs == Some(t.id) {
                    if !writes_key(&key) {
                        sink.push(Violation::OwnReadWithoutWrite { reader: t.id, key });
                    }
                    continue;
                }
                if writes_key(&key) {
                    // Read before the txn's own write: must be the snapshot version.
                    let e = state.get(&key).copied().unwrap_or(Entry { value: None, writer: None });
                    if e.value.is_none() || e.writer != obs {
                        sink.push(Violation::RangeMismatch {
                            reader: t.id,
                            commit_ts: t.commit_ts,
                            table,
                            lo: rr.lo,
                            hi: rr.hi,
                            pk,
                            expected: e.value.map(|_| e.writer),
                            observed: Some(obs),
                        });
                    }
                    continue;
                }
                observed.push((pk, obs));
            }

            // Merge with the replay state.
            let mut exp_iter: Box<dyn Iterator<Item = (i64, Option<TxnId>)>> = if rr.lo <= rr.hi {
                Box::new(
                    state
                        .range(Key { table, pk: rr.lo }..=Key { table, pk: rr.hi })
                        .filter(|(k, e)| {
                            e.value.is_some()
                                && !writes_key(k)
                                && ignored.binary_search(&k.pk).is_err()
                        })
                        .map(|(k, e)| (k.pk, e.writer)),
                )
            } else {
                Box::new(std::iter::empty())
            };
            let mut obs_iter = observed.iter().copied();
            let mut ne = exp_iter.next();
            let mut no = obs_iter.next();
            loop {
                let (pk, expected, seen) = match (ne, no) {
                    (None, None) => break,
                    (Some((ep, ew)), Some((op, ow))) if ep == op => {
                        ne = exp_iter.next();
                        no = obs_iter.next();
                        if ew == ow {
                            continue;
                        }
                        (ep, Some(ew), Some(ow))
                    }
                    (Some((ep, ew)), Some((op, _))) if ep < op => {
                        ne = exp_iter.next();
                        (ep, Some(ew), None)
                    }
                    (Some((ep, ew)), None) => {
                        ne = exp_iter.next();
                        (ep, Some(ew), None)
                    }
                    (_, Some((op, ow))) => {
                        no = obs_iter.next();
                        (op, None, Some(ow))
                    }
                };
                sink.push(Violation::RangeMismatch {
                    reader: t.id,
                    commit_ts: t.commit_ts,
                    table,
                    lo: rr.lo,
                    hi: rr.hi,
                    pk,
                    expected,
                    observed: seen,
                });
            }
            drop(exp_iter);
            range_rows.insert((ti, ri), rows);
        }

        for w in &t.writes {
            state.insert(w.key, Entry { value: w.value.as_ref(), writer: Some(t.id) });
        }
    }
    drop(state);

    // ---- 3. dependency graph -------------------------------------------------
    let initial_keys: BTreeSet<Key> = h.initial.iter().map(|(k, _)| *k).collect();
    let mut versions: BTreeMap<Key, Versions> = BTreeMap::new();
    for (r, &ti) in order.iter().enumerate() {
        let t = &h.txns[ti];
        for w in &t.writes {
            let v = versions.entry(w.key).or_default();
            if v.ranks.last() == Some(&(r as u32)) {
                // Duplicate write event (already reported): last one wins, as in the replay.
                *v.deletes.last_mut().unwrap() = w.value.is_none();
                continue;
            }
            v.ranks.push(r as u32);
            v.deletes.push(w.value.is_none());
        }
    }
    for (k, v) in versions.iter_mut() {
        let mut la = Vec::with_capacity(v.ranks.len() + 1);
        let mut cur: i64 = if initial_keys.contains(k) { -1 } else { 0 };
        la.push(cur);
        for (i, &d) in v.deletes.iter().enumerate() {
            if d {
                cur = (i + 1) as i64;
            }
            la.push(cur);
        }
        v.last_absent = la;
    }

    let mut edges: Vec<RawEdge> = Vec::new();
    for (k, v) in &versions {
        for w in v.ranks.windows(2) {
            edges.push(RawEdge { from: w[0], to: w[1], kind: DepKind::Ww, key: *k });
        }
    }

    // Edges for a read by `reader` of version position `vpos` of key `k`.
    fn read_edges(edges: &mut Vec<RawEdge>, v: &Versions, k: Key, reader: u32, vpos: usize) {
        if vpos > 0 {
            let w = v.ranks[vpos - 1];
            if w != reader {
                edges.push(RawEdge { from: w, to: reader, kind: DepKind::Wr, key: k });
            }
        }
        if vpos < v.ranks.len() {
            let next = v.ranks[vpos];
            if next != reader {
                edges.push(RawEdge { from: reader, to: next, kind: DepKind::Rw, key: k });
            }
        }
    }
    // Version position of a read observing `obs` (None = initial), if it exists.
    let version_pos = |v: &Versions, obs: Option<TxnId>| -> Option<usize> {
        match obs {
            None => Some(0),
            Some(o) => {
                let r = *rank_of.get(&o)?;
                v.ranks.binary_search(&r).ok().map(|i| i + 1)
            }
        }
    };

    for (r, &ti) in order.iter().enumerate() {
        let r = r as u32;
        let t = &h.txns[ti];
        for rd in &t.reads {
            if rd.observed == Some(t.id) {
                continue;
            }
            let Some(v) = versions.get(&rd.key) else { continue };
            if let Some(p) = version_pos(v, rd.observed) {
                read_edges(&mut edges, v, rd.key, r, p);
            }
        }
        for (ri, rr) in t.range_reads.iter().enumerate() {
            if rr.lo > rr.hi {
                continue;
            }
            let Some(rows) = range_rows.get(&(ti, ri)) else { continue };
            let lo = Key { table: rr.table, pk: rr.lo };
            let hi = Key { table: rr.table, pk: rr.hi };
            for (k, v) in versions.range(lo..=hi) {
                if v.ranks.binary_search(&r).is_ok() {
                    // Keys the reader writes are skipped (cannot tell which version).
                    continue;
                }
                match rows.binary_search_by_key(&k.pk, |&(pk, _)| pk) {
                    Ok(i) => {
                        if let Some(p) = version_pos(v, rows[i].1) {
                            read_edges(&mut edges, v, *k, r, p);
                        }
                    }
                    Err(_) => {
                        // Absent at read time: the latest absent version before the reader.
                        let before = v.ranks.partition_point(|&x| x < r);
                        let p = v.last_absent[before];
                        if p >= 0 {
                            read_edges(&mut edges, v, *k, r, p as usize);
                        }
                    }
                }
            }
        }
    }
    drop(range_rows);

    edges.sort_unstable_by_key(|e| (e.from, e.to, e.kind));
    edges.dedup_by_key(|e| (e.from, e.to, e.kind));
    stats.dsg_edges = edges.len();

    let n = order.len();
    let mut off = vec![0usize; n + 1];
    for e in &edges {
        off[e.from as usize + 1] += 1;
    }
    for i in 0..n {
        off[i + 1] += off[i];
    }
    // Edges are sorted by `from`, so they already form the CSR adjacency.
    let graph = Graph { off, edges: &edges };

    let comp2 = graph.tarjan(2);
    if comp2.nontrivial > 0 {
        let comp0 = graph.tarjan(0);
        let comp1 = graph.tarjan(1);
        let mut members: Vec<Vec<u32>> = vec![Vec::new(); comp2.count];
        for (v, &c) in comp2.comp.iter().enumerate() {
            if comp2.size[c as usize] > 1 {
                members[c as usize].push(v as u32);
            }
        }
        let mut bfs = Bfs::new(n);
        for m in members.iter().filter(|m| !m.is_empty()) {
            let (class, level, comps, start) = if let Some(&x) =
                m.iter().find(|&&x| comp0.size[comp0.comp[x as usize] as usize] > 1)
            {
                (CycleClass::G0, 0u8, &comp0, x)
            } else if let Some(&x) =
                m.iter().find(|&&x| comp1.size[comp1.comp[x as usize] as usize] > 1)
            {
                (CycleClass::G1c, 1u8, &comp1, x)
            } else {
                (CycleClass::G2, 2u8, &comp2, m[0])
            };
            let cyc = bfs.cycle(&graph, comps, level, start);
            let edges_out: Vec<DepEdge> = cyc
                .iter()
                .map(|&ei| {
                    let e = &edges[ei];
                    DepEdge {
                        from: h.txns[order[e.from as usize]].id,
                        to: h.txns[order[e.to as usize]].id,
                        kind: e.kind,
                        key: e.key,
                    }
                })
                .collect();
            // A cycle found at level 2 might still only use ww/wr edges; classify it.
            let class = if class == CycleClass::G2 {
                if edges_out.iter().any(|e| e.kind == DepKind::Rw) {
                    CycleClass::G2
                } else if edges_out.iter().any(|e| e.kind == DepKind::Wr) {
                    CycleClass::G1c
                } else {
                    CycleClass::G0
                }
            } else {
                class
            };
            sink.push(Violation::Cycle { class, edges: edges_out, scc_size: m.len() });
        }
    }

    CheckReport {
        ok: sink.total == 0,
        violations: sink.stored,
        stats,
        total_violations: sink.total,
    }
}

fn level_of(k: DepKind) -> u8 {
    match k {
        DepKind::Ww => 0,
        DepKind::Wr => 1,
        DepKind::Rw => 2,
    }
}

struct Graph<'a> {
    off: Vec<usize>,
    edges: &'a [RawEdge],
}

struct Sccs {
    comp: Vec<u32>,
    size: Vec<usize>,
    count: usize,
    nontrivial: usize,
}

impl Graph<'_> {
    /// Iterative Tarjan over edges whose level is `<= level`.
    fn tarjan(&self, level: u8) -> Sccs {
        let n = self.off.len() - 1;
        const UNSET: u32 = u32::MAX;
        let mut index = vec![UNSET; n];
        let mut low = vec![0u32; n];
        let mut on_stack = vec![false; n];
        let mut comp = vec![UNSET; n];
        let mut size: Vec<usize> = Vec::new();
        let mut stack: Vec<u32> = Vec::new();
        let mut call: Vec<(u32, usize)> = Vec::new();
        let mut next_index = 0u32;
        for s in 0..n {
            if index[s] != UNSET {
                continue;
            }
            index[s] = next_index;
            low[s] = next_index;
            next_index += 1;
            stack.push(s as u32);
            on_stack[s] = true;
            call.push((s as u32, self.off[s]));
            while let Some(&(v, ei)) = call.last() {
                let v = v as usize;
                if ei < self.off[v + 1] {
                    call.last_mut().unwrap().1 += 1;
                    let e = &self.edges[ei];
                    if level_of(e.kind) > level {
                        continue;
                    }
                    let w = e.to as usize;
                    if index[w] == UNSET {
                        index[w] = next_index;
                        low[w] = next_index;
                        next_index += 1;
                        stack.push(w as u32);
                        on_stack[w] = true;
                        call.push((w as u32, self.off[w]));
                    } else if on_stack[w] {
                        low[v] = low[v].min(index[w]);
                    }
                } else {
                    call.pop();
                    if let Some(&(u, _)) = call.last() {
                        let u = u as usize;
                        low[u] = low[u].min(low[v]);
                    }
                    if low[v] == index[v] {
                        let c = size.len() as u32;
                        let mut sz = 0;
                        loop {
                            let w = stack.pop().unwrap() as usize;
                            on_stack[w] = false;
                            comp[w] = c;
                            sz += 1;
                            if w == v {
                                break;
                            }
                        }
                        size.push(sz);
                    }
                }
            }
        }
        let nontrivial = size.iter().filter(|&&s| s > 1).count();
        Sccs { comp, count: size.len(), size, nontrivial }
    }
}

struct Bfs {
    stamp: Vec<u32>,
    parent_edge: Vec<usize>,
    cur: u32,
}

impl Bfs {
    fn new(n: usize) -> Self {
        Bfs { stamp: vec![0; n], parent_edge: vec![0; n], cur: 0 }
    }

    /// Shortest cycle through `start` inside its component, using edges of level
    /// `<= level`. Returns edge indices in cycle order.
    fn cycle(&mut self, g: &Graph, sccs: &Sccs, level: u8, start: u32) -> Vec<usize> {
        self.cur += 1;
        let c = sccs.comp[start as usize];
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(start);
        self.stamp[start as usize] = self.cur;
        while let Some(v) = queue.pop_front() {
            for ei in g.off[v as usize]..g.off[v as usize + 1] {
                let e = &g.edges[ei];
                if level_of(e.kind) > level || sccs.comp[e.to as usize] != c {
                    continue;
                }
                if e.to == start {
                    let mut path = vec![ei];
                    let mut x = v;
                    while x != start {
                        let pe = self.parent_edge[x as usize];
                        path.push(pe);
                        x = g.edges[pe].from;
                    }
                    path.reverse();
                    return path;
                }
                let w = e.to as usize;
                if self.stamp[w] != self.cur {
                    self.stamp[w] = self.cur;
                    self.parent_edge[w] = ei;
                    queue.push_back(e.to);
                }
            }
        }
        Vec::new() // unreachable for a nontrivial SCC
    }
}

#[cfg(test)]
mod tests;
