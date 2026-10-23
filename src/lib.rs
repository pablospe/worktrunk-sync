//! Library surface for the `wt-sync` binary and its parent-inference benches.
//! Not a stable API: the modules are public only so benches can reach them.

#[doc(hidden)]
pub mod parent_infer;
#[doc(hidden)]
pub mod sync;
