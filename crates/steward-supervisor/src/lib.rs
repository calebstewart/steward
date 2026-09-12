//! steward's supervision logic, with no Windows in it: each service's state
//! machine ([`machine`]), the restart policy it follows ([`policy`]), and the
//! order services start and stop in ([`plan`]). The manager feeds it what
//! happened and carries out what it asks for.

pub mod machine;
pub mod plan;
pub mod policy;

pub use machine::{Action, Event, Machine, Process, State};
pub use plan::{Decision, Plan, Progress};
pub use policy::Outcome;
