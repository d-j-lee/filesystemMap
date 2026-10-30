//! The agent harness (decision D-15): a verification gate and Claude Code hooks that put
//! the compiler, the tests and fsmap's own repository knowledge into an agent's loop.
//!
//! The hooks are guardrails against an agent's shortcuts, not a security boundary: the
//! agent can edit what the hooks read. CI, running the same gate, is the gate of record.
//! Guide: docs/agent-harness.md.

pub mod hooks;
pub mod integrity;
pub mod proc;
pub mod shell;
pub mod state;
pub mod verify;
