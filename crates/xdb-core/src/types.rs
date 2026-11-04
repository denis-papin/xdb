//! Types shared by shards and the coordinator.

use std::collections::{BTreeSet, HashMap};
use xdb_ir::{ColId, Expr, Key, Row, TableId, Ts, TxnId};

pub type ShardId = usize;
pub type NodeId = usize;
pub type SessionId = usize;

/// Transaction priority: lower = older = wins conflicts. `class` dominates (0 system,
/// 1 interactive, 2 batch), then the start timestamp of the first attempt of the block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Priority {
    pub class: u8,
    pub ts: Ts,
}

/// Smallest timestamp strictly greater than `t` on the shard side (node 0).
/// Coordinators always pick their final timestamp from their HLC above it.
pub fn bumped(t: Ts) -> Ts {
    Ts { wall: t.wall, logical: t.logical + 1, node: 0 }
}

#[derive(Clone, Debug)]
pub struct TxnMeta {
    pub id: TxnId,
    pub prio: Priority,
    pub read_ts: Ts,
    pub write_ts: Ts,
    pub node: NodeId,
    pub sid: SessionId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    /// Parallel commit in progress (or 2PC prepared): can no longer be wounded.
    Staging,
    Committed(Ts),
    Aborted,
}

#[derive(Clone, Debug)]
pub struct TxnState {
    pub status: Status,
    pub prio: Priority,
    pub node: NodeId,
    pub sid: SessionId,
    pub read_ts: Ts,
}

/// The replicated transaction records, modelled as one global table (the latency of
/// reaching a record is charged by the messages that change it, not by lookups).
#[derive(Default)]
pub struct Global {
    pub txns: HashMap<TxnId, TxnState>,
    /// Active (running / staging) transactions by read timestamp.
    pub active: BTreeSet<(Ts, TxnId)>,
}

impl Global {
    pub fn status(&self, t: TxnId) -> Status {
        self.txns.get(&t).map(|s| s.status).unwrap_or(Status::Aborted)
    }
    pub fn begin(&mut self, id: TxnId, st: TxnState) {
        self.active.insert((st.read_ts, id));
        self.txns.insert(id, st);
    }
    pub fn set(&mut self, id: TxnId, status: Status) {
        if let Some(s) = self.txns.get_mut(&id) {
            if matches!(status, Status::Committed(_) | Status::Aborted) {
                self.active.remove(&(s.read_ts, id));
            }
            s.status = status;
        }
    }
    pub fn set_read_ts(&mut self, id: TxnId, ts: Ts) {
        if let Some(s) = self.txns.get_mut(&id) {
            if self.active.remove(&(s.read_ts, id)) {
                self.active.insert((ts, id));
            }
            s.read_ts = ts;
        }
    }
    pub fn min_active_read_ts(&self) -> Option<Ts> {
        self.active.iter().next().map(|(t, _)| *t)
    }
}

/// A read span to re-validate at commit when the commit timestamp moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Span {
    Point(Key),
    Range(TableId, i64, i64),
}

/// A micro-instruction as executed by a shard.
#[derive(Clone, Debug)]
pub enum ShardOp {
    Get { key: Key, for_update: bool },
    Update { key: Key, sets: Vec<(ColId, Expr)> },
    Insert { key: Key, row: Row },
    Delete { key: Key },
    /// Range read of `[lo, hi]` of `table` restricted to this shard. `filter` uses
    /// `Col(_, c)` for the row's columns; `limit` counts rows passing the filter.
    Scan { table: TableId, lo: i64, hi: i64, filter: Option<Expr>, limit: Option<u64>, for_update: bool },
    /// Escrow: add these amounts to integer columns (commutative update).
    Delta { key: Key, adds: Vec<(ColId, i64)> },
    /// One-phase commit, after the same transaction's writes in this batch.
    Commit1PC,
}

#[derive(Clone, Debug)]
pub enum MiResult {
    Read { value: Option<Row>, writer: Option<TxnId> },
    Write { new_value: Option<Row>, applied: bool, prev: Option<Row>, prev_writer: Option<TxnId> },
    Scan { rows: Vec<Row>, seen: Vec<(i64, Option<TxnId>)>, hi_eff: i64 },
    /// A delta was placed (`applied` = the row exists).
    Delta { applied: bool },
    /// Answer to `Commit1PC` when it could not commit (fall back to the normal path).
    OnePc { ok: bool },
    Err(i32),
    Aborted,
}

#[derive(Clone, Debug)]
pub enum ToShard {
    Batch { txn: TxnMeta, mis: Vec<(u32, ShardOp)> },
    Refresh { txn: TxnId, sid: SessionId, node: NodeId, spans: Vec<Span>, from: Ts, to: Ts },
    Stage { txn: TxnId, sid: SessionId, node: NodeId },
    Prepare { txn: TxnId, sid: SessionId, node: NodeId },
    CommitRecord { txn: TxnId, sid: SessionId, node: NodeId },
    Resolve { txn: TxnId, commit_ts: Ts },
    Abort { txn: TxnId },
    RollbackTo { txn: TxnId, from_seq: u32, to_seq: u32 },
    /// Sent by a shard to itself once a one-phase commit batch is replicated.
    Finish1PC { txn: TxnId, ts: Ts, sid: SessionId, node: NodeId },
}

#[derive(Clone, Debug)]
pub enum ToCoord {
    MiResp { sid: SessionId, txn: TxnId, seq: u32, shard: ShardId, result: MiResult, push_above: Option<Ts>, durable: bool },
    Durable { sid: SessionId, txn: TxnId, seq: u32 },
    RefreshResp { sid: SessionId, txn: TxnId, ok: bool },
    StageResp { sid: SessionId, txn: TxnId, ok: bool },
    PrepareResp { sid: SessionId, txn: TxnId, ok: bool },
    CommitRecordResp { sid: SessionId, txn: TxnId },
    Wounded { sid: SessionId, txn: TxnId },
    OnePcResp { sid: SessionId, txn: TxnId, ts: Ts },
}

#[derive(Clone, Debug)]
pub enum Msg {
    S(ToShard),
    C(ToCoord),
}

/// Message endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Ep {
    Coord(NodeId),
    Shard(ShardId),
    /// The replica of a shard on a given node, serving follower reads.
    Follower(ShardId, NodeId),
}

/// A message produced by a shard, sent after `extra_ns` (+ network latency).
#[derive(Debug)]
pub struct Out {
    pub to: Ep,
    pub msg: Msg,
    pub extra_ns: u64,
    /// Add one replication delay (majority fsync) before sending.
    pub after_replication: bool,
}

/// Internal: a CHECK bound failed on a row version newer than the transaction's
/// snapshot. If the transaction read anything, its reads are stale and it restarts
/// (a serial execution would have seen that version); otherwise it is a normal
/// CHECK violation.
pub const CHECK_STALE: i32 = -22_900;
