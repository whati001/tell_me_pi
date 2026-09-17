//! Client side of the omp RPC protocol (`omp --mode rpc`).

pub mod frame;
pub mod process;

pub use process::{HostToolCall, HostToolFn, OmpProcess, PROCESS_EXIT_EVENT, Progress, SpawnSpec, ToolOutcome};
