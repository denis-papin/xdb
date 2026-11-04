//! XDB core: a deterministic simulation of a sharded, replicated transactional store
//! that runs PL/SQL-subset procedures as micro-instructions (MIs), with
//! priority-based (wound-wait) concurrency control. See docs/research-mi-distributed-sql.html.
//!
//! Everything runs in one thread in simulated time: network latency, replication
//! (majority fsync) and shard CPU are modelled; results are reproducible from the seed.

pub mod code;
pub mod config;
pub mod eval;
pub mod rng;
pub mod shard;
pub mod sim;
pub mod types;

pub use config::{ClusterConfig, ConflictRule, CpuConfig, ExecMode, NetConfig};
pub use rng::Rng;
pub use sim::{Call, CallStat, RunReport, Sim, SimStats};
