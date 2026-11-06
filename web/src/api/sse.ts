// Server-sent events, read from a response body (the HTML standard's event stream format). The
// live stream is read with `fetch` rather than `EventSource`, which cannot resume from a position
// after a refused reconnect (it gives up for good on any answer but 200) and hides the API's error.

/** One dispatched event. */
export type SseEvent = {
  readonly event: string;
  readonly data: string;
  readonly id: string | null;
};

/** The events completed by `text`, and the text after the last one, to carry into the next read. */
export function parseEvents(text: string): { events: SseEvent[]; rest: string } {
  // A trailing CR may be the first half of a CRLF the next read completes.
  const held = text.endsWith("\r") ? "\r" : "";
  const normal = (held === "" ? text : text.slice(0, -1)).replaceAll(/\r\n?/g, "\n");
  const blocks = normal.split("\n\n");
  const rest = (blocks.pop() ?? "") + held;
  const events: SseEvent[] = [];
  for (const block of blocks) {
    let event = "message";
    let id: string | null = null;
    const data: string[] = [];
    for (const line of block.split("\n")) {
      if (line === "" || line.startsWith(":")) {
        continue;
      }
      const colon = line.indexOf(":");
      const field = colon === -1 ? line : line.slice(0, colon);
      const raw = colon === -1 ? "" : line.slice(colon + 1);
      const value = raw.startsWith(" ") ? raw.slice(1) : raw;
      if (field === "event") {
        event = value;
      } else if (field === "data") {
        data.push(value);
      } else if (field === "id" && !value.includes("\0")) {
        id = value;
      }
    }
    // A block of comments alone, such as a keep-alive, dispatches nothing.
    if (data.length > 0) {
      events.push({ event, data: data.join("\n"), id });
    }
  }
  return { events, rest };
}

/** The events of `body`, as they arrive, until it ends. */
export async function* readEvents(body: ReadableStream<Uint8Array>): AsyncGenerator<SseEvent> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let pending = "";
  for (;;) {
    // Each read waits for the stream's next chunk; they come one after another.
    // oxlint-disable-next-line eslint/no-await-in-loop -- reads are sequential
    const { done, value } = await reader.read();
    if (done) {
      return;
    }
    const { events, rest } = parseEvents(pending + decoder.decode(value, { stream: true }));
    pending = rest;
    yield* events;
  }
}
