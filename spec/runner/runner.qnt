// -*- mode: Bluespec; -*-

// The graph runner of abc (S1): how the server runs a graph, one step at a time, by leasing each
// step's work to a worker. docs/spec/runner.md says the same in plain words and names the
// definitions it explains; read the two together.
//
// The model has three parts:
//
// - the server: an append-only event log (`log`), the state the runner keeps in memory and
//   rebuilds from the log (`view`), whether it is up, and the graph version new runs pin;
// - the workers: online or not, the lease offers in flight to them, the work they hold, and their
//   outbox of results, which survives the worker going to sleep;
// - the environment's budgets, and the ghost variables that only the properties read.
//
// Every runner decision is an event. The server's state changes only by `applyEvent`, one event
// at a time, and recovery after a crash is `replay(log)`, so the state is always the log's replay.

/// The runner's types, apart from its parameters so that a model can name them when it sets
/// the parameters.
module runnerTypes {
  type RunId = str
  type WorkerId = str
  type LeaseId = int
  type Version = int

  type Option[a] = Some(a) | None

  /// A back edge of a graph: when its step passes and asks to loop, the run goes back to
  /// `target`, at most `bound` times in the run.
  type BackEdge = { target: int, bound: int }

  /// One step of a graph: how many times its checker may bounce the maker's work back before
  /// the step escalates to a person, and its back edge, if it has one.
  type StepDef = { maxBounces: int, loop: Option[BackEdge] }

  /// Who works on a step: first a maker, then a checker that judges the maker's work.
  type Phase = Maker | Checker

  type RunStatus =
    | Running
    // Parked on an escalation: the run waits for a person's answer.
    | AwaitingPerson
    | Done
    | Cancelled

  type Run = {
    // The graph version the run pinned when it started.
    version: Version,
    // The step the run is on, an index into its graph.
    step: int,
    phase: Phase,
    status: RunStatus,
    // How many results the run has applied: the fencing number of its next result.
    epoch: int,
    // How many times the checker bounced the current step back to its maker.
    bounces: int,
    // How many times the run took each step's back edge, by step.
    loops: int -> int,
    // The run's live lease, if one is out.
    lease: Option[LeaseId],
  }

  /// A lease on a run's step and phase, given to one worker.
  type Lease = {
    runId: RunId,
    worker: WorkerId,
    step: int,
    phase: Phase,
    epoch: int,
    live: bool,
  }

  /// What a worker reports for its lease: a maker's `Made`, or a checker's verdict.
  type Outcome =
    | Made
    | Pass
    // Pass, and take the step's back edge if its bound allows.
    | Loop
    | Bounce
    | Escalate

  /// A person's answer to an escalation.
  type Answer =
    // Run the step again from its maker, with the answer as guidance.
    | Retry
    // Accept the step's work as passed.
    | Accept
    | Cancel

  /// The events in the server's log. Each carries everything its effect depends on, so
  /// replaying the log needs nothing else.
  type Event =
    | RunStarted({ runId: RunId, version: Version })
    | LeaseGranted({ lease: LeaseId, runId: RunId, worker: WorkerId })
    | LeaseExpired(LeaseId)
    | ResultApplied({ lease: LeaseId, outcome: Outcome })
    | Answered({ runId: RunId, answer: Answer })

  /// The runner's state, as it keeps it in memory and as replaying the log rebuilds it.
  type State = { runs: RunId -> Run, leases: LeaseId -> Lease }

  /// A lease as a worker knows it: offered, held while it works, and named in its result.
  type Work = { lease: LeaseId, phase: Phase }

  type Result = { lease: LeaseId, outcome: Outcome }

  type Worker = {
    online: bool,
    // Leases the server sent that have not reached the worker yet.
    offers: Set[Work],
    // Work the worker has taken and not finished.
    holding: Set[Work],
    // Results not yet acknowledged by the server; kept on the worker's disk, so they survive
    // the worker going offline and are sent again until the server acknowledges them.
    outbox: Set[Result],
  }

  /// A seeded bug, for the gate's negative tests: each must fail a property with a
  /// counterexample. The checked model runs with `NoFault`.
  type Fault =
    | NoFault
    // Expiry frees the step but leaves its lease live.
    | KeepLeaseOnExpiry
    // A result is applied when its lease's step and phase are the run's, live or not.
    | ApplyStaleResult
}

/// The runner, for any runs, workers, graphs and environment budgets.
module runner {
  import runnerTypes.*

  // ---------------------------------------------------------------------------------------------
  // Parameters
  // ---------------------------------------------------------------------------------------------

  const RUNS: Set[RunId]
  const WORKERS: Set[WorkerId]
  /// The published graphs, by version: each a list of steps that ends after its last step.
  const GRAPHS: Version -> List[StepDef]
  /// The graph version published when the model starts.
  const FIRST_VERSION: Version
  /// How many faults the environment may cause in a behaviour: server crashes and workers
  /// going offline together. Finite, as any one run sees finitely many.
  const MAX_FAULTS: int
  /// How many escalations a person answers in a behaviour.
  const MAX_ANSWERS: int
  const FAULT: Fault

  // ---------------------------------------------------------------------------------------------
  // Variables
  // ---------------------------------------------------------------------------------------------

  /// The server's append-only event log, which survives a crash.
  var log: List[Event]
  /// The runner's in-memory state; lost on a crash and rebuilt by replaying `log`.
  var view: State
  /// Whether the server is running.
  var up: bool
  /// The graph version that runs started now pin.
  var published: Version
  var workers: WorkerId -> Worker
  /// The environment's remaining faults and answers.
  var budget: { faults: int, answers: int }
  /// Ghost variables, for the properties only: the version each run pinned when it started, and
  /// how many results were applied for each run and epoch.
  var ghost: { pinned: RunId -> Version, applied: (RunId, int) -> int }

  // ---------------------------------------------------------------------------------------------
  // The runner's state machine: applying one event
  // ---------------------------------------------------------------------------------------------

  pure val emptyState: State = { runs: Map(), leases: Map() }

  pure def getOr(m: a -> b, k: a, default: b): b =
    if (m.keys().contains(k)) m.get(k) else default

  pure def stepDef(cur: Run): StepDef = GRAPHS.get(cur.version)[cur.step]

  /// Where the run goes back to when its step passes: the target of the step's back edge, when
  /// `wantLoop` asks and the edge's bound allows.
  pure def backTarget(cur: Run, wantLoop: bool): Option[int] =
    match stepDef(cur).loop {
      | Some(edge) =>
          if (wantLoop and cur.loops.getOr(cur.step, 0) < edge.bound) Some(edge.target) else None
      | None => None
    }

  /// The run after its step passed: along the step's back edge when `wantLoop` asks and its
  /// bound allows, otherwise forward to the next step, or done after the last.
  pure def advance(cur: Run, wantLoop: bool): Run =
    match backTarget(cur, wantLoop) {
      | Some(target) => {
          ...cur,
          step: target,
          phase: Maker,
          bounces: 0,
          loops: cur.loops.put(cur.step, cur.loops.getOr(cur.step, 0) + 1),
        }
      | None =>
          if (cur.step + 1 == GRAPHS.get(cur.version).length())
            { ...cur, status: Done }
          else
            { ...cur, step: cur.step + 1, phase: Maker, bounces: 0 }
    }

  /// The run after its checker bounced the maker's work: back to the maker while the step's
  /// bound allows, and parked on an escalation once it does not.
  pure def bounce(cur: Run): Run =
    if (cur.bounces < stepDef(cur).maxBounces)
      { ...cur, phase: Maker, bounces: cur.bounces + 1 }
    else
      { ...cur, status: AwaitingPerson }

  /// The run after a worker's outcome for its current step and phase.
  pure def afterOutcome(cur: Run, outcome: Outcome): Run =
    match outcome {
      | Made => { ...cur, phase: Checker }
      | Pass => advance(cur, false)
      | Loop => advance(cur, true)
      | Bounce => bounce(cur)
      | Escalate => { ...cur, status: AwaitingPerson }
    }

  /// The run after a person's answer resumes it at the step it parked on.
  pure def afterAnswer(cur: Run, answer: Answer): Run =
    match answer {
      | Retry => { ...cur, status: Running, phase: Maker, bounces: 0 }
      | Accept => advance({ ...cur, status: Running }, false)
      | Cancel => { ...cur, status: Cancelled }
    }

  /// The state after one event: the runner's whole transition function.
  pure def applyEvent(s: State, event: Event): State =
    match event {
      | RunStarted(p) => {
          ...s,
          runs: s.runs.put(p.runId, {
            version: p.version,
            step: 0,
            phase: Maker,
            status: Running,
            epoch: 0,
            bounces: 0,
            loops: Map(),
            lease: None,
          }),
        }
      | LeaseGranted(p) =>
          val cur = s.runs.get(p.runId)
          {
            runs: s.runs.set(p.runId, { ...cur, lease: Some(p.lease) }),
            leases: s.leases.put(p.lease, {
              runId: p.runId,
              worker: p.worker,
              step: cur.step,
              phase: cur.phase,
              epoch: cur.epoch,
              live: true,
            }),
          }
      | LeaseExpired(id) =>
          val lease = s.leases.get(id)
          val cur = s.runs.get(lease.runId)
          {
            runs: s.runs.set(lease.runId, { ...cur, lease: None }),
            leases: s.leases.set(id, { ...lease, live: FAULT == KeepLeaseOnExpiry }),
          }
      | ResultApplied(p) =>
          val lease = s.leases.get(p.lease)
          val cur = s.runs.get(lease.runId)
          {
            runs: s.runs.set(
              lease.runId,
              afterOutcome({ ...cur, lease: None, epoch: cur.epoch + 1 }, p.outcome)
            ),
            leases: s.leases.set(p.lease, { ...lease, live: false }),
          }
      | Answered(p) => {
          ...s,
          runs: s.runs.set(p.runId, afterAnswer(s.runs.get(p.runId), p.answer)),
        }
    }

  /// The state the log rebuilds: what recovery after a crash computes.
  pure def replay(events: List[Event]): State = events.foldl(emptyState, (s, e) => applyEvent(s, e))

  /// Whether the runner applies `result`: its lease is live, so it is the run's current lease
  /// and its result was not applied before.
  pure def accepts(s: State, result: Result): bool = and {
    s.leases.keys().contains(result.lease),
    val lease = s.leases.get(result.lease)
    val cur = s.runs.get(lease.runId)
    match FAULT {
      | ApplyStaleResult => and {
          cur.status == Running,
          cur.step == lease.step,
          cur.phase == lease.phase,
        }
      | _ => lease.live
    },
  }

  /// The outcomes a worker may report for work in `phase`.
  pure def outcomes(phase: Phase): Set[Outcome] =
    match phase {
      | Maker => Set(Made)
      | Checker => Set(Pass, Loop, Bounce, Escalate)
    }

  // ---------------------------------------------------------------------------------------------
  // Views of the state that the actions and properties read
  // ---------------------------------------------------------------------------------------------

  /// The leases the runner holds live.
  def liveLeases: Set[LeaseId] = view.leases.keys().filter(id => view.leases.get(id).live)

  /// Online workers with no live lease: the server gives each one step at a time.
  def idleWorkers: Set[WorkerId] =
    WORKERS.filter(w => and {
      workers.get(w).online,
      liveLeases.forall(id => view.leases.get(id).worker != w),
    })

  /// Whether worker `w` is still working on, or reporting, lease `id`: it heartbeats a lease
  /// while it holds it or its result is in its outbox, and while the offer is in flight.
  def keepsAlive(w: WorkerId, id: LeaseId): bool =
    val worker = workers.get(w)
    and {
      worker.online,
      or {
        worker.offers.exists(o => o.lease == id),
        worker.holding.exists(h => h.lease == id),
        worker.outbox.exists(r => r.lease == id),
      },
    }

  /// Leases the server may expire: a run's current lease whose worker stopped heartbeating it,
  /// because the worker went offline or the offer was lost in a crash.
  def expirable: Set[LeaseId] =
    liveLeases.filter(id =>
      val lease = view.leases.get(id)
      and {
        view.runs.get(lease.runId).lease == Some(id),
        not(keepsAlive(lease.worker, id)),
      }
    )

  // ---------------------------------------------------------------------------------------------
  // Actions
  // ---------------------------------------------------------------------------------------------

  /// The server records `e` and applies it, in one transaction.
  action record(e: Event): bool = all {
    log' = log.append(e),
    view' = applyEvent(view, e),
  }

  /// A person starts run `r`; it pins the published graph version.
  action startRun(r: RunId): bool = all {
    up,
    not(view.runs.keys().contains(r)),
    record(RunStarted({ runId: r, version: published })),
    ghost' = { ...ghost, pinned: ghost.pinned.put(r, published) },
    published' = published,
    workers' = workers,
    budget' = budget,
    up' = up,
  }

  /// Graph `version`, newer than the published one, is published. Runs already started keep
  /// theirs.
  action publish(version: Version): bool = all {
    GRAPHS.keys().contains(version),
    version > published,
    published' = version,
    log' = log,
    view' = view,
    up' = up,
    workers' = workers,
    budget' = budget,
    ghost' = ghost,
  }

  /// The server leases run `r`'s current step and phase to idle worker `w`, and sends `w` the
  /// offer.
  action grant(r: RunId, w: WorkerId): bool = all {
    up,
    view.runs.keys().contains(r),
    view.runs.get(r).status == Running,
    view.runs.get(r).lease == None,
    idleWorkers.contains(w),
    val id = view.leases.keys().size()
    val work = { lease: id, phase: view.runs.get(r).phase }
    all {
      record(LeaseGranted({ lease: id, runId: r, worker: w })),
      workers' = workers.setBy(w, worker => { ...worker, offers: worker.offers.union(Set(work)) }),
    },
    up' = up,
    published' = published,
    budget' = budget,
    ghost' = ghost,
  }

  /// The server expires lease `id`, whose worker stopped heartbeating it, freeing its step.
  action expire(id: LeaseId): bool = all {
    up,
    expirable.contains(id),
    record(LeaseExpired(id)),
    up' = up,
    published' = published,
    workers' = workers,
    budget' = budget,
    ghost' = ghost,
  }

  /// Worker `w`'s outbox sends `result`. The server applies it if its lease is live and discards
  /// it otherwise, and acknowledges it either way. With `ackLost`, a fault of the environment,
  /// the acknowledgement does not arrive: the result stays in the outbox and is sent again.
  action deliver(w: WorkerId, result: Result, ackLost: bool): bool = all {
    up,
    workers.get(w).online,
    workers.get(w).outbox.contains(result),
    not(ackLost) or budget.faults > 0,
    if (accepts(view, result)) all {
      record(ResultApplied(result)),
      val key = (view.leases.get(result.lease).runId, view.leases.get(result.lease).epoch)
      ghost' = { ...ghost, applied: ghost.applied.put(key, ghost.applied.getOr(key, 0) + 1) },
    } else all {
      log' = log,
      view' = view,
      ghost' = ghost,
    },
    if (ackLost) all {
      workers' = workers,
      budget' = { ...budget, faults: budget.faults - 1 },
    } else all {
      workers' = workers.setBy(w, worker => { ...worker, outbox: worker.outbox.exclude(Set(result)) }),
      budget' = budget,
    },
    up' = up,
    published' = published,
  }

  /// A person gives answer `a` to run `r`'s escalation, and the run resumes at the step it
  /// parked on.
  action answer(r: RunId, a: Answer): bool = all {
    up,
    budget.answers > 0,
    view.runs.keys().contains(r),
    view.runs.get(r).status == AwaitingPerson,
    record(Answered({ runId: r, answer: a })),
    budget' = { ...budget, answers: budget.answers - 1 },
    up' = up,
    published' = published,
    workers' = workers,
    ghost' = ghost,
  }

  /// The server crashes: it loses its memory, and with its connections the offers in flight.
  action crash: bool = all {
    up,
    budget.faults > 0,
    up' = false,
    view' = emptyState,
    workers' = workers.keys().mapBy(w => { ...workers.get(w), offers: Set() }),
    budget' = { ...budget, faults: budget.faults - 1 },
    log' = log,
    published' = published,
    ghost' = ghost,
  }

  /// The server restarts and rebuilds its state by replaying the log.
  action recover: bool = all {
    not(up),
    up' = true,
    view' = replay(log),
    log' = log,
    published' = published,
    workers' = workers,
    budget' = budget,
    ghost' = ghost,
  }

  /// The worker part of a step: the server's state is unchanged.
  action workerOnly(w: WorkerId, worker: Worker): bool = all {
    workers' = workers.set(w, worker),
    log' = log,
    view' = view,
    up' = up,
    published' = published,
    budget' = budget,
    ghost' = ghost,
  }

  /// Worker `w` receives the offer of `work` and starts it.
  action take(w: WorkerId, work: Work): bool = all {
    workers.get(w).online,
    workers.get(w).offers.contains(work),
    val worker = workers.get(w)
    workerOnly(w, {
      ...worker,
      offers: worker.offers.exclude(Set(work)),
      holding: worker.holding.union(Set(work)),
    }),
  }

  /// Worker `w` finishes `work` with `outcome` and puts the result in its outbox.
  action finish(w: WorkerId, work: Work, outcome: Outcome): bool = all {
    workers.get(w).online,
    workers.get(w).holding.contains(work),
    outcomes(work.phase).contains(outcome),
    val worker = workers.get(w)
    workerOnly(w, {
      ...worker,
      holding: worker.holding.exclude(Set(work)),
      outbox: worker.outbox.union(Set({ lease: work.lease, outcome: outcome })),
    }),
  }

  /// Worker `w` goes offline, as a laptop sleeps: offers in flight to it are lost, and it keeps
  /// its work and its outbox for when it comes back.
  action lose(w: WorkerId): bool = all {
    workers.get(w).online,
    budget.faults > 0,
    workers' = workers.setBy(w, worker => { ...worker, online: false, offers: Set() }),
    budget' = { ...budget, faults: budget.faults - 1 },
    log' = log,
    view' = view,
    up' = up,
    published' = published,
    ghost' = ghost,
  }

  /// Worker `w` comes back online, possibly long after its leases expired.
  action back(w: WorkerId): bool = all {
    not(workers.get(w).online),
    val worker = workers.get(w)
    workerOnly(w, { ...worker, online: true }),
  }

  action init = all {
    log' = List(),
    view' = emptyState,
    up' = true,
    published' = FIRST_VERSION,
    workers' = WORKERS.mapBy(_ => { online: true, offers: Set(), holding: Set(), outbox: Set() }),
    budget' = { faults: MAX_FAULTS, answers: MAX_ANSWERS },
    ghost' = { pinned: Map(), applied: Map() },
  }

  // Each action below makes one of the choices the actions above take as parameters. The model
  // checker tries every choice; a simulation picks one at random and records it in its trace.

  /// The server leases run `r`'s step to some idle worker.
  action grantSome(r: RunId): bool = {
    nondet w = WORKERS.oneOf()
    grant(r, w)
  }

  /// The server expires some lease it may expire.
  action expireSome: bool = {
    nondet id = view.leases.keys().oneOf()
    expire(id)
  }

  /// Worker `w`'s outbox sends some result, whose acknowledgement may be lost.
  action deliverSome(w: WorkerId): bool = {
    nondet result = workers.get(w).outbox.oneOf()
    nondet ackLost = Set(false, true).oneOf()
    deliver(w, result, ackLost)
  }

  /// Worker `w` takes some offer.
  action takeSome(w: WorkerId): bool = {
    nondet work = workers.get(w).offers.oneOf()
    take(w, work)
  }

  /// Worker `w` finishes some work with some outcome.
  action finishSome(w: WorkerId): bool = {
    nondet work = workers.get(w).holding.oneOf()
    nondet outcome = Set(Made, Pass, Loop, Bounce, Escalate).oneOf()
    finish(w, work, outcome)
  }

  /// A person gives some answer to run `r`'s escalation.
  action answerSome(r: RunId): bool = {
    nondet a = Set(Retry, Accept, Cancel).oneOf()
    answer(r, a)
  }

  /// Some newer graph version is published.
  action publishSome: bool = {
    nondet version = GRAPHS.keys().oneOf()
    publish(version)
  }

  action step = {
    nondet r = RUNS.oneOf()
    nondet w = WORKERS.oneOf()
    any {
      startRun(r),
      grantSome(r),
      answerSome(r),
      deliverSome(w),
      takeSome(w),
      finishSome(w),
      lose(w),
      back(w),
      publishSome,
      expireSome,
      crash,
      recover,
    }
  }

  // ---------------------------------------------------------------------------------------------
  // Properties
  // ---------------------------------------------------------------------------------------------

  /// At most one live lease per step: no two workers work on one step at once.
  val oneLiveLeasePerStep: bool =
    liveLeases.forall(a => liveLeases.forall(b =>
      val first = view.leases.get(a)
      val second = view.leases.get(b)
      a == b or first.runId != second.runId or first.step != second.step
    ))

  /// A step's result is applied at most once: for each run and epoch, one result, however
  /// late or often a worker reports.
  val resultAppliedOnce: bool = ghost.applied.keys().forall(key => ghost.applied.get(key) <= 1)

  /// A run never changes graph version: it keeps the version it pinned when it started.
  val versionPinned: bool =
    view.runs.keys().forall(r => view.runs.get(r).version == ghost.pinned.get(r))

  /// Bounces never pass their bound, nor back edges theirs.
  val boundsHold: bool =
    view.runs.keys().forall(r =>
      val cur = view.runs.get(r)
      val graph = GRAPHS.get(cur.version)
      and {
        cur.step >= 0,
        cur.step < graph.length(),
        cur.bounces <= stepDef(cur).maxBounces,
        cur.loops.keys().forall(s => match graph[s].loop {
          | Some(edge) => cur.loops.get(s) <= edge.bound
          | None => false
        }),
      }
    )

  /// Replaying the log rebuilds the same state as the running server holds.
  val replayRebuildsState: bool = up implies view == replay(log)

  /// A run that is not running holds no live lease, so a parked run takes no worker.
  val onlyRunningRunsHoldLeases: bool =
    liveLeases.forall(id => view.runs.get(view.leases.get(id).runId).status == Running)

  val safety: bool = and {
    oneLiveLeasePerStep,
    resultAppliedOnce,
    versionPinned,
    boundsHold,
    replayRebuildsState,
    onlyRunningRunsHoldLeases,
  }

  /// The state the durable log holds, whether the server is up or not.
  def durable: State = replay(log)

  def isRunning(r: RunId): bool =
    durable.runs.keys().contains(r) and durable.runs.get(r).status == Running

  def endsOrAsks(r: RunId): bool =
    durable.runs.keys().contains(r) and durable.runs.get(r).status != Running

  /// What the environment is assumed to do: the server comes back after a crash, workers come
  /// back online, and nothing that stays possible is put off forever. People are not assumed to
  /// answer, nor runs to start, nor graphs to be published.
  temporal fairness = and {
    weakFair(recover, (log, view, up, published, workers, budget, ghost)),
    weakFair(expireSome, (log, view, up, published, workers, budget, ghost)),
    RUNS.forall(r => weakFair(grantSome(r), (log, view, up, published, workers, budget, ghost))),
    WORKERS.forall(w => and {
      weakFair(takeSome(w), (log, view, up, published, workers, budget, ghost)),
      weakFair(finishSome(w), (log, view, up, published, workers, budget, ghost)),
      weakFair(deliverSome(w), (log, view, up, published, workers, budget, ghost)),
      weakFair(back(w), (log, view, up, published, workers, budget, ghost)),
    }),
  }

  /// Under fairness, every running run ends or waits on a person: no silent stall.
  temporal noSilentStall =
    fairness implies RUNS.forall(r => always(isRunning(r) implies eventually(endsOrAsks(r))))
}
