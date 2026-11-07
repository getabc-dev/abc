// -*- mode: Bluespec; -*-

/// Scenarios of the runner, run by `quint test` in `cargo xtask spec`: each drives the runner
/// through one story from docs/spec/runner.md, step by step, and checks where it ends.
module tests {
  import runnerTypes.* from "./runner"
  import runner(
    RUNS = Set("r1"),
    WORKERS = Set("w1", "w2"),
    GRAPHS = Map(
      1 -> [
        { maxBounces: 1, loop: None },
        { maxBounces: 0, loop: Some({ target: 0, bound: 1 }) },
      ],
      2 -> [{ maxBounces: 0, loop: None }],
    ),
    FIRST_VERSION = 1,
    MAX_FAULTS = 3,
    MAX_ANSWERS = 2,
    FAULT = NoFault,
  ).* from "./runner"

  /// The run's state, as the running server holds it.
  def r1: Run = view.runs.get("r1")

  /// Worker `w` does lease `id`'s work in `phase` with `outcome`, and its result is applied.
  action work(w: WorkerId, id: LeaseId, phase: Phase, outcome: Outcome): bool =
    grant("r1", w)
      .then(take(w, { lease: id, phase: phase }))
      .then(finish(w, { lease: id, phase: phase }, outcome))
      .then(deliver(w, { lease: id, outcome: outcome }, false))

  /// The run passes both steps of graph 1 first time: maker then checker, twice.
  run passesEveryStepTest =
    init
      .then(startRun("r1"))
      .then(work("w1", 0, Maker, Made))
      .expect(r1.phase == Checker and r1.step == 0)
      .then(work("w2", 1, Checker, Pass))
      .expect(r1.phase == Maker and r1.step == 1)
      .then(work("w1", 2, Maker, Made))
      .then(work("w1", 3, Checker, Pass))
      .expect(and { r1.status == Done, r1.epoch == 4, liveLeases == Set(), safety })

  /// A checker's bounce sends the step back to its maker until the step's bound, then escalates.
  run bouncesUntilTheBoundThenEscalatesTest =
    init
      .then(startRun("r1"))
      .then(work("w1", 0, Maker, Made))
      .then(work("w1", 1, Checker, Bounce))
      .expect(r1.phase == Maker and r1.bounces == 1 and r1.status == Running)
      .then(work("w1", 2, Maker, Made))
      .then(work("w1", 3, Checker, Bounce))
      .expect(r1.status == AwaitingPerson and r1.bounces == 1 and safety)
      .then(grant("r1", "w1"))
      .fail()

  /// An escalation parks the run; the answer resumes it at the step it parked on.
  run anAnswerResumesTheParkedStepTest =
    init
      .then(startRun("r1"))
      .then(work("w1", 0, Maker, Made))
      .then(work("w1", 1, Checker, Pass))
      .then(work("w1", 2, Maker, Made))
      .then(work("w2", 3, Checker, Escalate))
      .expect(r1.status == AwaitingPerson and r1.step == 1)
      .then(answer("r1", Retry))
      .expect(and { r1.status == Running, r1.step == 1, r1.phase == Maker, r1.bounces == 0 })
      .then(work("w1", 4, Maker, Made))
      .then(work("w1", 5, Checker, Escalate))
      .then(answer("r1", Accept))
      .expect(r1.status == Done and budget.answers == 0)

  /// The back edge takes the run back once, as its bound allows, and then lets it go on.
  run aBackEdgeStopsAtItsBoundTest =
    init
      .then(startRun("r1"))
      .then(work("w1", 0, Maker, Made))
      .then(work("w1", 1, Checker, Pass))
      .then(work("w1", 2, Maker, Made))
      .then(work("w1", 3, Checker, Loop))
      .expect(r1.step == 0 and r1.loops.get(1) == 1)
      .then(work("w1", 4, Maker, Made))
      .then(work("w1", 5, Checker, Pass))
      .then(work("w1", 6, Maker, Made))
      .then(work("w1", 7, Checker, Loop))
      .expect(r1.status == Done and r1.loops.get(1) == 1 and safety)

  /// A worker that went offline and comes back after its lease expired reports late: the result
  /// is discarded, and the step's result is applied once.
  run aLateWorkersResultIsDiscardedTest =
    init
      .then(startRun("r1"))
      .then(grant("r1", "w1"))
      .then(take("w1", { lease: 0, phase: Maker }))
      .then(lose("w1"))
      .then(expire(0))
      .expect(r1.lease == None and liveLeases == Set())
      .then(work("w2", 1, Maker, Made))
      .expect(r1.phase == Checker and r1.epoch == 1)
      .then(back("w1"))
      .then(finish("w1", { lease: 0, phase: Maker }, Made))
      .then(deliver("w1", { lease: 0, outcome: Made }, false))
      .expect(and {
        log.length() == 5,
        r1.phase == Checker,
        r1.epoch == 1,
        ghost.applied.get(("r1", 0)) == 1,
        workers.get("w1").outbox == Set(),
        safety,
      })

  /// A result whose acknowledgement was lost is sent again and applied once.
  run aResentResultIsAppliedOnceTest =
    init
      .then(startRun("r1"))
      .then(grant("r1", "w1"))
      .then(take("w1", { lease: 0, phase: Maker }))
      .then(finish("w1", { lease: 0, phase: Maker }, Made))
      .then(deliver("w1", { lease: 0, outcome: Made }, true))
      .expect(workers.get("w1").outbox.size() == 1 and r1.epoch == 1)
      .then(deliver("w1", { lease: 0, outcome: Made }, false))
      .expect(and { r1.epoch == 1, workers.get("w1").outbox == Set(), resultAppliedOnce })

  /// A crash loses the server's memory; replaying the log rebuilds exactly what it held.
  run aCrashRecoversByReplayingTheLogTest =
    init
      .then(startRun("r1"))
      .then(work("w1", 0, Maker, Made))
      .then(grant("r1", "w2"))
      .then(all { crash, assert(view.runs.get("r1").lease == Some(1)) })
      .expect(not(up) and view == emptyState and workers.get("w2").offers == Set())
      .then(recover)
      .expect(and { up, view == replay(log), r1.lease == Some(1), r1.phase == Checker })
      .then(expire(1))
      .then(work("w2", 2, Checker, Pass))
      .expect(r1.step == 1 and safety)

  /// A run keeps the graph version it started on, through a newer version and a crash.
  run aRunKeepsItsGraphVersionTest =
    init
      .then(startRun("r1"))
      .then(publish(2))
      .then(crash)
      .then(recover)
      .expect(r1.version == 1 and published == 2)
      .then(work("w1", 0, Maker, Made))
      .then(work("w1", 1, Checker, Pass))
      .expect(r1.step == 1 and r1.status == Running and versionPinned)

  /// The server never leases a parked or finished run, nor to a worker that holds a live lease.
  run leasesGoOnlyWhereTheyMayTest =
    init
      .then(startRun("r1"))
      .then(grant("r1", "w1"))
      .then(grant("r1", "w2"))
      .fail()
}
