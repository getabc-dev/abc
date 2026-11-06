import { describe, expect, test } from "vitest";

import { parseEvents, readEvents } from "./sse";

describe("parseEvents", () => {
  test("reads each complete event's name, data and id, and keeps the rest", () => {
    const { events, rest } = parseEvents(
      'event: snapshot\nid: 7\ndata: {"a":1}\n\nevent: machines\ndata: x',
    );
    expect(events).toEqual([{ event: "snapshot", data: '{"a":1}', id: "7" }]);
    expect(rest).toBe("event: machines\ndata: x");
  });

  test("joins data lines, defaults the name, and takes a value without its one leading space", () => {
    const { events } = parseEvents("data:one\ndata:  two\n\n");
    expect(events).toEqual([{ event: "message", data: "one\n two", id: null }]);
  });

  test("skips comments, keep-alives and unknown fields", () => {
    const { events, rest } = parseEvents(": keep-alive\n\nretry: 10\nfoo\ndata: x\n\n");
    expect(events).toEqual([{ event: "message", data: "x", id: null }]);
    expect(rest).toBe("");
  });

  test("ignores an id holding NUL", () => {
    const { events } = parseEvents("id: a\0b\ndata: x\n\n");
    expect(events[0]?.id).toBeNull();
  });

  test("accepts CRLF and CR line ends, holding a trailing CR for the next read", () => {
    expect(parseEvents("data: a\r\n\r\ndata: b\r\rdata: c\r").events).toEqual([
      { event: "message", data: "a", id: null },
      { event: "message", data: "b", id: null },
    ]);
    const first = parseEvents("data: c\r");
    expect(first.rest).toBe("data: c\r");
    expect(parseEvents(`${first.rest}\n\r\n`).events).toEqual([
      { event: "message", data: "c", id: null },
    ]);
  });
});

/** A body that delivers `chunks` one read at a time. */
function body(chunks: readonly string[]): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  return new ReadableStream({
    start(controller) {
      for (const chunk of chunks) {
        controller.enqueue(encoder.encode(chunk));
      }
      controller.close();
    },
  });
}

test("readEvents yields events split across reads, in order, until the body ends", async () => {
  const seen = [];
  for await (const event of readEvents(body(["id: 1\nda", "ta: a\n", "\nid: 2\ndata: b\n\n"]))) {
    seen.push(event);
  }
  expect(seen).toEqual([
    { event: "message", data: "a", id: "1" },
    { event: "message", data: "b", id: "2" },
  ]);
});
