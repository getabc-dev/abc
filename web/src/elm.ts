// The Elm architecture (decision 36): a screen is a program of a model, one message union, a pure
// `update(model, message) -> [model, effects]`, and `perform`, which runs effects at the edge and
// feeds what they learn back as messages. `update` holds every rule and is tested without a DOM;
// `perform` only talks to the world.

import { useCallback, useEffect, useRef, useState } from "react";

/** What `update` returns: the next model and the effects to run. */
export type Step<Model, Effect> = readonly [Model, readonly Effect[]];

export type Dispatch<Message> = (message: Message) => void;

export type Program<Model, Message, Effect> = {
  /** The first model and the effects to start with. Pure. */
  readonly init: () => Step<Model, Effect>;
  /** The next model, and the effects to run, after `message`. Pure. */
  readonly update: (model: Model, message: Message) => Step<Model, Effect>;
  /**
   * Runs `effect`, dispatching what it learns. `signal` aborts when the screen goes, and an
   * effect that outlives it must stop then.
   */
  readonly perform: (effect: Effect, dispatch: Dispatch<Message>, signal: AbortSignal) => void;
};

/**
 * Runs `program` for as long as the calling component is mounted, and returns its model and its
 * dispatch. The program is read once, on the first render: a screen's program never changes.
 * Messages dispatched after unmount are dropped, and every running effect's signal aborts.
 */
export function useProgram<Model, Message, Effect>(
  program: Program<Model, Message, Effect>,
): readonly [Model, Dispatch<Message>] {
  const [first] = useState(() => program);
  const [model, setModel] = useState(() => first.init()[0]);
  const dispatchRef = useRef<Dispatch<Message>>(() => undefined);
  useEffect(() => {
    const { init, update, perform } = first;
    const controller = new AbortController();
    // The model state already holds init's model: init is pure, and runs once per mount here
    // only for its effects (React mounts twice in development's strict mode).
    const [start, effects] = init();
    let current = start;
    const dispatch: Dispatch<Message> = (message) => {
      if (controller.signal.aborted) {
        return;
      }
      const [next, more] = update(current, message);
      current = next;
      setModel(next);
      for (const effect of more) {
        perform(effect, dispatch, controller.signal);
      }
    };
    dispatchRef.current = dispatch;
    for (const effect of effects) {
      perform(effect, dispatch, controller.signal);
    }
    return () => {
      controller.abort();
    };
  }, [first]);
  const dispatch = useCallback<Dispatch<Message>>((message) => {
    dispatchRef.current(message);
  }, []);
  return [model, dispatch] as const;
}

/** Waits for `task` and dispatches `done`'s message for its value, or `failed`'s for its failure. */
export async function settle<T, Message>(
  task: Promise<T>,
  done: (value: T) => Message,
  failed: (error: unknown) => Message,
  dispatch: Dispatch<Message>,
): Promise<void> {
  let message: Message;
  try {
    message = done(await task);
  } catch (error: unknown) {
    message = failed(error);
  }
  dispatch(message);
}

/** Dispatches `message` after `ms` milliseconds, unless `signal` aborts first. */
export function after<Message>(
  ms: number,
  message: Message,
  dispatch: Dispatch<Message>,
  signal: AbortSignal,
): void {
  const timer = setTimeout(() => {
    dispatch(message);
  }, ms);
  signal.addEventListener("abort", () => {
    clearTimeout(timer);
  });
}
