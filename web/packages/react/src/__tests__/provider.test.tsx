import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import type { MMClient } from "@matrixmedia/client";
import { MMProvider } from "../MMProvider";
import { useRoomStreams } from "../hooks/useRoomStreams";

function makeStub(over: Partial<MMClient> = {}): MMClient {
  return {
    listRoomStreams: vi.fn().mockResolvedValue([{ id: "s1", isLive: false }]),
    ...over,
  } as unknown as MMClient;
}

function StreamsChild({ roomId }: { roomId: string }) {
  const { streams, error } = useRoomStreams(roomId);
  if (error) return <div>error: {error.message}</div>;
  return (
    <ul>
      {streams.map((s) => (
        <li key={s.id}>{s.id}</li>
      ))}
    </ul>
  );
}

describe("MMProvider + useRoomStreams", () => {
  it("renders streams from an injected stub client", async () => {
    const client = makeStub();
    render(
      <MMProvider client={client}>
        <StreamsChild roomId="!r:hs" />
      </MMProvider>,
    );

    expect(await screen.findByText("s1")).toBeTruthy();
    expect(client.listRoomStreams).toHaveBeenCalledWith("!r:hs");
  });

  it("surfaces an error from listRoomStreams via the hook error state", async () => {
    const client = makeStub({
      listRoomStreams: vi.fn().mockRejectedValue(new Error("boom")),
    });
    render(
      <MMProvider client={client}>
        <StreamsChild roomId="!r:hs" />
      </MMProvider>,
    );

    await waitFor(() =>
      expect(screen.getByText(/error: boom/)).toBeTruthy(),
    );
  });

  it("constructs a client from config when no client prop is given", async () => {
    render(
      <MMProvider config={{ baseUrl: "https://x", getToken: async () => "tok" }}>
        <div>configured</div>
      </MMProvider>,
    );
    expect(screen.getByText("configured")).toBeTruthy();
  });
});
