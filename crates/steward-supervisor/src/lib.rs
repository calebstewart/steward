//! steward's supervision logic, with no Windows in it: each service's state
//! machine ([`machine`]) and the restart policy it follows ([`policy`]). The
//! manager feeds it what happened and carries out what it asks for.

pub mod machine;
pub mod policy;

pub use machine::{Action, Event, Machine, Process, State};
pub use policy::Outcome;
