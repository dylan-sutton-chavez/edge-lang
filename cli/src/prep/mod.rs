// what a preparation step may see and touch and how much of it
pub mod caps;
// the cpu memory and disk a box has as any user of it reads them
pub mod capacity;
// a program run as another user with nothing of the caller in reach
pub mod run;
// files read written and walked only below one real directory
pub mod scope;
// a step's outputs copied where its user cannot write and hashed
pub mod seal;
// every process of a user ended once its step returns
pub mod sweep;

/* the unprivileged user and group a step runs as */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
}
