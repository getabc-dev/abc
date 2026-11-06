// Server-sent events, read from a response body (the HTML standard's event stream format). The
// live stream is read with `fetch` rather than `EventSource`, which cannot resume from a position
// after a refused reconnect (it gives up for good on any answer but 200) and hides the API's error.

/** One dispatched event. */
export type SseEvent = {
  readonly event: string;
  readonly data: string;
  readonly id: string | null;
};

/** Takes the stream's text a chunk at a time and returns the events each chunk completes. */
export type EventParser = (chunk: string) => SseEvent[];

/**
 * A parser for one stream. It looks at each character once: it keeps only the unfinished line
 * and the fields of the event in progress, so a large event split across many chunks costs time
 * in proportion to its length.
 */
export function eventParser(): EventParser {
  let line = "";
  // A CR ended the last line; an LF right after it belongs to the same line end.
  let afterCr = false;
  let event = "message";
  let id: string | null = null;
  let data: string[] = [];

  const field = (text: string, events: SseEvent[]): void => {
    if (text === "") {
      // A blank line dispatches the event; a block of comments alone, such as a keep-alive,
      // dispatches nothing.
      if (data.length > 0) {
        events.push({ event, data: data.join("\n"), id });
      }
      event = "message";
      id = null;
      data = [];
      return;
    }
    if (text.startsWith(":")) {
      return;
    }
    const colon = text.indexOf(":");
    const name = colon === -1 ? text : text.slice(0, colon);
    const raw = colon === -1 ? "" : text.slice(colon + 1);
    const value = raw.startsWith(" ") ? raw.slice(1) : raw;
    if (name === "event") {
      event = value;
    } else if (name === "data") {
      data.push(value);
    } else if (name === "id" && !value.includes("\0")) {
      id = value;
    }
  };

  return (chunk) => {
    const events: SseEvent[] = [];
    let start = 0;
    for (let index = 0; index < chunk.length; index += 1) {
      const character = chunk[index];
      if (character === "\n" && afterCr && index === start) {
        // The LF of a CRLF whose CR ended the previous line, perhaps in the previous chunk.
        start = index + 1;
        afterCr = false;
        continue;
      }
      afterCr = false;
      if (character === "\r" || character === "\n") {
        field(line + chunk.slice(start, index), events);
        line = "";
        start = index + 1;
        afterCr = character === "\r";
      }
    }
    line += chunk.slice(start);
    return events;
  };
}

/**
 * The events of `body`, as they arrive, until it ends. A consumer that stops early, by returning
 * or throwing, cancels the body, so the response does not stay open.
 */
export async function* readEvents(body: ReadableStream<Uint8Array>): AsyncGenerator<SseEvent> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  const parse = eventParser();
  let finished = false;
  try {
    for (;;) {
      let read: ReadableStreamReadResult<Uint8Array>;
      try {
        // Each read waits for the stream's next chunk; they come one after another.
        // oxlint-disable-next-line eslint/no-await-in-loop -- reads are sequential
        read = await reader.read();
      } catch (error: unknown) {
        // A failed body is already closed; the failure is the caller's to see.
        finished = true;
        throw error;
      }
      const { done, value } = read;
      if (done) {
        finished = true;
        return;
      }
      yield* parse(decoder.decode(value, { stream: true }));
    }
  } finally {
    if (!finished) {
      await reader.cancel();
    }
  }
}
