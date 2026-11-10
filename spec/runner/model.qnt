// -*- mode: Bluespec; -*-

// The models `cargo xtask spec` checks: the runner of runner.qnt with small constants. No one model
// with every behaviour fits CI time, so each model keeps the behaviours its concern needs and TLC
// explores every state of each. docs/spec/runner.md says what each covers.

/// Leases, crashes and late workers: two workers race for a one-step graph's work while the
/// server crashes and workers go offline.
module leases {
  import runnerTypes.* from "./runner"
  import runner(
    RUNS = Set("r1"),
    WORKERS = Set("w1", "w2"),
    GRAPHS = Map(1 -> [{ maxBounces: 0, loop: None }]),
    FIRST_VERSION = 1,
    MAX_FAULTS = 2,
    MAX_ANSWERS = 0,
    FAULT = NoFault,
  ).* from "./runner"
}

/// The graph: bounces, a back edge, escalation and its answer, and a new version published
/// while the run is under way, with no faults, which `leases` covers.
module flow {
  import runnerTypes.* from "./runner"
  import runner(
    RUNS = Set("r1"),
    WORKERS = Set("w1"),
    GRAPHS = Map(
      1 -> [
        { maxBounces: 1, loop: None },
        { maxBounces: 0, loop: Some({ target: 0, bound: 1 }) },
      ],
      2 -> [{ maxBounces: 0, loop: None }],
    ),
    FIRST_VERSION = 1,
    MAX_FAULTS = 0,
    MAX_ANSWERS = 1,
    FAULT = NoFault,
  ).* from "./runner"
}

/// Two runs share one worker, which the server gives one step at a time.
module contention {
  import runnerTypes.* from "./runner"
  import runner(
    RUNS = Set("r1", "r2"),
    WORKERS = Set("w1"),
    GRAPHS = Map(1 -> [{ maxBounces: 0, loop: None }]),
    FIRST_VERSION = 1,
    MAX_FAULTS = 1,
    MAX_ANSWERS = 0,
    FAULT = NoFault,
  ).* from "./runner"
}
