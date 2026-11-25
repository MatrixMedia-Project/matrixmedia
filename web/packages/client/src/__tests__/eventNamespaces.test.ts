import { describe, it, expect, vi } from "vitest";
import {
  BUILTIN_READ_NAMESPACES,
  defaultEventNamespaces,
  isValidNamespace,
  matchEventType,
  parseEventNamespaces,
} from "../eventNamespaces";
import { MMClient, clientHeader } from "../MMClient";
import { SDK_VERSION } from "../version";

const v1 = (namespace: string) => ({ namespace, profile: "v1" });
const names = (r: { read: { namespace: string }[] }) => r.read.map((e) => e.namespace);

describe("event namespaces (ADR-0011)", () => {
  it("a missing, malformed or unknown-version block yields the default", () => {
    for (const raw of [undefined, null, "x", 1, {}, { version: 2, write: "x", read: [] }]) {
      expect(parseEventNamespaces(raw)).toEqual(defaultEventNamespaces());
    }
    expect(defaultEventNamespaces().write).toBe("legacy");
    expect(names(defaultEventNamespaces())).toEqual([...BUILTIN_READ_NAMESPACES]);
  });

  it("parses the server's default announcement", () => {
    const out = parseEventNamespaces({
      version: 1,
      write: "legacy",
      read: BUILTIN_READ_NAMESPACES.map(v1),
      bot_user_id: "@mmbot:example.org",
    });
    expect(out.write).toBe("legacy");
    expect(names(out)).toEqual([...BUILTIN_READ_NAMESPACES]);
    expect(out.botUserId).toBe("@mmbot:example.org");
  });

  it("validates namespaces: one failing case per rule", () => {
    expect(isValidNamespace("dev.matrixmedia")).toBe(true);
    expect(isValidNamespace("org.example-media.v2")).toBe(true);
    expect(isValidNamespace("")).toBe(false);
    expect(isValidNamespace(`a.${"b".repeat(63)}`)).toBe(false);
    expect(isValidNamespace("matrixmedia")).toBe(false);
    expect(isValidNamespace("Dev.matrixmedia")).toBe(false);
    expect(isValidNamespace("dev.matrix_media")).toBe(false);
    expect(isValidNamespace("dev..matrixmedia")).toBe(false);
    for (const reserved of ["m", "m.x", "org.matrix", "org.matrix.x", "im.vector", "im.vector.x", "io.element", "io.element.x"]) {
      expect(isValidNamespace(reserved), reserved).toBe(false);
    }
  });

  it("drops invalid entries and unknown profiles, keeps the rest and the built-ins", () => {
    const out = parseEventNamespaces({
      version: 1,
      write: "org.new.media",
      read: [v1("org.new.media"), v1("org.matrix"), { namespace: "org.v2.media", profile: "v2" }, "junk"],
    });
    expect(names(out)).toEqual([...BUILTIN_READ_NAMESPACES, "org.new.media"]);
    expect(out.write).toBe("org.new.media");
  });

  it("an announcement cannot remove a built-in name", () => {
    const out = parseEventNamespaces({ version: 1, write: "legacy", read: [] });
    expect(names(out)).toEqual([...BUILTIN_READ_NAMESPACES]);
  });

  it("caps announced non-built-in names at 8, in order", () => {
    const read = Array.from({ length: 10 }, (_, i) => v1(`org.n${i}.media`));
    const out = parseEventNamespaces({ version: 1, write: "org.n9.media", read });
    const extra = names(out).filter((n) => !(BUILTIN_READ_NAMESPACES as readonly string[]).includes(n));
    expect(extra).toEqual(read.slice(0, 8).map((e) => e.namespace));
    expect(out.write).toBe("legacy"); // org.n9 was capped out: never write what is not read
  });

  it("honours write only when it is legacy or on the read list", () => {
    expect(parseEventNamespaces({ version: 1, write: "org.other", read: [] }).write).toBe("legacy");
    expect(parseEventNamespaces({ version: 1, write: "dev.matrixmedia", read: [] }).write).toBe("dev.matrixmedia");
  });

  it("ignores a malformed bot user id", () => {
    const base = { version: 1, write: "legacy", read: [] };
    expect(parseEventNamespaces({ ...base, bot_user_id: "mmbot" }).botUserId).toBeUndefined();
    expect(parseEventNamespaces({ ...base, bot_user_id: `@${"a".repeat(260)}:x` }).botUserId).toBeUndefined();
  });

  it("matches whole event types by known suffix only", () => {
    expect(matchEventType("com.matrixmedia.stream")).toEqual({ suffix: "stream", namespace: "com.matrixmedia" });
    expect(matchEventType("dev.matrixmedia.stream.e2ee_key")?.suffix).toBe("stream.e2ee_key");
    expect(matchEventType("com.steegler.matrixmedia.feed.post")?.suffix).toBe("feed.post");
    expect(matchEventType("dev.matrixmedia")).toBeUndefined();
    expect(matchEventType("dev.matrixmedia.unknown")).toBeUndefined();
    expect(matchEventType("org.new.media.stream")).toBeUndefined();
    expect(matchEventType("org.new.media.stream", ["org.new.media"])?.suffix).toBe("stream");
  });
});

describe("X-MM-Client", () => {
  it("is sent on every request, the token exchange included", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({ mm_token: "mm", refresh_token: "rt", user_id: "@u:hs", expires_in: 900 }),
        { status: 200, headers: { "content-type": "application/json" } },
      ),
    );
    const c = new MMClient({ baseUrl: "https://x", getToken: () => "", fetch: fetchMock });
    const out = await c.exchangeOpenIdToken({
      access_token: "a",
      token_type: "Bearer",
      matrix_server_name: "hs",
      expires_in: 60,
    });
    const init = fetchMock.mock.calls[0][1] as RequestInit;
    expect((init.headers as Record<string, string>)["X-MM-Client"]).toBe(
      `matrixmedia-web/${SDK_VERSION} (web; build 0)`,
    );
    // An old server sends no block: the client writes legacy names.
    expect(out.eventNamespaces.write).toBe("legacy");
  });

  it("can be customised or switched off", () => {
    expect(clientHeader({ version: "2.1.0", build: "7" })).toBe("matrixmedia-web/2.1.0 (web; build 7)");
    expect(clientHeader(false)).toBeUndefined();
  });
});
