//! Commands that run a plan or review outside the agent loop: `deliver`, `review`, `plan`, `notify`, `take`;
//! and `migrate-profile`.

pub mod deliver;
pub mod migrate;
pub mod notify;
pub mod plans_cli;
pub mod review_cli;
pub mod take;
