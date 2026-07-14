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
  // A realistic StreamResponse wire row (snake_case, numeric room_id).
  const streamRow = (over: Record<string, unknown> = {}) => ({
    id: "s1",
    room_id: 42,
    host_user_id: "@h:hs",
    media_type: "video",
    title: "Live now",
    status: "active",
    participant_count: 3,
    started_at: "2026-06-07T00:00:00Z",
    ended_at: null,
    state_event_id: "$evt",
    min_tier_level: null,
    ...over,
  });

  it("listRoomStreams hits the prefixed path with auth", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValue(jsonResponse({ streams: [streamRow()] }));
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: async () => "tok",
      fetch: fetchMock,
    });
    const out = await c.listRoomStreams("!r:hs");
    expect(out[0].id).toBe("s1");
    expect(out[0].roomId).toBe("42");
    expect(out[0].isLive).toBe(true);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toContain("/_mm/client/v1/rooms/");
    expect(init.headers.Authorization).toBe("Bearer tok");
  });

  it("getStream maps id, stringified room_id, and optional fields", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse(
        streamRow({ id: "s7", room_id: 99, status: "ended", ended_at: "2026-06-07T01:00:00Z" }),
      ),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.getStream("s7");
    expect(out.id).toBe("s7");
    expect(out.roomId).toBe("99");
    expect(out.isLive).toBe(false);
    expect(out.endedAt).toBe("2026-06-07T01:00:00Z");
    expect(out.stateEventId).toBe("$evt");
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
      .mockResolvedValue(jsonResponse([streamRow({ id: "s9", room_id: 9 })]));
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.listRoomStreams("!r:hs");
    expect(out[0].id).toBe("s9");
    expect(out[0].roomId).toBe("9");
  });

  it("resumeStream issues POST .../streams/{id}/resume and maps CreateStreamResponse", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({
        stream_id: "s1",
        sfu_url: "wss://sfu",
        sfu_token: "jwt",
        state_event_id: "$evt",
      }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.resumeStream("s1");
    expect(out.streamId).toBe("s1");
    expect(out.sfuUrl).toBe("wss://sfu");
    expect(out.sfuToken).toBe("jwt");
    expect(out.stateEventId).toBe("$evt");
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
        sfu_url: "wss://sfu",
        sfu_token: "jwt",
        state_event_id: "$evt",
      }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.createStream("!r:hs", { title: "t", mediaType: "audio" });
    expect(out.streamId).toBe("s2");
    expect(out.sfuToken).toBe("jwt");
    expect(out.stateEventId).toBe("$evt");
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("https://x/_mm/client/v1/streams");
    expect(init.method).toBe("POST");
    const body = JSON.parse(init.body);
    expect(body.room_id).toBe("!r:hs");
    expect(body.media_type).toBe("audio");
    expect(init.headers["Content-Type"]).toBe("application/json");
  });

  it("createStream maps e2ee and switch credentials when present", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({
        stream_id: "s3",
        sfu_url: "wss://sfu",
        sfu_token: "jwt",
        state_event_id: "$evt",
        e2ee: {
          enabled: true,
          algorithm: "aes-gcm",
          key_id: "k1",
          key_generation: 2,
          key_b64: "AAAA",
        },
        switch_url: "https://switch",
        switch_source_id: "src1",
        switch_publisher_token: "ptok",
      }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.createStream("!r:hs");
    expect(out.e2ee?.keyB64).toBe("AAAA");
    expect(out.e2ee?.keyGeneration).toBe(2);
    expect(out.switchUrl).toBe("https://switch");
    expect(out.switchSourceId).toBe("src1");
    expect(out.switchPublisherToken).toBe("ptok");
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

  it("joinStream maps the mm-switch fields when the server returns them", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({
        sfu_url: "wss://sfu",
        sfu_token: "jwt",
        participant_id: "p1",
        switch_url: "https://hs/_mm/switch",
        switch_source_id: "stream-s1",
        switch_viewer_id: "viewer-s1--u-hs",
        switch_viewer_token: "hmac-token",
      }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.joinStream("s1");
    expect(out.switchUrl).toBe("https://hs/_mm/switch");
    expect(out.switchSourceId).toBe("stream-s1");
    expect(out.switchViewerId).toBe("viewer-s1--u-hs");
    expect(out.switchViewerToken).toBe("hmac-token");
    // LiveKit credentials still mapped alongside.
    expect(out.sfuUrl).toBe("wss://sfu");
    expect(out.sfuToken).toBe("jwt");
  });

  it("joinStream leaves switch fields undefined when absent (LiveKit fallback)", async () => {
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
    expect(out.switchUrl).toBeUndefined();
    expect(out.switchSourceId).toBeUndefined();
    expect(out.switchViewerId).toBeUndefined();
    expect(out.switchViewerToken).toBeUndefined();
    // The LiveKit path is untouched.
    expect(out.sfuUrl).toBe("wss://sfu");
    expect(out.sfuToken).toBe("jwt");
    expect(out.participantId).toBe("p1");
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
            title: "Last show",
            status: "ready",
            duration_ms: 1000,
            size_bytes: 2048,
            playback_url: "https://cdn/rec1.mp4",
            mxc_url: "mxc://hs/abc",
            thumbnail_url: "https://cdn/rec1.jpg",
            created_at: "2026-06-07T00:00:00Z",
            min_tier_level: null,
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
    expect(out[0].playbackUrl).toBe("https://cdn/rec1.mp4");
    expect(out[0].thumbnailUrl).toBe("https://cdn/rec1.jpg");
  });

  it("listCreatorTiers maps tiers", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({
        tiers: [
          {
            id: "t1",
            creator_user_id: "@creator:hs",
            room_id: null,
            name: "Gold",
            description: "Top tier",
            tier_level: 2,
            price_cents: 500,
            currency: "usd",
            stripe_price_id: "price_123",
            perks: ["ad_free", "vod"],
            permissions: {
              can_read: true,
              can_send: true,
              can_react: true,
              can_comment: true,
              can_watch_recordings: true,
              can_join_live: true,
              can_tip: true,
              can_manage_room: false,
            },
            active: true,
            created_at: "2026-06-07T00:00:00Z",
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
    expect(out[0].tierLevel).toBe(2);
    expect(out[0].priceCents).toBe(500);
    expect(out[0].perks).toEqual(["ad_free", "vod"]);
    expect(out[0].permissions.can_join_live).toBe(true);
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

  it("adDecision maps a serve_ad tagged-union response", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({
        type: "serve_ad",
        ad: {
          ad_id: "ad1",
          title: "Buy stuff",
          media_url: "https://c/ad.mp4",
          duration_secs: 15,
          click_through_url: "https://advertiser",
          owner_type: "platform",
        },
        impression_token: "imp",
        challenge: "ch",
        viewer_secret: "vs",
        slot: "pre_roll",
        enforcement: "sfu",
        skip_after_secs: 5,
      }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.adDecision("s1", "pre_roll");
    expect(out.type).toBe("serve_ad");
    if (out.type !== "serve_ad") throw new Error("expected serve_ad");
    expect(out.ad.id).toBe("ad1");
    expect(out.ad.mediaUrl).toBe("https://c/ad.mp4");
    expect(out.ad.durationSecs).toBe(15);
    expect(out.ad.clickThroughUrl).toBe("https://advertiser");
    expect(out.impressionToken).toBe("imp");
    expect(out.skipAfterSecs).toBe(5);
    const [url] = fetchMock.mock.calls[0];
    expect(url).toContain("/streams/s1/ad-decision?slot=pre_roll");
  });

  it("adDecision maps a no_ad tagged-union response", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse({ type: "no_ad", reason: "creator_opt_out" }),
    );
    const c = new MMClient({
      baseUrl: "https://x",
      getToken: () => "tok",
      fetch: fetchMock,
    });
    const out = await c.adDecision("s1");
    expect(out.type).toBe("no_ad");
    if (out.type !== "no_ad") throw new Error("expected no_ad");
    expect(out.reason).toBe("creator_opt_out");
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
