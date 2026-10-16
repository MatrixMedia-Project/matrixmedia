import { describe, it, expect, vi } from "vitest";
import { MMClient } from "../MMClient";
import { toErrorCode, type ErrorCode, type ServerErrorCode } from "../types";

function jsonResponse(body: unknown, status: number): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function client(res: Response): MMClient {
  return new MMClient({
    baseUrl: "https://x",
    getToken: () => "tok",
    fetch: vi.fn().mockResolvedValue(res),
    maxRetries: 0,
    timeoutMs: 0,
  });
}

describe("server error codes", () => {
  // REGRESSION. The SDK used to match a lowercase vocabulary ("stream_ended") that
  // mm-core never sends; it sends MM_STREAM_ENDED. Every real error therefore
  // reached callers as "unknown" — and the tests mocked the lowercase shape, so
  // nothing noticed.
  it("a real mm-core error is not reported as unknown", async () => {
    await expect(
      client(jsonResponse({ error: "MM_CONTENT_GATED", message: "subscribe" }, 402)).getStream("s1"),
    ).rejects.toMatchObject({ code: "content_gated", serverCode: "MM_CONTENT_GATED", status: 402 });
  });

  // The categories callers actually branch on, with the codes the server uses
  // for them. Table-driven so a wrong entry names itself.
  const cases: Array<[ServerErrorCode, ErrorCode]> = [
    ["MM_NOT_FOUND", "not_found"],
    ["MM_STREAM_ENDED", "stream_ended"],
    ["MM_RATE_LIMITED", "rate_limited"],
    ["M_LIMIT_EXCEEDED", "rate_limited"],
    ["MM_INVALID_TOKEN", "unauthorized"],
    ["MM_FORBIDDEN", "forbidden"],
    ["MM_PERMISSION_DENIED", "forbidden"],
    ["MM_CONTENT_GATED", "content_gated"],
    ["MM_TIER_TOO_LOW", "content_gated"],
    ["MM_INSUFFICIENT_TIER", "content_gated"],
    ["MM_PAYMENT_FAILED", "payment_required"],
    ["MM_BALANCE_TOO_LOW", "payment_required"],
    ["MM_INVALID_REQUEST", "invalid_request"],
    ["MM_INVALID_AMOUNT", "invalid_request"],
    ["MM_SETTINGS_INVALID", "invalid_request"],
    ["MM_SETTINGS_READ_ONLY", "invalid_request"],
    ["MM_ROOM_FULL", "unknown"],
    ["MM_INTERNAL", "unknown"],
    ["MM_AD_CONFLICT", "unknown"],
    ["MM_SETTINGS_CONFLICT", "unknown"],
    ["MM_AD_UPLOAD_REJECTED", "unknown"],
    ["MM_AD_UPLOAD_TIMEOUT", "unknown"],
  ];
  it.each(cases)("%s -> %s", (server, sdk) => {
    expect(toErrorCode(server)).toBe(sdk);
  });

  // mm-core sends MM_FORBIDDEN with HTTP 401. The CODE decides the category, not
  // the status. (The code itself is ambiguous; see FROM_SERVER in types.ts.)
  it("a known code wins over its HTTP status", async () => {
    await expect(
      client(jsonResponse({ error: "MM_FORBIDDEN", message: "only the host" }, 401)).getStream("s1"),
    ).rejects.toMatchObject({ code: "forbidden", status: 401 });
  });

  // The SDK's "timeout" means the client gave up waiting. A server's 408 is a
  // different thing, and a known code is never re-read from its status.
  it("a server-side upload timeout is not the client's timeout", async () => {
    await expect(
      client(jsonResponse({ error: "MM_AD_UPLOAD_TIMEOUT", message: "stalled" }, 408)).getStream("s1"),
    ).rejects.toMatchObject({ code: "unknown", serverCode: "MM_AD_UPLOAD_TIMEOUT", status: 408 });
  });

  // A code with no SDK category is still reported precisely.
  it("a code with no category keeps its exact server code", async () => {
    await expect(
      client(jsonResponse({ error: "MM_ROOM_FULL", message: "full" }, 409)).getStream("s1"),
    ).rejects.toMatchObject({ code: "unknown", serverCode: "MM_ROOM_FULL" });
  });
});

describe("when the body has no recognisable code", () => {
  // A code newer than this SDK: the status still carries meaning.
  it("falls back to the HTTP status", async () => {
    await expect(
      client(jsonResponse({ error: "MM_SOMETHING_NEW" }, 402)).getStream("s1"),
    ).rejects.toMatchObject({ code: "payment_required", serverCode: "MM_SOMETHING_NEW" });
  });

  // A proxy's HTML page or Axum's plain-text rejection of a malformed body.
  it("classifies a non-JSON error by status, with no server code", async () => {
    const res = new Response("Failed to deserialize the JSON body", { status: 422 });
    await expect(client(res).getStream("s1")).rejects.toMatchObject({
      code: "invalid_request",
      serverCode: null,
      status: 422,
    });
  });

  it.each([
    [400, "invalid_request"],
    [401, "unauthorized"],
    [403, "forbidden"],
    [404, "not_found"],
    [410, "stream_ended"],
    [429, "rate_limited"],
    [500, "unknown"],
    [503, "unknown"],
  ] as Array<[number, ErrorCode]>)("status %i -> %s", (status, sdk) => {
    expect(toErrorCode(undefined, status)).toBe(sdk);
  });
});

describe("compatibility", () => {
  // Test doubles written against the SDK's documented names keep working.
  it("accepts the SDK's own names verbatim", () => {
    expect(toErrorCode("stream_ended")).toBe("stream_ended");
    expect(toErrorCode("content_gated", 500)).toBe("content_gated");
  });

  it("the one-argument form still works for existing callers", () => {
    expect(toErrorCode("MM_NOT_FOUND")).toBe("not_found");
    expect(toErrorCode(42)).toBe("unknown");
  });
});
