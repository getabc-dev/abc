// @vitest-environment jsdom

// The program runner: messages go through `update`, effects through `perform`, and nothing runs
// after the screen goes.

import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useEffect } from "react";
import { afterEach, expect, test, vi } from "vitest";

import { after, type Dispatch, type Program, settle, useProgram } from "./elm";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

type Message = { readonly type: "add"; readonly by: number } | { readonly type: "echo" };
type Effect = { readonly type: "echo_later" };

/** A counter whose `echo` message asks for an effect that adds 10, and which starts with one. */
function counter(log: string[]): Program<number, Message, Effect> {
  return {
    init: () => [0, [{ type: "echo_later" }]],
    update: (model, message) => {
      switch (message.type) {
        case "add":
          return [model + message.by, []];
        case "echo":
          return [model, [{ type: "echo_later" }]];
      }
    },
    perform: (effect, dispatch, signal) => {
      log.push(effect.type);
      after(10, { type: "add", by: 10 }, dispatch, signal);
    },
  };
}

function Counter({
  program,
  onDispatch,
}: {
  readonly program: Program<number, Message, Effect>;
  readonly onDispatch?: (dispatch: Dispatch<Message>) => void;
}) {
  const [model, dispatch] = useProgram(program);
  useEffect(() => {
    onDispatch?.(dispatch);
  }, [dispatch, onDispatch]);
  return (
    <>
      <output aria-label="count">{model}</output>
      <button type="button" onClick={() => dispatch({ type: "add", by: 2 })}>
        Add two
      </button>
      <button type="button" onClick={() => dispatch({ type: "echo" })}>
        Echo
      </button>
    </>
  );
}

function count(): string | null {
  return screen.getByRole("status", { name: "count" }).textContent;
}

test("runs the first effects, then each message through update and its effects", async () => {
  vi.useFakeTimers();
  const log: string[] = [];
  render(<Counter program={counter(log)} />);
  expect(count()).toBe("0");
  expect(log).toEqual(["echo_later"]);
  await act(async () => {
    await vi.advanceTimersByTimeAsync(10);
  });
  expect(count()).toBe("10");
  fireEvent.click(screen.getByRole("button", { name: "Add two" }));
  expect(count()).toBe("12");
  fireEvent.click(screen.getByRole("button", { name: "Echo" }));
  expect(log).toEqual(["echo_later", "echo_later"]);
  await act(async () => {
    await vi.advanceTimersByTimeAsync(10);
  });
  expect(count()).toBe("22");
});

test("aborts running effects and drops messages once unmounted", async () => {
  vi.useFakeTimers();
  const program = counter([]);
  const update = vi.spyOn(program, "update");
  const dispatches: Dispatch<Message>[] = [];
  const view = render(
    <Counter
      program={program}
      onDispatch={(dispatch) => {
        dispatches.push(dispatch);
      }}
    />,
  );
  view.unmount();
  await vi.advanceTimersByTimeAsync(10);
  for (const dispatch of dispatches) {
    dispatch({ type: "add", by: 1 });
  }
  expect(dispatches.length).toBeGreaterThan(0);
  expect(update).not.toHaveBeenCalled();
});

test("reads the program once, on the first render", () => {
  const first: string[] = [];
  const second: string[] = [];
  const view = render(<Counter program={counter(first)} />);
  view.rerender(<Counter program={counter(second)} />);
  expect(first).toEqual(["echo_later"]);
  expect(second).toEqual([]);
});

test("settle dispatches the message for a value or for a failure", async () => {
  const seen: string[] = [];
  const dispatch = (message: string) => {
    seen.push(message);
  };
  await settle(
    Promise.resolve(2),
    (value) => `done ${value}`,
    () => "failed",
    dispatch,
  );
  await settle(
    Promise.reject(new Error("no")),
    () => "done",
    (error) => `failed ${error instanceof Error ? error.message : ""}`,
    dispatch,
  );
  expect(seen).toEqual(["done 2", "failed no"]);
});
