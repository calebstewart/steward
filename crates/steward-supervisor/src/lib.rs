//! steward's supervision logic, with no Windows in it: each service's state
//! machine ([`machine`]), the restart policy it follows ([`policy`]), the
//! order services start and stop in ([`plan`]), and when timers elapse
//! ([`timer`]). The manager feeds it what happened and carries out what it
//! asks for.

pub mod machine;
pub mod plan;
pub mod policy;
pub mod timer;

pub use machine::{Action, Event, Machine, Process, State};
pub use plan::{Decision, Plan, Progress};
pub use policy::Outcome;
pub use timer::{Moments, Schedule};
