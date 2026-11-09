// -*- mode: Bluespec; -*-

// Seeded bugs: the runner with one fault each, which `cargo xtask spec` must find. Each must
// fail the property named beside it with a counterexample; if one passes, the gate fails, as the
// properties no longer catch the bug they exist for.

/// Expiry frees the step but leaves the lease live, so a second worker gets the same step:
/// fails `oneLiveLeasePerStep`.
module keepLeaseOnExpiry {
  import runnerTypes.* from "./runner"
  import runner(
    RUNS = Set("r1"),
    WORKERS = Set("w1", "w2"),
    GRAPHS = Map(1 -> [{ maxBounces: 0, loop: None }]),
    FIRST_VERSION = 1,
    MAX_FAULTS = 1,
    MAX_ANSWERS = 0,
    FAULT = KeepLeaseOnExpiry,
  ).* from "./runner"
}

/// A result counts when its lease's step and phase are the run's, live or not, so a worker that
/// comes back late has its result applied after its replacement's: fails `resultAppliedOnce`.
module applyStaleResult {
  import runnerTypes.* from "./runner"
  import runner(
    RUNS = Set("r1"),
    WORKERS = Set("w1", "w2"),
    GRAPHS = Map(1 -> [{ maxBounces: 1, loop: None }]),
    FIRST_VERSION = 1,
    MAX_FAULTS = 1,
    MAX_ANSWERS = 0,
    FAULT = ApplyStaleResult,
  ).* from "./runner"
}
