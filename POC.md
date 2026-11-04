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

### Improvements study (section 7 of `bench/results.md`)

Modes compared:
- `v1`: the proven baseline.
- `+1pc`: one-phase commit for single-shard blocks, plus refresh only of the shards actually read. Proven.
- `+escrow`: commutative updates. Experiment.

Escrow turns `UPDATE t SET c = c + x` (or `c - x`, with `x` known when the statement runs) into a **delta**. Deltas on the same row from different transactions do not conflict with each other. They still conflict with reads and normal writes, like intents, and are applied in commit-timestamp order. When a delta commits below one already applied, the later values are recomputed; no reader can have seen them, because readers wait for pending deltas.

All 155 benchmark runs and the extended test suite (now run in all 10 modes) are serializable. A dedicated test hammers one counter with 48 clients: the final value is exact, and no reader ever sees half of a transaction.

| calls/s (3 shards, 64 clients) | v1 | v1+1pc | v1+escrow | v1+1pc+escrow | speculative | speculative+1pc+escrow |
|---|---|---|---|---|---|---|
| bank, 10,000 accounts (low contention) | 249k | 250k | 250k | 250k | 250k | 251k |
| bank, 100 accounts | 91k | 92k | 157k | 161k | 83k | **183k** |
| bank, 10 accounts (hot spot) | 13.2k | 13.7k | 23.8k | 24.8k | 13.0k | **41.6k** |
| TPC-C lite, 1 warehouse | 10.7k | 10.5k | 30.4k | 31.4k | 11.3k | **40.1k** |
| TPC-C lite, 4 warehouses | 41.5k | 40.9k | 85.3k | 89.4k | 43.1k | **114.5k** |

11. **One-phase commit is a latency fix, not a throughput fix.**
    - A single-shard block (deposit) drops from 203 to 148 µs (−27%). Multi-shard calls (transfer, payment) are unchanged.
    - Under load, throughput is unchanged. Those clusters are limited by shard CPU, and most bank and TPC-C transactions touch several shards.
    - The 1-shard bank run is CPU-saturated in every mode (≈121k calls/s), so it can't show a difference.
    - Keep it: it is cheap and proven, and real applications designed for co-location (§6.1) have many single-shard blocks.
12. **Escrow is the biggest win of the whole POC on contended data.**
    - TPC-C with 1 warehouse goes from 10.7k to 30.4k calls/s (×2.8). The hot `ytd` counters stop serializing payments.
    - Bank at 100 accounts: ×1.7. At 10 accounts: ×1.8.
    - No effect without contention, as expected.
    - Cost: the tail gets worse on the extreme hot spot (bank, 10 accounts: p99 7.1 → 10.7 ms). Transfers read a balance before updating it, and those reads must wait for the pending deltas of more concurrent writers.
13. **The three ideas add up.**
    - Speculation shortens the time intents are held, escrow removes counter conflicts, and 1PC trims single-shard commits.
    - Together they give the best result on every contended workload: ×3.1 to ×3.8 throughput (bank hot spot 13.2k → 41.6k; TPC-C 1 warehouse 10.7k → 40.1k) and the best p99 (bank hot spot 3.98 ms vs 7.1 ms).
    - Speculation alone helps little under contention, but combined with escrow it more than doubles escrow's gain on the hot spot (23.8k → 41.6k).

### Improvements round 2 (section 8 of `bench/results.md`)

Three more proven techniques:
- **`+sfu` (implicit `SELECT ... FOR UPDATE`, CockroachDB):** a read of a row that the same block later updates takes the write intent at once. The compiler pass skips it when the later update is an escrow delta (see finding 14).
- **`+route` (leaseholder routing):** a call runs on the node that leads most of its keys. Keys computed from the parameters (such as composite keys) count; read-only calls go to a node holding a follower replica.
- **`+follower` (follower reads at a closed timestamp, Spanner / CockroachDB):**
  - Read-only procedures read at `now − 2 ms`, on a follower replica on their own node when there is one.
  - New writes are pushed above that closed timestamp.
  - A follower read waits for pending writes at or below its timestamp.

| calls/s (3 shards) | v1 | v1+sfu | **proven core** v1+1pc+sfu+route+follower | best (speculative+1pc+escrow) | **best + sfu+route+follower** |
|---|---|---|---|---|---|
| bank, 10,000 accounts | 249k | 250k | 261k | 251k | 262k |
| bank, 100 accounts | 91k | 117k | 134k | 183k | **205k** |
| bank, 10 accounts | 13.2k | 16.8k | 19.4k | 41.6k | **47.8k** |
| bank read-heavy (80% point reads, 128 clients) | 681k | 681k | 1,130k | 681k | **1,131k** |
| TPC-C lite, 1 warehouse | 10.7k | 10.7k | 13.1k | 40.1k | **43.3k** |
| TPC-C lite, 4 warehouses | 41.5k | 41.5k | 54.6k | 114.5k | **123.6k** |

| idle latency, client on the "wrong" node | v1 | best | best + route |
|---|---|---|---|
| deposit (1 shard) | 203 µs | 148 µs | **94 µs** |
| transfer (2 shards + ledger) | 264 µs | 207 µs | **150 µs** |

14. **Implicit `FOR UPDATE` is a big win for the proven core, and it conflicts with escrow.**
    - On v1, bank at 100 accounts goes from 91k to 117k calls/s (+30%), and aborts per commit fall from 0.71 to 0.13. Hot spot: +27% throughput, p99 7.1 → 5.1 ms.
    - The first version also locked rows that escrow would update as deltas. That blocked the other transactions' deltas and **cut hot-spot throughput by 39%** (41.6k → 25.3k).
    - The fix: no implicit lock when the later update is delta-shaped and escrow is on. With that, it's neutral on top of escrow.
    - TPC-C is unchanged: new_order already uses `FOR UPDATE`, and payment doesn't read before writing.
15. **Routing cuts latency by 27–37%, and helps contended loads.**
    - Under load: hot spot +15%; TPC-C 4 warehouses +8% on the best mode, and +31% within the proven core.
    - Pitfall found: routing by the *first* key made payment slower (150 → 206 µs), because its other keys are computed from a local variable. Voting over all keys, after evaluating simple assignments, fixed it.
16. **Follower reads move read load off the leaders.**
    - Read-heavy mix: +66% throughput (681k → 1.13M calls/s); p50 75 → 12.5 µs (9.2 µs with routing), because the read is served by the local replica.
    - The price: read-only procedures see data 2 ms old. They still see a consistent snapshot: every concurrent audit sees exactly the constant total.
    - Pitfall found: routing first sent read-only calls to the leader's node, losing the local follower (p50 69 µs). Read-only calls now route to a follower's node.
17. **The proven core is much stronger than v1.**
    - v1 + one-phase commit + implicit `FOR UPDATE` + routing + follower reads, all proven elsewhere, gives +48% on bank 100, +47% on the hot spot, +66% read-heavy and +31% on TPC-C 4 warehouses, at no correctness cost.
    - The experiments (escrow, speculation) still add the most on hot counters: TPC-C with 1 warehouse goes from 13.1k (proven core) to 43.3k (everything).

### Bounded escrow (section 9 of `bench/results.md`)

Point 8 of the improvement list: **O'Neil escrow with bounds**.

**The SQL side.** Tables can declare `CHECK (col >= n)` bounds (also `>`, `<=`, `<`). Every write that would break one fails with ORA-02290 (code −2290).

**The shard side.** With escrow on, a debit is a delta, and the shard accepts it only if the bound holds **in the worst case** over all pending deltas on that row:
- `base + Σ pending debits + my delta ≥ min` → accept;
- `base + Σ pending credits + my delta < min` → refuse, the bound breaks even in the best case;
- otherwise → wait. Under wound-wait, an older debit wounds the younger pending deltas it depends on; a younger one waits.

Any subset of pending deltas can then commit, in any order, without breaking the bound.

**The workload.** `transfer_escrow` debits without reading the balance and relies on `CHECK (balance >= 0)`. The guarded `transfer` reads the balance, checks it in an `IF`, then debits.

**Two bugs found by the new tests, both fixed.**
- **Overdraft, under wait-die.** A debit that could only be covered by *other transactions' pending credits* was accepted without waiting for them. If they abort, the balance goes negative (−45 in the test). The shard now waits on every pending delta whose outcome matters.
- **Wrong error code.** The guarded `transfer` passed its `IF` on its snapshot, then the CHECK refused the debit because of transactions that committed after that snapshot. A serial execution would have raised the `IF`'s error instead. Now a CHECK that fails on data newer than the snapshot of a transaction that has read something restarts it with a fresh snapshot. With speculation, reads still in flight count too.

**Test.** A dedicated test runs every mode and both conflict rules on accounts starting at 50, with amounts up to 40: no balance ever goes below 0, money is conserved, and every history is serializable.

| transfers/s, 64 clients | guarded `transfer`, v1 | `transfer_escrow`, v1 | guarded, best (spec+1pc+escrow) | `transfer_escrow`, best | `transfer_escrow`, everything |
|---|---|---|---|---|---|
| 100 accounts, balance 1000 | 77k | 97k | 163k | **249k** | 250k |
| 10 accounts, balance 1000 (hot spot) | 11.1k | 14.0k | 34.0k | **164k** | 168k |
| 100 accounts, balance 100 (near the bound) | 65k | 72k | 85k | **124k** | 131k |
| 10 accounts, balance 100 | 9.1k | 11.2k | 22.8k | **49.5k** | 53.9k |

18. **Bounded escrow is the biggest single gain of the POC.**
    - On the hot spot with rich accounts: 34k → 164k transfers/s (×4.8), p99 4.2 → 1.4 ms, aborts per commit 4.4 → 0.14.
    - Overall, the guarded `transfer` in v1 does 11.1k/s and `transfer_escrow` with every improvement does 168k/s: **×15** on the same hot spot.
19. **The bounds cost little when they don't bind, and still win when they do.**
    - With poor accounts (most debits near 0), many debits must wait for pending ones (≈76k escrow waits on 10 accounts), yet throughput is still ×2.2 better than the guarded transfer.
    - The share of refused transfers is about the same as with the guarded version, so the worst-case rule is not overly pessimistic here.
20. **Escrow-style code helps even without escrow.** In v1, `transfer_escrow` beats the guarded transfer by 26% (rich accounts, 100): it has no read, so no failed read re-checks at commit.
21. **The price is a programming model.**
    - The overdraft rule moves from procedural code (`IF v_bal < p_amt THEN RAISE`) to a declarative constraint, and the error becomes −2290 instead of the application's −20001.
    - Only bounds on single integer columns are supported, which covers balances, stock quantities and seat counts.
    - An unknown account is silently ignored (0 rows) instead of raising NO_DATA_FOUND.

**Recommendation.** Ship `+1pc`, `+sfu`, `+route` and `+follower` in the proven core. Keep escrow and speculation as switchable experiments, but treat escrow as the first experiment to harden: it gives the largest measured gain on contended data.

Escrow limitations in the POC:
- Bounds are now supported through `CHECK` constraints; see "Bounded escrow" below.
- Only `c = c ± constant` is recognized.
- The final value of a delta is not returned to the statement. That's fine for `UPDATE`; a `RETURNING` clause would need the read.

## Road to product

The tests prove the **design** is sound. They do not prove that a **product** built on it would be. This is the order I would follow; each step keeps your three principles (Oracle-like behaviour, fast on a small LAN, proven techniques).

### Step 0 — keep the simulator as the main test tool

Build the real node so the same code runs either inside the simulator or on real sockets and disks. The core logic must not do its own I/O: it receives messages, clock ticks and disk completions, and returns messages and disk writes to send. FoundationDB is built this way and credits it for its reliability. Every protocol bug is then found in simulation, deterministically, with a seed you can replay.

### Step 1 — durability and failures, in simulation

- Replace the replication-latency model with real Raft (`raft-rs`, as TiKV uses), one group per shard.
- Inject faults at every step: node crash and restart, lost and duplicated messages, network partitions, a disk write lost before fsync, leader change in the middle of a commit.
- Add clock skew and the HLC uncertainty window (§10.6).
- Exit criterion: weeks of simulated time per night, zero checker violations, zero lost committed writes.

### Step 2 — stronger tests

- **Client-side checking.** Today the engine records its own history, so a bug in the recorder could hide a bug in the engine. Add workloads where clients alone can see every anomaly (Jepsen's Elle: append-only lists and read-your-writes registers), and check only what clients observed.
- **Mutation testing.** Inject dozens of deliberate protocol bugs, one at a time: skip a wound, drop a refresh span, resolve an intent at the wrong timestamp, lose a resolve message, reorder a channel. Each one must make a test fail. The POC has one such mutation.
- Close the known holes: protect the "row exists" read behind `DUP_VAL_ON_INDEX`, and make handler rollback statement-level.

### Step 3 — real nodes

- One process per node, in Docker containers on a LAN, with a binary wire protocol.
- Run Jepsen-style tests on real networks (partitions with `iptables`, killed processes, clock jumps).
- Measure real latency and compare it with the simulation model; recalibrate the model.

### Step 4 — product features

These are proven, but large:
- secondary indexes;
- joins and a real SQL optimizer;
- online schema change;
- MVCC garbage collection and "snapshot too old";
- backup and restore;
- rolling upgrades;
- authentication;
- observability.

Each comes with its own simulated tests.

### Design improvements found by the POC

| Improvement | Proven in | Status |
|---|---|---|
| Randomized restart backoff (wait-die livelock) | Every database that restarts on conflict | **Done** in the POC |
| One-phase commit when a block's writes all go to one shard | CockroachDB 1PC | **Done** (`+1pc`): single-shard block −27% latency; no throughput change (findings 11–13) |
| Refresh only the shards that were actually read | CockroachDB refresh spans | **Done**, part of `+1pc` |
| Stage the commit in parallel with the last writes | CockroachDB parallel commits | **Not cheap.** A staged transaction can no longer be wounded. If one of its in-flight writes then waits for an older transaction, the two wait for each other. CockroachDB avoids this with a "transaction recovery" protocol (others check whether all writes landed, and if not, prevent the missing one and abort). Step 1 material. |
| Implicit `SELECT ... FOR UPDATE` (not for escrow deltas) | CockroachDB | **Done** (`+sfu`): +30% on contended bank (finding 14) |
| Route each call to the node leading most of its keys | Leaseholder routing (CockroachDB, Spanner) | **Done** (`+route`): −27 to −37% latency (finding 15) |
| Follower reads at a closed timestamp for read-only procedures | Spanner stale reads, CockroachDB follower reads | **Done** (`+follower`): +66% on read-heavy load (finding 16) |
| Per-key wait queues instead of re-polling parked operations | CockroachDB lock table | Step 1 material |
| Commutative updates (escrow, O'Neil 1986) for hot counters | Research, not in mainstream SQL engines | **Experiment** (`+escrow`): ×2–3 throughput on contended workloads, serializable in every test (findings 11–13) |
| Escrow with bounds: `CHECK (col >= n)` guaranteed on worst-case pending deltas | O'Neil 1986 (research) | **Experiment**, done: up to ×4.8 on hot-spot transfers written escrow-style, never an overdraft (findings 18–21) |

## What is not proven by this POC

- Behaviour on real machines and a real network (compromise 1). The next step would be one process per node in Docker containers, with real Raft (`raft-rs`), on the same tests.
- Failover and recovery (compromise 2).
- Clock skew (compromise 4).
