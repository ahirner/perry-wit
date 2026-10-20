//! Unstructured control flow with block arguments.
//!
//! Adapted from `pliron-llvm` (Apache-2.0, The pliron contributors).
pliron_control::control_flow_ops!(
    "perry.br",
    "perry.cond_br",
    "perry.return",
    crate::scalar::is_bool,
    "a signless i1"
);
