// Matrix event names of the MatrixMedia protocol (ADR-0011).
//
// Event types are a namespace plus a suffix (`dev.matrixmedia` +
// `stream` = `dev.matrixmedia.stream`). Every reader reads the built-in
// namespaces; a server may announce more in the `event_namespaces` block of
// the token exchange. This module parses that block by the rules every client
// follows (brief 2.3, 7.1, 7.2, 7.8) and matches event types.

/** Namespaces read always. An announcement can add to them, never remove. */
export const BUILTIN_READ_NAMESPACES = [
  "dev.matrixmedia",
  "com.matrixmedia",
  "com.steegler.matrixmedia",
] as const;

/** The suffixes of the protocol; only these match under any namespace. */
export const EVENT_SUFFIXES = [
  "stream",
  "stream.e2ee_key",
  "room_config",
  "donation",
  "feed.broadcast.started",
  "feed.broadcast.ended",
  "feed.recording.available",
  "feed.post",
  "feed_muted",
  "reaction_recents",
  "subscription_tiers",
  "subscription_proof",
  "content_gate",
] as const;

export type EventSuffix = (typeof EVENT_SUFFIXES)[number];

/** Payload profiles this SDK can parse. */
export const KNOWN_PROFILES = ["v1"] as const;

/** Most announced namespaces kept that are not built in. */
export const MAX_ANNOUNCED_NAMESPACES = 8;

/** `write` value meaning "use the legacy names". */
export const WRITE_LEGACY = "legacy";

const RESERVED_PREFIXES = ["m.", "org.matrix.", "im.vector.", "io.element."];

/** One namespace a client reads, with its payload profile. */
export interface AnnouncedNamespace {
  namespace: string;
  profile: string;
}

/** The `event_namespaces` block as the server sends it. */
export interface EventNamespacesWire {
  version: number;
  write: string;
  read: AnnouncedNamespace[];
  bot_user_id?: string;
}

/** The block after validation: what a client acts on. */
export interface EventNamespaces {
  /** `"legacy"` or a namespace on `read`. */
  write: string;
  /** Built-in namespaces first, then the valid announced ones. */
  read: AnnouncedNamespace[];
  /** The announcing server's bot, when announced in a valid form. */
  botUserId?: string;
}

/** What a client uses when a server announces nothing: write legacy names. */
export function defaultEventNamespaces(): EventNamespaces {
  return {
    write: WRITE_LEGACY,
    read: BUILTIN_READ_NAMESPACES.map((namespace) => ({ namespace, profile: "v1" })),
  };
}

/**
 * Lower-case reverse-DNS, at least two labels of `[a-z0-9-]`, at most 64
 * characters, and `<namespace>.` must not start with `m.`, `org.matrix.`,
 * `im.vector.` or `io.element.`.
 */
export function isValidNamespace(ns: string): boolean {
  if (ns.length === 0 || ns.length > 64) return false;
  const dotted = `${ns}.`;
  if (RESERVED_PREFIXES.some((p) => dotted.startsWith(p))) return false;
  const labels = ns.split(".");
  return labels.length >= 2 && labels.every((l) => /^[a-z0-9-]+$/.test(l));
}

function isBuiltin(ns: string): boolean {
  return (BUILTIN_READ_NAMESPACES as readonly string[]).includes(ns);
}

function isValidUserId(id: unknown): id is string {
  return typeof id === "string" && id.length <= 255 && /^@[^:]+:.+$/.test(id);
}

/**
 * Parse an `event_namespaces` block. A missing, malformed or unknown-version
 * block yields the default (built-in names, write legacy) without error;
 * invalid entries are dropped and the rest still apply.
 */
export function parseEventNamespaces(raw: unknown): EventNamespaces {
  const out = defaultEventNamespaces();
  if (typeof raw !== "object" || raw === null) return out;
  const block = raw as Partial<EventNamespacesWire>;
  if (block.version !== 1) return out;

  let extra = 0;
  for (const entry of Array.isArray(block.read) ? block.read : []) {
    if (typeof entry !== "object" || entry === null) continue;
    const { namespace, profile } = entry as Partial<AnnouncedNamespace>;
    if (typeof namespace !== "string" || typeof profile !== "string") continue;
    if (!(KNOWN_PROFILES as readonly string[]).includes(profile)) continue;
    if (!isValidNamespace(namespace)) continue;
    if (out.read.some((e) => e.namespace === namespace)) continue;
    if (!isBuiltin(namespace)) {
      if (extra >= MAX_ANNOUNCED_NAMESPACES) continue;
      extra += 1;
    }
    out.read.push({ namespace, profile });
  }

  if (
    typeof block.write === "string" &&
    (block.write === WRITE_LEGACY || out.read.some((e) => e.namespace === block.write))
  ) {
    out.write = block.write;
  }
  if (isValidUserId(block.bot_user_id)) out.botUserId = block.bot_user_id;
  return out;
}

/**
 * Match an event type against a namespace list: it equals
 * `<namespace>.<suffix>` for a known suffix. Whole-string match only.
 */
export function matchEventType(
  eventType: string,
  namespaces: readonly string[] = BUILTIN_READ_NAMESPACES,
): { suffix: EventSuffix; namespace: string } | undefined {
  for (const namespace of namespaces) {
    if (!eventType.startsWith(`${namespace}.`)) continue;
    const rest = eventType.slice(namespace.length + 1);
    if ((EVENT_SUFFIXES as readonly string[]).includes(rest)) {
      return { suffix: rest as EventSuffix, namespace };
    }
  }
  return undefined;
}
