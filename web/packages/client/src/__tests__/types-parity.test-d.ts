// Compile-time parity assertions between the hand-written wire interfaces in
// ../types.ts and the types generated from the committed OpenAPI contract
// (`contracts/api/mm_api_v1.yaml`) by `npm run gen:types`.
//
// Scope (improvement doc 02, TypeScript slice): the three core resources —
// streams, recordings, donations — plus the join-stream response. These are
// pure type-level checks executed by `vitest run --typecheck`; nothing here
// runs at runtime.
//
// If one of these assertions fails, either the spec or the hand-written wire
// interface drifted. The server serde structs in `crates/mm-api/src/` are the
// source of truth: fix the spec to match the server, regenerate with
// `npm run gen:types`, then align ../types.ts.

import { describe, expectTypeOf, it } from "vitest";

import type { components, paths } from "../generated/api-types";
import type {
  CreateStreamResponseWire,
  DonationResultWire,
  JoinResponseWire,
  RecordingItemWire,
  StreamSummaryWire,
} from "../types";

// Mutual-assignability assertion: A and B are exactly the same type.
type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends (
  <T>() => T extends B ? 1 : 2
)
  ? true
  : false;
type Expect<T extends true> = T;

// ---------------------------------------------------------------------------
// 1. STREAMS — the wire type `mapStreamSummary` consumes must be exactly the
//    generated schema for mm-api's `StreamResponse` (spec name: StreamDetails).
// ---------------------------------------------------------------------------

export type AssertStreamSchema = Expect<
  Equal<StreamSummaryWire, components["schemas"]["StreamDetails"]>
>;

// Via the paths tree too, proving the operation wiring (GET /streams/{id}).
type GetStream200 =
  paths["/_mm/client/v1/streams/{stream_id}"]["get"]["responses"]["200"]["content"]["application/json"];
export type AssertStreamOperation = Expect<
  Equal<StreamSummaryWire, GetStream200>
>;

// Create-stream response (host credentials incl. mm-switch publisher fields).
export type AssertCreateStreamSchema = Expect<
  Equal<CreateStreamResponseWire, components["schemas"]["CreateStreamResponse"]>
>;

// Join-stream response — must carry the four switch_* viewer fields so the
// SDK and the spec agree on the preferred (mm-switch) media path.
export type AssertJoinSchema = Expect<
  Equal<JoinResponseWire, components["schemas"]["JoinStreamResponse"]>
>;
type JoinStream200 =
  paths["/_mm/client/v1/streams/{stream_id}/join"]["post"]["responses"]["200"]["content"]["application/json"];
export type AssertJoinOperation = Expect<Equal<JoinResponseWire, JoinStream200>>;

// ---------------------------------------------------------------------------
// 2. RECORDINGS — list/get item shape (spec name: Recording).
// ---------------------------------------------------------------------------

export type AssertRecordingSchema = Expect<
  Equal<RecordingItemWire, components["schemas"]["Recording"]>
>;

// The room recordings list endpoint must page items of exactly that shape.
type ListRecordings200 =
  paths["/_mm/client/v1/rooms/{room_id}/recordings"]["get"]["responses"]["200"]["content"]["application/json"];
export type AssertRecordingOperation = Expect<
  Equal<RecordingItemWire, ListRecordings200["recordings"][number]>
>;

// ---------------------------------------------------------------------------
// 3. DONATIONS — create-donation response, including the Lightning `invoice`
//    branch (spec name: CreateDonationResponse).
// ---------------------------------------------------------------------------

export type AssertDonationSchema = Expect<
  Equal<DonationResultWire, components["schemas"]["CreateDonationResponse"]>
>;
type CreateDonation201 =
  paths["/_mm/client/v1/donations"]["post"]["responses"]["201"]["content"]["application/json"];
export type AssertDonationOperation = Expect<
  Equal<DonationResultWire, CreateDonation201>
>;

// ---------------------------------------------------------------------------
// vitest typecheck suite — same assertions through expectTypeOf so failures
// are reported per-case by `vitest run --typecheck`.
// ---------------------------------------------------------------------------

describe("generated API types parity (contracts/api/mm_api_v1.yaml)", () => {
  it("streams: StreamSummaryWire === schemas.StreamDetails", () => {
    expectTypeOf<StreamSummaryWire>().toEqualTypeOf<
      components["schemas"]["StreamDetails"]
    >();
    expectTypeOf<StreamSummaryWire>().toEqualTypeOf<GetStream200>();
  });

  it("streams: CreateStreamResponseWire === schemas.CreateStreamResponse", () => {
    expectTypeOf<CreateStreamResponseWire>().toEqualTypeOf<
      components["schemas"]["CreateStreamResponse"]
    >();
  });

  it("join-stream: JoinResponseWire === schemas.JoinStreamResponse (switch_* fields)", () => {
    expectTypeOf<JoinResponseWire>().toEqualTypeOf<
      components["schemas"]["JoinStreamResponse"]
    >();
    expectTypeOf<JoinResponseWire>().toEqualTypeOf<JoinStream200>();
  });

  it("recordings: RecordingItemWire === schemas.Recording", () => {
    expectTypeOf<RecordingItemWire>().toEqualTypeOf<
      components["schemas"]["Recording"]
    >();
    expectTypeOf<RecordingItemWire>().toEqualTypeOf<
      ListRecordings200["recordings"][number]
    >();
  });

  it("donations: DonationResultWire === schemas.CreateDonationResponse (incl. invoice)", () => {
    expectTypeOf<DonationResultWire>().toEqualTypeOf<
      components["schemas"]["CreateDonationResponse"]
    >();
    expectTypeOf<DonationResultWire>().toEqualTypeOf<CreateDonation201>();
  });
});
