//! The agent loop for mu: turns, compaction, plan mode and its store, the review flow, the tool
//! policy and the system prompt. Front ends (the TUI, `-p`, ACP, headless subagents) drive an
//! `agent::Agent`; the process-level commands live in the binary.

pub mod agent;
pub mod plans;
pub mod policy;
pub mod prompt;
pub mod review;

/// The scripted-agent builder shared by this crate's tests and the binary's headless tests.
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
