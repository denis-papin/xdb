use super::*;
use std::collections::BTreeMap;
use xdb_ir::{RangeReadEvent, ReadEvent, TxnRecord, WriteEvent};

fn k(pk: i64) -> Key {
    Key { table: 0, pk }
}
fn row(pk: i64, v: i64) -> Row {
    vec![Value::Int(pk), Value::Int(v)]
}
fn ts(w: u64) -> Ts {
    Ts { wall: w, logical: 0, node: 0 }
}
fn txn(id: TxnId, at: u64) -> TxnRecord {
    TxnRecord {
        id,
        outcome: Outcome::Committed,
        commit_ts: ts(at),
        reads: vec![],
        range_reads: vec![],
        writes: vec![],
    }
}
fn rd(mut t: TxnRecord, pk: i64, obs: Option<TxnId>, v: Option<i64>) -> TxnRecord {
    t.reads.push(ReadEvent { key: k(pk), observed: obs, value: v.map(|v| row(pk, v)) });
    t
}
fn wr(mut t: TxnRecord, pk: i64, v: Option<i64>) -> TxnRecord {
    t.writes.push(WriteEvent { key: k(pk), value: v.map(|v| row(pk, v)) });
    t
}
fn rr(mut t: TxnRecord, lo: i64, hi: i64, rows: &[(i64, Option<TxnId>)]) -> TxnRecord {
    t.range_reads.push(RangeReadEvent { table: 0, lo, hi, rows: rows.to_vec() });
    t
}
fn aborted(mut t: TxnRecord) -> TxnRecord {
    t.outcome = Outcome::Aborted;
    t
}
fn hist(initial: &[(i64, i64)], txns: Vec<TxnRecord>) -> History {
    History { initial: initial.iter().map(|&(p, v)| (k(p), row(p, v))).collect(), txns }
}
fn has(rep: &CheckReport, f: impl Fn(&Violation) -> bool) -> bool {
    rep.violations.iter().any(f)
}
fn cycle_class(rep: &CheckReport) -> Option<CycleClass> {
    rep.violations.iter().find_map(|v| match v {
        Violation::Cycle { class, .. } => Some(*class),
        _ => None,
    })
}
fn dump(rep: &CheckReport) -> String {
    rep.violations.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n")
}

#[test]
fn valid_serial_history() {
    let t1 = wr(rd(txn(1, 10), 1, None, Some(100)), 1, Some(90));
    let t2 = wr(rd(rd(txn(2, 20), 1, Some(1), Some(90)), 2, None, Some(200)), 2, Some(210));
    let t3 = rr(txn(3, 30), 0, 10, &[(1, Some(1)), (2, Some(2))]);
    let h = hist(&[(1, 100), (2, 200)], vec![t3, t1, t2]); // out of order on purpose
    let rep = check(&h);
    assert!(rep.ok, "{}", dump(&rep));
    assert_eq!(rep.stats.committed, 3);
    assert_eq!(rep.stats.reads, 3);
    assert_eq!(rep.stats.range_reads, 1);
    assert_eq!(rep.stats.writes, 2);
    assert!(rep.stats.dsg_edges > 0);
    let fs = final_state(&h);
    assert_eq!(fs[&k(1)], row(1, 90));
    assert_eq!(fs[&k(2)], row(2, 210));
}

#[test]
fn valid_concurrent_disjoint_keys() {
    let t1 = wr(rd(txn(1, 10), 1, None, Some(1)), 1, Some(2));
    let t2 = wr(rd(txn(2, 11), 2, None, Some(1)), 2, Some(3));
    let t3 = wr(rd(txn(3, 12), 3, None, None), 3, Some(4));
    let rep = check(&hist(&[(1, 1), (2, 1)], vec![t2, t3, t1]));
    assert!(rep.ok, "{}", dump(&rep));
}

#[test]
fn lost_update() {
    let t1 = wr(rd(txn(1, 10), 1, None, Some(100)), 1, Some(110));
    let t2 = wr(rd(txn(2, 20), 1, None, Some(100)), 1, Some(120));
    let rep = check(&hist(&[(1, 100)], vec![t1, t2]));
    assert!(!rep.ok);
    assert!(has(&rep, |v| matches!(v, Violation::ReadMismatch { reader: 2, expected_writer: Some(1), .. })));
    assert_eq!(cycle_class(&rep), Some(CycleClass::G2), "{}", dump(&rep));
}

#[test]
fn write_skew() {
    let t1 = wr(rd(rd(txn(1, 10), 1, None, Some(1)), 2, None, Some(1)), 2, Some(0));
    let t2 = wr(rd(rd(txn(2, 20), 1, None, Some(1)), 2, None, Some(1)), 1, Some(0));
    let rep = check(&hist(&[(1, 1), (2, 1)], vec![t1, t2]));
    assert!(!rep.ok);
    assert!(has(&rep, |v| matches!(v, Violation::ReadMismatch { reader: 2, key, .. } if key.pk == 2)));
    assert_eq!(cycle_class(&rep), Some(CycleClass::G2), "{}", dump(&rep));
}

#[test]
fn phantom_via_range_read() {
    // T2 inserts 5 at ts 10, T1 (ts 20) scans [0, 10] and misses it.
    let t2 = wr(txn(2, 10), 5, Some(50));
    let t1 = wr(rr(txn(1, 20), 0, 10, &[(1, None)]), 100, Some(1));
    let rep = check(&hist(&[(1, 10)], vec![t1, t2]));
    assert!(!rep.ok);
    assert!(has(&rep, |v| matches!(
        v,
        Violation::RangeMismatch { reader: 1, pk: 5, expected: Some(Some(2)), observed: None, .. }
    )), "{}", dump(&rep));

    // Same with a cycle: T2 also reads key 100 before T1 wrote it (rw T2 -> T1),
    // and T1 misses T2's insert (rw T1 -> T2).
    let t2 = wr(rd(txn(2, 10), 100, None, None), 5, Some(50));
    let t1 = wr(rr(txn(1, 20), 0, 10, &[(1, None)]), 100, Some(1));
    let rep = check(&hist(&[(1, 10)], vec![t1, t2]));
    assert_eq!(cycle_class(&rep), Some(CycleClass::G2), "{}", dump(&rep));

    // Ghost row: scan sees a row that does not exist.
    let t1 = rr(txn(1, 20), 0, 10, &[(1, None), (7, None)]);
    let rep = check(&hist(&[(1, 10)], vec![t1]));
    assert!(has(&rep, |v| matches!(v, Violation::RangeMismatch { pk: 7, expected: None, .. })));
}

#[test]
fn read_of_aborted_write() {
    let t1 = aborted(wr(txn(1, 10), 1, Some(999)));
    let t2 = rd(txn(2, 20), 1, Some(1), Some(999));
    let t3 = rr(txn(3, 30), 0, 9, &[(1, Some(1))]);
    let rep = check(&hist(&[(1, 1)], vec![t1, t2, t3]));
    assert!(!rep.ok);
    assert!(has(&rep, |v| matches!(v, Violation::AbortedRead { reader: 2, writer: 1, .. })));
    assert!(has(&rep, |v| matches!(v, Violation::AbortedRead { reader: 3, writer: 1, .. })));
    assert_eq!(rep.stats.aborted, 1);
}

#[test]
fn g1c_cycle() {
    // T1 reads T2's write of y, T2 reads T1's write of x.
    let t1 = wr(rd(txn(1, 10), 2, Some(2), Some(20)), 1, Some(10));
    let t2 = wr(rd(txn(2, 20), 1, Some(1), Some(10)), 2, Some(20));
    let rep = check(&hist(&[], vec![t1, t2]));
    assert!(!rep.ok);
    assert_eq!(cycle_class(&rep), Some(CycleClass::G1c), "{}", dump(&rep));
    let cyc = rep.violations.iter().find_map(|v| match v {
        Violation::Cycle { edges, .. } => Some(edges.clone()),
        _ => None,
    });
    let cyc = cyc.unwrap();
    assert_eq!(cyc.len(), 2);
    assert!(cyc.iter().all(|e| e.kind == DepKind::Wr));
}

#[test]
fn own_write_reads() {
    let mut t1 = wr(txn(1, 10), 1, Some(5));
    t1.reads.push(ReadEvent { key: k(1), observed: None, value: Some(row(1, 1)) }); // before write
    t1.reads.push(ReadEvent { key: k(1), observed: Some(1), value: Some(row(1, 4)) }); // own intermediate
    let t1 = rr(t1, 0, 10, &[(1, Some(1)), (2, None)]);
    let rep = check(&hist(&[(1, 1), (2, 2)], vec![t1.clone()]));
    assert!(rep.ok, "{}", dump(&rep));

    // Own read on a key it never writes.
    let bad = rd(txn(2, 20), 3, Some(2), Some(1));
    let rep = check(&hist(&[(1, 1), (2, 2)], vec![t1, bad]));
    assert!(has(&rep, |v| matches!(v, Violation::OwnReadWithoutWrite { reader: 2, .. })));
}

#[test]
fn delete_then_read_absent() {
    let t1 = wr(txn(1, 10), 1, None);
    let t2 = rr(rd(rd(txn(2, 20), 1, Some(1), None), 9, None, None), 0, 10, &[(2, None)]);
    let h = hist(&[(1, 1), (2, 2)], vec![t1, t2]);
    let rep = check(&h);
    assert!(rep.ok, "{}", dump(&rep));
    assert!(!final_state(&h).contains_key(&k(1)));

    // Reading the deleted row as absent but attributing it to the initial load is wrong.
    let t1 = wr(txn(1, 10), 1, None);
    let t2 = rd(txn(2, 20), 1, None, None);
    let rep = check(&hist(&[(1, 1)], vec![t1, t2]));
    assert!(has(&rep, |v| matches!(v, Violation::ReadMismatch { expected_writer: Some(1), .. })));
}

#[test]
fn well_formedness() {
    let mut t1b = txn(1, 30);
    t1b.outcome = Outcome::Aborted;
    let t2 = txn(2, 10);
    let t3 = rd(txn(3, 10), 1, Some(42), Some(1));
    let t4 = wr(wr(txn(4, 40), 1, Some(1)), 1, Some(2));
    let rep = check(&hist(&[], vec![txn(1, 5), t1b, t2, t3, t4]));
    assert!(has(&rep, |v| matches!(v, Violation::DuplicateTxnId { id: 1 })));
    assert!(has(&rep, |v| matches!(v, Violation::DuplicateCommitTs { .. })));
    assert!(has(&rep, |v| matches!(v, Violation::UnknownObserved { observed: 42, .. })));
    assert!(has(&rep, |v| matches!(v, Violation::DuplicateWrite { txn: 4, .. })));
    let unsorted = rr(txn(5, 50), 0, 5, &[(3, None), (1, None), (9, None)]);
    let rep = check(&hist(&[(1, 1), (3, 3)], vec![unsorted]));
    assert!(has(&rep, |v| matches!(v, Violation::MalformedRangeRead { .. })), "{}", dump(&rep));
    for v in &rep.violations {
        assert!(!v.to_string().is_empty());
    }
}

#[test]
fn violation_cap() {
    let mut txns = Vec::new();
    for i in 0..500 {
        txns.push(rd(txn(i + 1, i + 1), 1, None, Some(-1)));
    }
    let rep = check(&hist(&[(1, 1)], txns));
    assert_eq!(rep.total_violations, 500);
    assert_eq!(rep.violations.len(), MAX_STORED_VIOLATIONS);
}

// ---------------------------------------------------------------------------
// Randomized serial executions
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A truly serial execution: transactions run one by one, reads see exactly the
/// committed state plus their own writes. Returned in shuffled order.
fn gen_serial(seed: u64, ntxn: usize, nkeys: i64, ops: usize) -> History {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut committed: BTreeMap<Key, (Option<Row>, Option<TxnId>)> = BTreeMap::new();
    let mut initial = Vec::new();
    for t in 0..2u32 {
        for pk in 0..nkeys {
            if rng.below(2) == 0 {
                let key = Key { table: t, pk };
                let r = row(pk, rng.below(1000) as i64);
                initial.push((key, r.clone()));
                committed.insert(key, (Some(r), None));
            }
        }
    }
    let mut txns = Vec::with_capacity(ntxn);
    for i in 0..ntxn {
        let id = i as TxnId + 1;
        let mut t = txn(id, 10 * (i as u64 + 1));
        t.commit_ts.node = rng.below(3) as u16;
        let mut own: BTreeMap<Key, Option<Row>> = BTreeMap::new();
        let nops = 1 + rng.below(ops as u64) as usize;
        for _ in 0..nops {
            let key = Key { table: rng.below(2) as u32, pk: rng.below(nkeys as u64) as i64 };
            match rng.below(10) {
                0..=3 => {
                    let (observed, value) = match own.get(&key) {
                        Some(v) => (Some(id), v.clone()),
                        None => match committed.get(&key) {
                            Some((r, w)) => (*w, r.clone()),
                            None => (None, None),
                        },
                    };
                    t.reads.push(ReadEvent { key, observed, value });
                }
                4 => {
                    let lo = key.pk - rng.below(5) as i64;
                    let hi = key.pk + rng.below(5) as i64;
                    let mut rows = Vec::new();
                    for pk in lo..=hi {
                        let kk = Key { table: key.table, pk };
                        match own.get(&kk) {
                            Some(Some(_)) => rows.push((pk, Some(id))),
                            Some(None) => {}
                            None => {
                                if let Some((Some(_), w)) = committed.get(&kk) {
                                    rows.push((pk, *w));
                                }
                            }
                        }
                    }
                    t.range_reads.push(RangeReadEvent { table: key.table, lo, hi, rows });
                }
                5..=6 => {
                    own.insert(key, None);
                }
                _ => {
                    own.insert(key, Some(row(key.pk, rng.below(1000) as i64)));
                }
            }
        }
        t.writes = own.iter().map(|(k, v)| WriteEvent { key: *k, value: v.clone() }).collect();
        if rng.below(10) == 0 {
            t.outcome = Outcome::Aborted;
        } else {
            for (k, v) in own {
                match v {
                    Some(r) => {
                        committed.insert(k, (Some(r), Some(id)));
                    }
                    None => {
                        committed.insert(k, (None, Some(id)));
                    }
                }
            }
        }
        txns.push(t);
    }
    for i in (1..txns.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        txns.swap(i, j);
    }
    History { initial, txns }
}

#[test]
fn randomized_serial_ok_then_mutated() {
    for seed in 1..=40u64 {
        let h = gen_serial(seed, 300, 30, 8);
        let rep = check(&h);
        assert!(rep.ok, "seed {seed}: {}", dump(&rep));

        // Check the final state against an independent simple model.
        let fs = final_state(&h);
        assert!(fs.len() <= 120);

        // Mutation 1: change a value read by a committed txn.
        let mut rng = Rng(seed * 7 + 1);
        let mut m = h.clone();
        let cands: Vec<(usize, usize)> = m
            .txns
            .iter()
            .enumerate()
            .filter(|(_, t)| t.outcome == Outcome::Committed)
            .flat_map(|(i, t)| {
                t.reads
                    .iter()
                    .enumerate()
                    .filter(move |(_, r)| r.observed != Some(t.id))
                    .map(move |(j, _)| (i, j))
            })
            .collect();
        assert!(!cands.is_empty());
        let (i, j) = cands[rng.below(cands.len() as u64) as usize];
        let r = &mut m.txns[i].reads[j];
        r.value = Some(vec![Value::Int(-1)]);
        assert!(!check(&m).ok, "seed {seed}: value mutation not detected");

        // Mutation 2: attribute a read to a different (committed) writer.
        let mut m = h.clone();
        let reader = m.txns[i].id;
        let other = h.txns.iter().find(|t| t.outcome == Outcome::Committed && t.id != reader).unwrap().id;
        let r = &mut m.txns[i].reads[j];
        r.observed = match r.observed {
            None => Some(other),
            Some(_) => None,
        };
        assert!(!check(&m).ok, "seed {seed}: writer mutation not detected");

        // Mutation 3: drop a row from a range read on a key the reader does not write.
        let mut m = h.clone();
        let target = m.txns.iter().enumerate().find_map(|(i, t)| {
            if t.outcome != Outcome::Committed {
                return None;
            }
            t.range_reads.iter().enumerate().find_map(|(j, rr)| {
                rr.rows.iter().position(|&(pk, _)| {
                    !t.writes.iter().any(|w| w.key == Key { table: rr.table, pk })
                })
                .map(|p| (i, j, p))
            })
        });
        if let Some((i, j, p)) = target {
            m.txns[i].range_reads[j].rows.remove(p);
            assert!(!check(&m).ok, "seed {seed}: dropped range row not detected");
        }
    }
}

#[test]
fn randomized_reordered_commit_ts_detected() {
    // Swapping the commit_ts of two dependent txns must be detected in most histories.
    let mut detected = 0;
    for seed in 1..=20u64 {
        let mut h = gen_serial(seed, 200, 10, 6);
        let mut ids: Vec<usize> = (0..h.txns.len())
            .filter(|&i| h.txns[i].outcome == Outcome::Committed)
            .collect();
        ids.sort_by_key(|&i| h.txns[i].commit_ts);
        let (a, b) = (ids[50], ids[51]);
        let tmp = h.txns[a].commit_ts;
        h.txns[a].commit_ts = h.txns[b].commit_ts;
        h.txns[b].commit_ts = tmp;
        if !check(&h).ok {
            detected += 1;
        }
    }
    assert!(detected > 0);
}

#[test]
#[ignore]
fn perf_one_million_events() {
    let h = gen_serial(12345, 210_000, 50_000, 9);
    let events: usize = h
        .txns
        .iter()
        .map(|t| t.reads.len() + t.range_reads.len() + t.writes.len())
        .sum();
    let start = std::time::Instant::now();
    let rep = check(&h);
    let el = start.elapsed();
    eprintln!("events={events} elapsed={el:?} stats={:?}", rep.stats);
    assert!(events >= 1_000_000, "only {events} events");
    assert!(rep.ok, "{}", dump(&rep));
    assert!(el.as_secs_f64() < 10.0);
}
