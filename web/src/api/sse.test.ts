import { describe, expect, test } from "vitest";

import { eventParser, readEvents, type SseEvent } from "./sse";

/** The events `chunks` complete, fed one at a time to one parser, and each chunk's share. */
function parse(...chunks: string[]): SseEvent[][] {
  const parser = eventParser();
  return chunks.map((chunk) => parser(chunk));
}

describe("eventParser", () => {
  test("reads each complete event's name, data and id, and keeps the rest", () => {
    const [first, second] = parse(
      'event: snapshot\nid: 7\ndata: {"a":1}\n\nevent: machines\ndata: x',
      "\n\n",
    );
    expect(first).toEqual([{ event: "snapshot", data: '{"a":1}', id: "7" }]);
    expect(second).toEqual([{ event: "machines", data: "x", id: null }]);
  });

  test("joins data lines, defaults the name, and takes a value without its one leading space", () => {
    const [events] = parse("data:one\ndata:  two\n\n");
    expect(events).toEqual([{ event: "message", data: "one\n two", id: null }]);
  });

  test("skips comments, keep-alives and unknown fields", () => {
    const [events, rest] = parse(": keep-alive\n\nretry: 10\nfoo\ndata: x\n\n", "");
    expect(events).toEqual([{ event: "message", data: "x", id: null }]);
    expect(rest).toEqual([]);
  });

  test("ignores an id holding NUL", () => {
    const [events] = parse("id: a\0b\ndata: x\n\n");
    expect(events?.[0]?.id).toBeNull();
  });

  test("accepts CRLF and CR line ends, holding a trailing CR for the next read", () => {
    expect(parse("data: a\r\n\r\ndata: b\r\rdata: c\r")[0]).toEqual([
      { event: "message", data: "a", id: null },
      { event: "message", data: "b", id: null },
    ]);
    const [first, second] = parse("data: c\r", "\n\r\n");
    expect(first).toEqual([]);
    expect(second).toEqual([{ event: "message", data: "c", id: null }]);
  });

  test("reads a large event split across many small chunks in linear time", () => {
    const data = "x".repeat(2_000_000);
    const text = `id: 9\ndata: ${data}\n\n`;
    const parser = eventParser();
    const events: SseEvent[] = [];
    for (let start = 0; start < text.length; start += 16) {
      events.push(...parser(text.slice(start, start + 16)));
    }
    expect(events).toEqual([{ event: "message", data, id: "9" }]);
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

test("readEvents reads an event split across many small chunks whole", async () => {
  const data = "x".repeat(200_000);
  const text = `id: 9\ndata: ${data}\n\n`;
  const chunks = Array.from({ length: Math.ceil(text.length / 7) }, (_, index) =>
    text.slice(index * 7, index * 7 + 7),
  );
  const seen = [];
  for await (const event of readEvents(body(chunks))) {
    seen.push(event);
  }
  expect(seen).toEqual([{ event: "message", data, id: "9" }]);
});

test("readEvents cancels the body when its consumer stops early", async () => {
  let cancelled = false;
  const encoder = new TextEncoder();
  const open = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(encoder.encode("data: a\n\ndata: b\n\n"));
    },
    cancel() {
      cancelled = true;
    },
  });
  const consume = async () => {
    for await (const event of readEvents(open)) {
      throw new Error(`stopped at ${event.data}`);
    }
  };
  await expect(consume()).rejects.toThrow("stopped at a");
  expect(cancelled).toBe(true);
});
