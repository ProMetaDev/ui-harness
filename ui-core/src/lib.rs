//! UI kernel.
//!
//! Seven contracts, each enforced by one module:
//!
//! 1. one step, one snapshot      — `prompt`
//! 2. one call, one effect set    — `tools` + `policy`
//! 3. one effect, one decision    — `policy`
//! 4. one budget, one account     — `budget`
//! 5. one side effect, one log    — `journal`
//! 6. one delegation, one mailbox — (session layer; not in this build)
//! 7. one error, one repair       — `agent::call_model`
//!
//! No dependencies, no plugin registry, no service mesh: the kernel is small
//! enough to read in an afternoon, which is the point.

pub mod agent;
pub mod budget;
pub mod cancel;
pub mod checkpoint;
pub mod compact;
pub mod exec;
pub mod fasthash;
pub mod isolate;
pub mod json;
pub mod journal;
pub mod knowledge;
pub mod mcp;
pub mod memory;
pub mod mode;
pub mod model;
pub mod observe;
pub mod plugin;
pub mod policy;
pub mod prompt;
pub mod protected;
pub mod sandbox;
pub mod secrets;
pub mod sha256;
pub mod skill;
pub mod spill;
pub mod time;
pub mod stream_verify;
pub mod tools;
pub mod winsandbox;
pub mod workflow;

pub use agent::{
    resume, Agent, AgentError, ApprovalRequest, ApprovalResponse, ApprovalScope, ApprovalTicket,
    Hooks, NullHooks, ResumeReport, TurnReport,
};
pub use budget::{estimate_tokens, Budget, BudgetExceeded, Dim, Limits};
pub use cancel::Cancel;
pub use compact::{compact, CompactOutcome, CompactPolicy, Summarizer};
pub use json::Json;
pub use journal::{analyze_incomplete, Journal};
pub use mode::Mode;
pub use model::{ExternalModel, Model, ModelResponse, ScriptedModel, ToolCall};
pub use policy::{Decision, Effect, Policy, Rule};
pub use prompt::{build_snapshot, diff_snapshots, inspect_environment, Message, Snapshot};
pub use skill::{Catalog, Skill};
pub use tools::{registry, Ctx, Tool, ToolResult};
pub use workflow::{Node, Task, Workflow};
