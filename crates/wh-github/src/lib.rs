//! GitHub integration, local-only: everything goes through the `gh` and `git`
//! command lines plus polling (no webhooks, no open port).
//!
//! - [`ship`]: push the task branch and open / find its pull request.
//! - [`GithubService`]: issue intake and PR tracking (reviews, CI, merge).
//! - [`commands`]: `wh task ship`, `wh github check`.

pub mod cli;
pub mod commands;
pub mod poll;
pub mod service;
pub mod ship;

pub use cli::{CliError, Gh};
pub use service::{GithubService, SyncReport};
pub use ship::ship;
