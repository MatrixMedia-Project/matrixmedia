import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, act } from "@testing-library/react";
import type { JoinStreamResponse } from "@matrixmedia/client";
import { useViewer } from "../hooks/useViewer";

const connect = vi.fn().mockResolvedValue(undefined);
const disconnect = vi.fn().mockResolvedValue(undefined);
const on = vi.fn();

vi.mock("@matrixmedia/client/webrtc", () => {
  return {
    StreamViewer: vi.fn().mockImplementation(() => ({
      connect,
      disconnect,
      on,
      get mediaStream() {
        return null;
      },
    })),
  };
});

const joinable: JoinStreamResponse = {
  sfuUrl: "wss://sfu",
  sfuToken: "t",
  participantId: "p1",
};

function Probe({ j }: { j: JoinStreamResponse | null }) {
  useViewer(j);
  return null;
}

describe("useViewer lifecycle", () => {
  beforeEach(() => {
    connect.mockClear();
    disconnect.mockClear();
    on.mockClear();
  });

  it("connects on a non-null joinable", async () => {
    await act(async () => {
      render(<Probe j={joinable} />);
    });
    expect(connect).toHaveBeenCalledWith(joinable);
    expect(on).toHaveBeenCalled();
  });

  it("disconnects on unmount", async () => {
    let unmount: () => void = () => {};
    await act(async () => {
      const r = render(<Probe j={joinable} />);
      unmount = r.unmount;
    });
    expect(connect).toHaveBeenCalledTimes(1);
    await act(async () => {
      unmount();
    });
    expect(disconnect).toHaveBeenCalled();
  });

  it("does not connect when joinable is null", async () => {
    await act(async () => {
      render(<Probe j={null} />);
    });
    expect(connect).not.toHaveBeenCalled();
  });
});
