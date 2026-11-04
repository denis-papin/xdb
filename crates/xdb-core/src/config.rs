//! Cluster and execution configuration.

use xdb_ir::TableId;

/// Network and disk model of the simulated LAN. All times in nanoseconds.
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// One-way latency between two nodes (half a round trip).
    pub one_way_ns: u64,
    /// Mean of the exponential jitter added to every message.
    pub jitter_mean_ns: u64,
    /// Latency of a message that stays on the same node.
    pub local_ns: u64,
    /// fsync latency of one replica.
    pub fsync_ns: u64,
}

impl Default for NetConfig {
    fn default() -> Self {
        NetConfig {
            one_way_ns: 25_000,
            jitter_mean_ns: 3_000,
            local_ns: 1_000,
            fsync_ns: 30_000,
        }
    }
}

/// CPU model. Each shard leader is one core that serves micro-instructions one at a
/// time, highest priority first.
#[derive(Clone, Debug)]
pub struct CpuConfig {
    /// Service time of one micro-instruction on a shard core.
    pub mi_ns: u64,
    /// Extra service time per row returned by a scan.
    pub scan_row_ns: u64,
    /// Fixed coordinator overhead at the start of each call (plan cache lookup, ...).
    pub call_overhead_ns: u64,
}

impl Default for CpuConfig {
    fn default() -> Self {
        CpuConfig {
            mi_ns: 2_000,
            scan_row_ns: 200,
            call_overhead_ns: 5_000,
        }
    }
}

/// Which execution techniques the coordinator uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecMode {
    /// Writes do not wait for their result before the next statement (CockroachDB
    /// write pipelining). Proven.
    pub pipelining: bool,
    /// Out-of-order issue of later independent data statements while the current one
    /// waits (the MI idea). Experiment.
    pub lookahead: bool,
    /// Lookahead also passes `IF cond THEN RAISE ... END IF` guards before `cond` is
    /// known (predicted not to raise). Experiment.
    pub speculation: bool,
    /// Parallel commit (one consensus round) instead of classic two-phase commit.
    /// Proven.
    pub parallel_commit: bool,
    /// One-phase commit when all of a block's unsent writes and intents are on one
    /// shard, and refresh only the shards actually read. Proven (CockroachDB 1PC,
    /// refresh spans).
    pub fast_path: bool,
    /// Commutative updates (escrow, O'Neil 1986): `UPDATE t SET c = c + x` places a
    /// delta that does not conflict with other deltas on the same row. Experiment.
    pub escrow: bool,
    /// Implicit `SELECT ... FOR UPDATE`: a read of a row that the same block later
    /// updates or deletes takes the write intent at once. Proven (CockroachDB).
    pub implicit_sfu: bool,
    /// Run each call on the node that leads the shard of its first key, instead of
    /// the node the client picked. Proven (leaseholder routing).
    pub routing: bool,
    /// Read-only procedures read at a closed timestamp (`now - follower_read_lag`)
    /// from a replica on the caller's node when there is one. Proven (Spanner stale
    /// reads, CockroachDB follower reads).
    pub follower_reads: bool,
}

impl ExecMode {
    const fn base(pipelining: bool, lookahead: bool, speculation: bool, parallel_commit: bool) -> Self {
        ExecMode {
            pipelining,
            lookahead,
            speculation,
            parallel_commit,
            fast_path: false,
            escrow: false,
            implicit_sfu: false,
            routing: false,
            follower_reads: false,
        }
    }
    /// One statement at a time, classic 2PC.
    pub fn naive() -> Self {
        Self::base(false, false, false, false)
    }
    /// Proven techniques only: pipelined writes, batching, parallel commit.
    pub fn v1() -> Self {
        Self::base(true, false, false, true)
    }
    /// v1 + out-of-order issue of independent statements.
    pub fn parallel() -> Self {
        Self::base(true, true, false, true)
    }
    /// parallel + speculation past guards.
    pub fn speculative() -> Self {
        Self::base(true, true, true, true)
    }
    pub fn with_fast_path(mut self) -> Self {
        self.fast_path = true;
        self
    }
    pub fn with_escrow(mut self) -> Self {
        self.escrow = true;
        self
    }
    pub fn with_sfu(mut self) -> Self {
        self.implicit_sfu = true;
        self
    }
    pub fn with_routing(mut self) -> Self {
        self.routing = true;
        self
    }
    pub fn with_follower_reads(mut self) -> Self {
        self.follower_reads = true;
        self
    }
    /// The best mode of the previous study: speculative + 1PC + escrow.
    pub fn best() -> Self {
        Self::speculative().with_fast_path().with_escrow()
    }
    pub fn name(&self) -> String {
        let base = match (self.pipelining, self.lookahead, self.speculation, self.parallel_commit) {
            (false, false, false, false) => "naive",
            (true, false, false, true) => "v1",
            (true, true, false, true) => "parallel",
            (true, true, true, true) => "speculative",
            _ => "custom",
        };
        let mut s = base.to_string();
        for (on, tag) in [
            (self.fast_path, "+1pc"),
            (self.escrow, "+escrow"),
            (self.implicit_sfu, "+sfu"),
            (self.routing, "+route"),
            (self.follower_reads, "+follower"),
        ] {
            if on {
                s.push_str(tag);
            }
        }
        s
    }
    pub fn all() -> [ExecMode; 4] {
        [Self::naive(), Self::v1(), Self::parallel(), Self::speculative()]
    }
    /// Base modes plus every improvement studied after the first POC.
    pub fn extended() -> Vec<ExecMode> {
        let mut v: Vec<ExecMode> = Self::all().to_vec();
        for m in [Self::v1(), Self::speculative()] {
            v.push(m.with_fast_path());
            v.push(m.with_escrow());
            v.push(m.with_fast_path().with_escrow());
        }
        v.push(Self::v1().with_sfu());
        v.push(Self::v1().with_routing());
        v.push(Self::v1().with_follower_reads());
        v.push(Self::best().with_sfu().with_routing().with_follower_reads());
        v
    }
}

/// Wound-wait (older wounds younger) or wait-die (younger dies) — §11.4 of the doc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictRule {
    WoundWait,
    WaitDie,
}

#[derive(Clone, Debug)]
pub struct ClusterConfig {
    /// Number of shards (1..=5 in the reference cluster). Each shard leader is on its
    /// own node; there are `shards` nodes.
    pub shards: usize,
    /// Replicas per shard (leader included). Commit waits for a majority.
    pub replicas: usize,
    pub net: NetConfig,
    pub cpu: CpuConfig,
    pub mode: ExecMode,
    pub rule: ConflictRule,
    pub seed: u64,
    /// Range sharding: for each table, the primary-key range `[lo, hi]` that is split
    /// evenly over the shards. Keys outside go to the first / last shard.
    pub table_ranges: Vec<(TableId, i64, i64)>,
    /// Range used for tables not listed in `table_ranges`.
    pub default_range: (i64, i64),
    /// Max instructions the lookahead window scans past the blocked one.
    pub lookahead_window: usize,
    /// Mean of the randomized backoff before restarting a block after a conflict
    /// abort; doubles with each restart of the same block (capped at x64).
    /// Without it, wait-die livelocks (older restarted MIs starve a younger holder).
    pub restart_backoff_ns: u64,
    /// Follower reads use timestamp `now - follower_read_lag_ns`; writes are pushed
    /// above it (the closed timestamp).
    pub follower_read_lag_ns: u64,
    /// Fault injection for testing the tests: writes ignore the read-timestamp cache
    /// (breaks serializability on purpose). Never set outside tests.
    #[doc(hidden)]
    pub fault_ignore_read_ts: bool,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        ClusterConfig {
            shards: 3,
            replicas: 3,
            net: NetConfig::default(),
            cpu: CpuConfig::default(),
            mode: ExecMode::v1(),
            rule: ConflictRule::WoundWait,
            seed: 1,
            table_ranges: Vec::new(),
            default_range: (0, 1_000_000),
            lookahead_window: 64,
            restart_backoff_ns: 20_000,
            follower_read_lag_ns: 2_000_000,
            fault_ignore_read_ts: false,
        }
    }
}
