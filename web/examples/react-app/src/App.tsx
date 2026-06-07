import { useState } from "react";
import type { JoinStreamResponse, MMClientOptions } from "@matrixmedia/client";
import {
  MMProvider,
  MMStream,
  MMViewer,
  useMMClient,
  useRoomStreams,
} from "@matrixmedia/react";

// ---------------------------------------------------------------------------
// Demo configuration
//
// Replace these with your own homeserver / room and a real token source.
// `getToken` must return a *MM session token* (a JWT minted by mm-core after
// exchanging a Matrix OpenID token — see docs/web-sdk/getting-started.md). For
// this demo it just reads a value baked in at build time, prompts the browser,
// or falls back to an obviously-fake placeholder.
// ---------------------------------------------------------------------------

const SERVER_BASE_URL =
  import.meta.env.VITE_MM_SERVER ?? "https://matrix.example.com";
const ROOM_ID = import.meta.env.VITE_MM_ROOM ?? "!demo:matrix.example.com";

const config: MMClientOptions = {
  baseUrl: SERVER_BASE_URL,
  // DEMO ONLY: a real app obtains this via client.exchangeOpenIdToken(...).
  getToken: () =>
    import.meta.env.VITE_MM_TOKEN ??
    (typeof window !== "undefined"
      ? window.prompt("Paste an MM session token") ?? "demo-token"
      : "demo-token"),
};

export function App(): JSX.Element {
  return (
    <MMProvider config={config}>
      <main style={{ fontFamily: "system-ui", maxWidth: 720, margin: "2rem auto" }}>
        <h1>MatrixMedia React example</h1>
        <p>
          Room: <code>{ROOM_ID}</code> on <code>{SERVER_BASE_URL}</code>
        </p>
        <RoomStreams roomId={ROOM_ID} />
        <hr />
        <h2>Embedded widget (custom element wrapper)</h2>
        <MMStream
          room={ROOM_ID}
          server={SERVER_BASE_URL}
          token={import.meta.env.VITE_MM_TOKEN ?? "demo-token"}
        />
      </main>
    </MMProvider>
  );
}

function RoomStreams({ roomId }: { roomId: string }): JSX.Element {
  const client = useMMClient();
  const { streams, loading, error, refetch } = useRoomStreams(roomId);
  const [joinable, setJoinable] = useState<JoinStreamResponse | null>(null);

  const join = async (streamId: string) => {
    // joinStream() needs the optional `livekit-client` peer at playback time.
    setJoinable(await client.joinStream(streamId));
  };

  if (loading) return <p>Loading streams…</p>;
  if (error) return <p role="alert">Error: {error.message}</p>;

  return (
    <section>
      <h2>Streams in this room</h2>
      <button type="button" onClick={() => void refetch()}>
        Refresh
      </button>
      {streams.length === 0 ? (
        <p>No streams yet.</p>
      ) : (
        <ul>
          {streams.map((s) => (
            <li key={s.id}>
              <strong>{s.title ?? s.id}</strong> — {s.status} · {s.mediaType} ·{" "}
              {s.participantCount} watching
              {s.status === "active" ? (
                <button type="button" onClick={() => void join(s.id)}>
                  Watch
                </button>
              ) : null}
            </li>
          ))}
        </ul>
      )}

      {joinable ? (
        <div>
          <h3>Now watching</h3>
          <MMViewer joinable={joinable} controls className="mm-viewer" />
          <button type="button" onClick={() => setJoinable(null)}>
            Stop
          </button>
        </div>
      ) : null}
    </section>
  );
}
