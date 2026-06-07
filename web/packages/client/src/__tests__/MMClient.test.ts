import { describe, it, expect, vi } from "vitest";
import { MMClient } from "../MMClient";
import { MMError } from "../types";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

describe("MMClient", () => {
  it("listRoomStreams hits the prefixed path with auth", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValue(jsonResponse({ streams: [{ stream_id: "s1" }] }));
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: async () => "tok",
      fetch: fetchMock,
    });
    const out = await c.listRoomStreams("!r:hs");
    expect(out[0].id).toBe("s1");
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toContain("/_mm/client/v1/rooms/");
    expect(init.headers.Authorization).toBe("Bearer tok");
  });

  it("listRoomStreams URL-encodes the room id in the path", async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse({ streams: [] }));
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    await c.listRoomStreams("!room:hs.example");
    const [url] = fetchMock.mock.calls[0];
    expect(url).toContain(encodeURIComponent("!room:hs.example"));
    // The raw, un-encoded id must NOT appear verbatim in the path.
    expect(url).not.toContain("/rooms/!room:hs.example/");
  });

  it("listRoomStreams accepts a bare array body too", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValue(jsonResponse([{ stream_id: "s9" }]));
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.listRoomStreams("!r:hs");
    expect(out[0].id).toBe("s9");
  });

  it("resumeStream issues POST .../streams/{id}/resume", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({
        stream_id: "s1",
        room_id: "!r:hs",
        host_user_id: "@h:hs",
        media_type: "video",
        status: "active",
        participant_count: 0,
        started_at: "2026-06-07T00:00:00Z",
        sfu_url: "wss://sfu",
        sfu_token: "jwt",
      }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.resumeStream("s1");
    expect(out.id).toBe("s1");
    expect(out.sfuUrl).toBe("wss://sfu");
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("https://x/_mm/client/v1/streams/s1/resume");
    expect(init.method).toBe("POST");
    expect(init.headers.Authorization).toBe("Bearer tok");
  });

  it("throws MMError with .code from body and .status on non-2xx", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValue(jsonResponse({ error: "stream_ended" }, 409));
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    await expect(c.resumeStream("s1")).rejects.toMatchObject({
      code: "stream_ended",
      status: 409,
    });
    await expect(c.resumeStream("s1")).rejects.toBeInstanceOf(MMError);
  });

  it("maps unknown error codes to 'unknown'", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValue(jsonResponse({ error: "some_weird_thing" }, 500));
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    await expect(c.getStream("s1")).rejects.toMatchObject({
      code: "unknown",
      status: 500,
    });
  });

  it("createStream POSTs /streams with snake_case body and maps response", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({
        stream_id: "s2",
        room_id: "!r:hs",
        host_user_id: "@h:hs",
        media_type: "audio",
        status: "active",
        participant_count: 0,
        started_at: "2026-06-07T00:00:00Z",
        sfu_url: "wss://sfu",
        sfu_token: "jwt",
      }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.createStream("!r:hs", { title: "t", mediaType: "audio" });
    expect(out.id).toBe("s2");
    expect(out.sfuToken).toBe("jwt");
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("https://x/_mm/client/v1/streams");
    expect(init.method).toBe("POST");
    const body = JSON.parse(init.body);
    expect(body.room_id).toBe("!r:hs");
    expect(body.media_type).toBe("audio");
    expect(init.headers["Content-Type"]).toBe("application/json");
  });

  it("exchangeOpenIdToken posts /auth/token without requiring a prior token", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({
        mm_token: "mm",
        refresh_token: "rt",
        user_id: "@u:hs",
        expires_in: 3600,
      }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "",
      fetch: fetchMock,
    });
    const out = await c.exchangeOpenIdToken({
      access_token: "a",
      token_type: "Bearer",
      matrix_server_name: "hs",
      expires_in: 60,
    });
    expect(out.mmToken).toBe("mm");
    const [url] = fetchMock.mock.calls[0];
    expect(url).toBe("https://x/_mm/client/v1/auth/token");
  });

  it("joinStream POSTs and maps sfu fields", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({
        sfu_url: "wss://sfu",
        sfu_token: "jwt",
        participant_id: "p1",
      }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.joinStream("s1");
    expect(out.sfuUrl).toBe("wss://sfu");
    expect(out.participantId).toBe("p1");
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("https://x/_mm/client/v1/streams/s1/join");
    expect(init.method).toBe("POST");
  });

  it("listRoomRecordings maps snake_case recording rows", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({
        recordings: [
          {
            id: "rec1",
            stream_id: "s1",
            host_user_id: "@h:hs",
            media_type: "video",
            duration_ms: 1000,
            status: "ready",
            created_at: "2026-06-07T00:00:00Z",
          },
        ],
        has_more: false,
      }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.listRoomRecordings("!r:hs");
    expect(out[0].id).toBe("rec1");
    expect(out[0].streamId).toBe("s1");
    expect(out[0].durationMs).toBe(1000);
  });

  it("listCreatorTiers maps tiers", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({
        tiers: [
          {
            tier_id: "t1",
            tier_name: "Gold",
            tier_level: 2,
            price_cents: 500,
            currency: "usd",
          },
        ],
      }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.listCreatorTiers("@creator:hs");
    expect(out[0].id).toBe("t1");
    expect(out[0].name).toBe("Gold");
    expect(out[0].priceCents).toBe(500);
    const [url] = fetchMock.mock.calls[0];
    expect(url).toContain(
      `/creators/${encodeURIComponent("@creator:hs")}/tiers`,
    );
  });

  it("donate POSTs to /donations with stream_id in the body and maps result", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({ donation_id: "d1", checkout_url: "https://pay" }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.donate("s1", { amountCents: 500, message: "hi" });
    expect(out.donationId).toBe("d1");
    expect(out.checkoutUrl).toBe("https://pay");
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("https://x/_mm/client/v1/donations");
    const body = JSON.parse(init.body);
    expect(body.stream_id).toBe("s1");
    expect(body.amount_cents).toBe(500);
  });

  it("adDecision GETs ad-decision with slot query", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({
        impression_token: "imp",
        creative_url: "https://c",
        duration_secs: 15,
        slot: "pre_roll",
        challenge: "ch",
      }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.adDecision("s1", "pre_roll");
    expect(out.impressionToken).toBe("imp");
    const [url] = fetchMock.mock.calls[0];
    expect(url).toContain("/streams/s1/ad-decision?slot=pre_roll");
  });

  it("default fetch falls back to global fetch", async () => {
    const globalSpy = vi
      .spyOn(globalThis, "fetch")
      .mockResolvedValue(jsonResponse({ streams: [] }));
    const c = new MMClient({ baseUrl: "https://x", getToken: () => "tok" });
    await c.listRoomStreams("!r:hs");
    expect(globalSpy).toHaveBeenCalled();
    globalSpy.mockRestore();
  });
});
