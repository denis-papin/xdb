# XDB proof of concept

This POC tests the idea in [docs/research-mi-distributed-sql.html](docs/research-mi-distributed-sql.html): run PL/SQL-like procedures as micro-instructions (MIs) over a sharded, replicated store, with conflicts settled by transaction priority (wound-wait) instead of a lock manager.

It answers two questions:

1. **Is it correct?** Are transactions serializable, under every execution mode, with concurrency, conflicts, restarts, savepoints and multi-block routines?
2. **Is it fast on a LAN?** How do the proven v1 techniques and the MI experiment (out-of-order issue, speculation) compare with one-statement-at-a-time execution?

## Layout

| Crate | Role |
|---|---|
| `crates/xdb-ir` | Shared contract: data model, IR of procedures, transaction history format |
| `crates/xdb-compiler` | PL/SQL subset → IR (hand-written lexer and parser) |
| `crates/xdb-core` | The engine, inside a deterministic discrete-event simulation of the cluster |
| `crates/xdb-checker` | Independent serializability checker (timestamp replay + Adya dependency graph) |
| `crates/xdb-bench` | Benchmark harness, writes `bench/results.md` and `bench/results.csv` |
| `workloads/*.sql` | Bank, batch (blocks with progress table, §7.3), TPC-C lite |

### What `xdb-core` implements (design doc § in brackets)

- MVCC records with one write intent per record, and a read-timestamp cache per key and per range [5.2]
- Wound-wait, or wait-die, on a fixed priority `(class, first-attempt timestamp)` [5.1, 5.3, 5.5, 11.4]
- Timestamp pushes and read refresh at commit [5.4]
- Thread-per-core shard leader: one MI queue per shard, highest priority first; per-transaction ordering of MIs on a shard [4.1]
- Majority replication (latency model), parallel commit, and classic 2PC for the naive mode [6.2, 6.3]
- Transaction blocks: `COMMIT`/`ROLLBACK` inside routines, automatic block retry from the last commit, statement-level savepoints for exception handlers, cursors across commit with Oracle semantics (`FOR UPDATE` → ORA-01002) [7]
- Four execution modes:
  - `naive`: one statement at a time, 2PC
  - `v1`: write pipelining, per-shard batching, parallel commit — **proven techniques only**
  - `parallel`: v1 + out-of-order issue of independent statements (the MI idea)
  - `speculative`: parallel + passing `IF c THEN RAISE` guards before `c` is known
- The same-key check of §3.4: reordered statements on the same key keep program order

## How to run

Inside WSL (or any Linux with Rust ≥ 1.80):

```bash
cargo test --release                      # all correctness tests
cargo run --release -p xdb-bench          # benchmarks -> bench/results.md
```

With Docker:

```bash
docker build -t xdb-poc .
docker run --rm -v "$PWD/bench:/out" xdb-poc --out /out
```

## How correctness is checked

- Every concurrent run records a full history: reads with the version observed, range reads, final writes, and commit timestamps. `xdb-checker` verifies it independently in two ways:
  - it replays committed transactions in commit-timestamp order and compares every read;
  - it builds the Adya dependency graph and looks for G0/G1c/G2 cycles.
- Invariants: money is conserved in the bank workload. Every concurrent `audit_total` over all accounts sees exactly the constant total. TPC-C warehouse YTD equals the sum of its district YTDs, and order ids per district match `next_o_id`.
- **The checks are not vacuous.** With the read-timestamp check deliberately disabled (`fault_ignore_read_ts`), the checker reports violations in 5 of 5 runs (test `checker_catches_a_deliberately_broken_engine`).
- Coverage: 4 modes × 2 conflict rules × 1–5 shards × 2–4 replicas × 12 seeds (fuzz test), plus hot spots with 64 clients on 4 rows, batch blocks running alongside OLTP, and exception savepoints.

## Compromises

I made these choices to test the concept in days, not months. Each one says what it hides.

| # | Compromise | Why | What it hides |
|---|---|---|---|
| 1 | **Simulation, not real machines.** One process, simulated time; network, fsync and CPU are parameters (defaults: 25 µs one-way, 30 µs fsync, 2 µs per MI). | Reproducible runs and exact interleavings. Thousands of runs in seconds, and no implementation overhead mixed into the protocol's cost. | Real network stacks, serialization costs, OS scheduling. The numbers are a **model**, to compare modes with each other, not absolute performance. |
| 2 | **Replication is a latency, not a Raft log.** A write is durable after the majority-th fastest replica answers. | Replication itself is a proven component (§6.2); the POC tests what is new. | Leader failover, log catch-up, crash recovery: not exercised. |
| 3 | **Transaction records are one global table.** Status lookups are free; changes are charged through messages. | Simpler code. | Lookup latency when a reader meets an unresolved intent: optimistic by about one RTT in that case. |
| 4 | **Perfect clocks.** All nodes share the simulated clock. | No HLC uncertainty restarts to model. | External-consistency cost under clock skew (§10.6). |
| 5 | **Data model.** One integer primary key per table, `NUMBER` = 64-bit integer, no secondary indexes, no joins, scans by key range only. | Enough for the procedural workloads. | Index maintenance (a second MI per write, often on another shard, §10.8). |
| 6 | **PL/SQL subset.** No record-field assignment, no RETURN, no collections or dynamic SQL; WHERE only on the primary key. See the compiler crate for the full list. | Scope. | Real application code will hit unsupported constructs. |
| 7 | **Coordinator CPU** is a fixed 5 µs per call; each shard is one core. | Shard CPU is the contended resource; the coordinator is not. | Coordinator saturation at very high call rates. |
| 8 | **Handler savepoint** rolls back the whole inner block, not only the failing statement. | Simpler. | Oracle keeps the inner block's earlier statements. The IR documents this. |
| 9 | **No MVCC garbage collection.** | Runs are short. | Memory growth and "snapshot too old" for long cursors. |

## Findings

Full tables: [bench/results.md](bench/results.md) (101 runs, all serializable, all invariants hold). Times are simulated µs.

1. **Correctness holds.** No serializability violation in any tested configuration, and the injected bug was always caught.
2. **A real bug the design did not foresee: wait-die livelock.** Under heavy contention, older transactions that "die" and restart immediately keep their old priority. The shard serves them first, so the younger transaction they all indirectly wait for never gets CPU. That is the priority inversion of §5.6, in practice. **A randomized restart backoff fixes it**, as real systems do; it is now in the design (`restart_backoff_ns`, 20 µs mean, doubling per restart).
3. **Idle latency of `transfer`** (3 shards, 3 replicas): naive ≈ 522 µs, v1 ≈ 263 µs, speculative ≈ 206 µs. The doc estimated 430 / 200 / 150 µs. The model is higher because commit waits for the write results before staging (no overlap of staging with the last writes).
4. **Out-of-order issue alone does nothing for `transfer`.** Its first statement is a read guarded by an `IF`, so nothing can move ahead without speculation. It does help TPC-C `new_order`, whose item reads are independent: idle 530 → 360 µs.
5. **Proven techniques give most of the gain, as the doc predicted.**

   | | idle `transfer` | idle `new_order` | bank throughput, 5 shards |
   |---|---|---|---|
   | naive | 522 µs | 811 µs | 146k calls/s |
   | v1 (proven) | 265 µs | 530 µs | 269k calls/s |
   | MI experiment | 207 µs (speculative) | 360 µs (parallel) | 312k calls/s (speculative) |

6. **Shards scale; replicas don't change much.**
   - With low contention, throughput grows from 121k calls/s (1 shard, CPU-bound) to 312k calls/s (5 shards).
   - 2, 3 or 4 replicas give nearly the same numbers: commit waits for the majority-th fastest ack, and on a LAN those are close. Fault tolerance is the reason to choose 3 (§6.2), not speed.
7. **The MI experiment pays more when the network is slower.** Speculative vs v1 on idle `transfer`: −17% at 5 µs one-way, −22% at 25 µs, −23% at 50 µs. At 50 µs, bank throughput is +21%.
8. **Wound-wait vs wait-die under contention.**
   - Wound-wait has better tail latency.
   - Wait-die restarts much more (up to 24 restarts for one call vs 9) but, on hot spots, gets more throughput (10 accounts: 19.6k vs 13.0k calls/s, speculative).
   - Speculation cuts restarts under wound-wait (aborts per commit at 10 accounts: 3.8 → 1.4), because a transaction holds its intents for a shorter time.
   - Wound-wait stays the default (§5); wait-die is a tuning option for hot-spot workloads.
9. **One hot row dominates everything.** TPC-C with 1 warehouse: every payment updates the same warehouse row, which caps throughput at about 11k calls/s whatever the mode.
   - Naive gets a lower new_order p50 (1.2 ms vs 5.0 ms) only because it starves payments (p50 23.5 ms vs 7.4 ms), at half the throughput.
   - With 4 warehouses everything scales ×4. This is the hot-row limit of §10.2: the fix is schema design, not the engine.
10. **Batch next to OLTP works with small blocks** (§5.5, §7.4).
    - A low-priority batch committing every 10 or 100 rows leaves transfer p99 unchanged (≈ 275–283 µs vs 276 µs without batch).
    - With blocks of 1000 rows, transfer p99 rises to 397 µs, and the batch is restarted about 120 times.
    - This confirms the §7.4 guidance of 100–10,000 rows per block, leaning to the low end when OLTP shares the data.

## What is not proven by this POC

- Behaviour on real machines and a real network (compromise 1). The next step would be one process per node in Docker containers, with real Raft (`raft-rs`), on the same tests.
- Failover and recovery (compromise 2).
- Clock skew (compromise 4).
