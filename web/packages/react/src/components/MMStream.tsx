import { useEffect, useState } from "react";
import "../jsx.d";

/** Props for {@link MMStream}. */
export interface MMStreamProps {
  /** Matrix room id, e.g. `!abc:matrix.example.com`. */
  room: string;
  /** mm-core base URL, e.g. `https://matrix.example.com`. */
  server: string;
  /** MM session token. */
  token: string;
  className?: string;
}

/**
 * Thin wrapper around the `@matrixmedia/widget` `<mm-stream>` custom element.
 *
 * `@matrixmedia/widget` is an OPTIONAL dependency: its registration side-effect
 * is loaded dynamically on mount so this package still imports cleanly when the
 * widget isn't installed. While loading (or if absent) a fallback message is
 * rendered. The `<mm-stream>` tag is always emitted so server-rendered markup
 * and attribute wiring are stable; it upgrades once the element registers.
 */
export function MMStream({
  room,
  server,
  token,
  className,
}: MMStreamProps): JSX.Element {
  const [status, setStatus] = useState<"loading" | "ready" | "unavailable">(
    "loading",
  );

  useEffect(() => {
    let cancelled = false;
    import("@matrixmedia/widget")
      .then(() => {
        if (!cancelled) setStatus("ready");
      })
      .catch(() => {
        if (!cancelled) setStatus("unavailable");
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (status === "unavailable") {
    return (
      <div className={className} role="alert">
        MatrixMedia stream widget is unavailable. Install the optional
        `@matrixmedia/widget` package to embed live streams.
      </div>
    );
  }

  return (
    <mm-stream room={room} server={server} token={token} className={className} />
  );
}
