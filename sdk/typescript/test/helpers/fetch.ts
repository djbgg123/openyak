export function createQueuedFetch(responses: Response[]): typeof fetch {
  const queue = [...responses];
  return (async () => {
    const response = queue.shift();
    if (!response) {
      throw new Error("unexpected fetch call with no queued response");
    }
    return response;
  }) as typeof fetch;
}

export function jsonResponse(body: unknown, init: ResponseInit = {}): Response {
  return new Response(JSON.stringify(body), {
    status: init.status ?? 200,
    headers: {
      "content-type": "application/json",
      ...(init.headers ?? {}),
    },
  });
}

export function sseResponse(events: unknown[]): Response {
  const body = events
    .map((event) => `data: ${JSON.stringify(event)}\n\n`)
    .join("");
  return new Response(body, {
    status: 200,
    headers: {
      "content-type": "text/event-stream",
    },
  });
}
