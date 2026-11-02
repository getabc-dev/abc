//! abc's domain rules. Pure: no IO, no clock and no randomness of its own. The store passes time
//! in as a [`time::Timestamp`] and randomness in through [`decision::Mint`].
//!
//! Every request, read or write, takes the same path, and for writes all of it runs inside one
//! store transaction:
//!
//! 1. the store authenticates an unverified `Caller` into a [`actor::Principal`] and loads the
//!    request's `Facts` (plain records);
//! 2. [`policy::authorize`], the one permission check, verifies any bearer capability the request
//!    carries ([`policy::Request::prove`]), consults the single policy matrix, and returns an
//!    [`policy::Authorized`]. It is the only way to obtain one;
//! 3. for a command, [`command::Command::decide`] turns `Authorized<C>` into a
//!    [`decision::Decision`]. Building one requires the `Authorized`, and the builder stamps the
//!    authorized actor and workspace on every event;
//! 4. the store appends the decision's events and updates views from them. Its append accepts only
//!    a `Decision`, so code that skips step 2 does not compile (D10).

pub mod actor;
pub mod command;
pub mod credential;
pub mod decision;
pub mod event;
pub mod id;
pub mod identity;
pub mod policy;
pub mod query;
pub mod secret;
pub mod time;
