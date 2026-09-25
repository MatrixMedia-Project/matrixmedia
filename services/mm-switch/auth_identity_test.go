package main

import (
	"net/http"
	"net/http/httptest"
	"os"
	"testing"
)

// ─── FR-347: identity binding ────────────────────────────────────────────────

// captureIdentity is a handler that records what the middleware attached.
func captureIdentity(got *switchIdentity) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		*got = identityOf(r)
		w.WriteHeader(200)
	}
}

func TestMiddlewareAttachesRoleAndSubjectInsteadOfDiscardingThem(t *testing.T) {
	const secret = "identity-test"
	var got switchIdentity

	wrapped := wrapAuth(secret, []string{roleServer, roleViewer}, captureIdentity(&got))

	req := httptest.NewRequest("POST", "/api/viewers/offer", nil)
	req.Header.Set("Authorization", "Bearer "+generateToken(secret, roleViewer, "viewer-s1-@u-hs", 60))
	rec := httptest.NewRecorder()
	wrapped.ServeHTTP(rec, req)

	if rec.Code != 200 {
		t.Fatalf("expected the valid token to pass, got %d", rec.Code)
	}
	if !got.Authenticated {
		t.Fatal("the handler saw no identity — the middleware validated the token and threw it away")
	}
	if got.Role != roleViewer || got.Subject != "viewer-s1-@u-hs" {
		t.Fatalf("identity not propagated: role=%q subject=%q", got.Role, got.Subject)
	}
}

func TestNoSecretMeansNoIdentity(t *testing.T) {
	var got switchIdentity
	wrapped := wrapAuth("", []string{roleServer}, captureIdentity(&got))

	rec := httptest.NewRecorder()
	wrapped.ServeHTTP(rec, httptest.NewRequest("GET", "/api/viewers", nil))

	if rec.Code != 200 {
		t.Fatalf("an unsecured origin must still pass requests through, got %d", rec.Code)
	}
	if got.Authenticated {
		t.Fatal("an unsecured pass-through must NOT look authenticated to handlers — " +
			"that would turn 'no auth configured' into 'trusted caller'")
	}
}

// THE ONE THAT MATTERS.
//
// A viewer token is minted per viewer, with sub = the id mm-core derived from the
// authenticated Matrix user. Before bindSubject, the body's `id` was taken at face
// value — so any holder of any valid viewer token could send another viewer's id,
// and AddViewer would displace that viewer's registration.
func TestViewerTokenCannotClaimAnotherViewersId(t *testing.T) {
	attacker := switchIdentity{Role: roleViewer, Subject: "viewer-s1-@attacker-hs", Authenticated: true}

	if _, err := bindSubject(attacker, "viewer-s1-@victim-hs"); err == nil {
		t.Fatal("a viewer token was allowed to claim a DIFFERENT viewer id — " +
			"one request would displace the victim's session")
	}
}

func TestViewerTokenWithoutIdAdoptsItsSubject(t *testing.T) {
	id := switchIdentity{Role: roleViewer, Subject: "viewer-s1-@u-hs", Authenticated: true}

	got, err := bindSubject(id, "")
	if err != nil {
		t.Fatalf("an omitted id must adopt the subject, got error: %v", err)
	}
	if got != "viewer-s1-@u-hs" {
		t.Fatalf("expected the subject to be adopted, got %q", got)
	}
}

func TestViewerTokenMatchingItsOwnSubjectIsAccepted(t *testing.T) {
	// The shipped SDKs send switch_viewer_id as the body id, and mm-core mints
	// the token with sub = that same id. This is the live path — if it ever
	// fails, every app in both stores stops being able to watch.
	id := switchIdentity{Role: roleViewer, Subject: "viewer-s1-@u-hs", Authenticated: true}

	got, err := bindSubject(id, "viewer-s1-@u-hs")
	if err != nil {
		t.Fatalf("the shipped-client path must keep working, got error: %v", err)
	}
	if got != "viewer-s1-@u-hs" {
		t.Fatalf("id changed unexpectedly: %q", got)
	}
}

func TestSubjectBoundRoleWithEmptySubjectIsRefused(t *testing.T) {
	// A legacy token that omitted `sub` has nothing to bind to. Letting it
	// through "because the subject is empty" restores the whole hole.
	for _, role := range []string{roleViewer, rolePublisher} {
		id := switchIdentity{Role: role, Subject: "", Authenticated: true}
		if _, err := bindSubject(id, "anything"); err == nil {
			t.Fatalf("role %q with no subject must be refused, not waved through", role)
		}
	}
}

func TestServerRoleIsNotSubjectBound(t *testing.T) {
	// mm-core's own control-plane token carries sub="mm-core" (switch_client.rs).
	// Binding it to resource ids would break every source/switch/record call.
	id := switchIdentity{Role: roleServer, Subject: "mm-core", Authenticated: true}

	got, err := bindSubject(id, "stream-42")
	if err != nil {
		t.Fatalf("the control plane must not be subject-bound, got error: %v", err)
	}
	if got != "stream-42" {
		t.Fatalf("server role must keep the requested id, got %q", got)
	}
}

func TestUnauthenticatedRequestKeepsLegacyBodyBehaviour(t *testing.T) {
	got, err := bindSubject(switchIdentity{}, "viewer-whatever")
	if err != nil || got != "viewer-whatever" {
		t.Fatalf("an unsecured origin must behave exactly as before: got %q, err %v", got, err)
	}
}

// ─── FR-348: fleet nodes fail closed ─────────────────────────────────────────

func TestFleetNodeRefusesToStartWithoutASecret(t *testing.T) {
	for _, flavor := range []string{nodeFlavorFanout, nodeFlavorEdge, nodeFlavorTranscode} {
		if err := requireAuthSecretOnFleetNode(flavor, ""); err == nil {
			t.Errorf("flavor %q started with no auth secret — a publicly reachable node "+
				"would pass every request through, including source management and recording", flavor)
		}
		if err := requireAuthSecretOnFleetNode(flavor, "s3cret"); err != nil {
			t.Errorf("flavor %q must start WITH a secret, got %v", flavor, err)
		}
	}
}

func TestOriginMayStillRunUnsecured(t *testing.T) {
	// FR-102: a single-host OSS install has mm-core and mm-switch on one box
	// with nothing else able to reach the port. Breaking that would break every
	// self-hosted deployment.
	if err := requireAuthSecretOnFleetNode(nodeFlavorOrigin, ""); err != nil {
		t.Fatalf("the origin must keep working unsecured: %v", err)
	}
}

func TestUnknownFlavorIsAnErrorNotADowngradeToOrigin(t *testing.T) {
	// A typo in a node's cloud-init must not yield a permissive public switch.
	if _, err := resolveNodeFlavor("fanount"); err == nil {
		t.Fatal("a misspelled flavor resolved successfully — it would default to origin " +
			"and start unauthenticated on a public node")
	}
}

func TestFlavorDefaultsToOriginAndIsCaseInsensitive(t *testing.T) {
	got, err := resolveNodeFlavor("")
	if err != nil || got != nodeFlavorOrigin {
		t.Fatalf("empty flavor must default to origin, got %q / %v", got, err)
	}
	if got, err := resolveNodeFlavor("  FanOut "); err != nil || got != nodeFlavorFanout {
		t.Fatalf("flavor parsing must trim and lowercase, got %q / %v", got, err)
	}
}

// ─── FR-349: the viewer list ─────────────────────────────────────────────────

func TestViewerListIsAuthenticatedOnFleetNodesAndOpenOnTheOrigin(t *testing.T) {
	t.Setenv("MM_SWITCH_PRIVATE_VIEWER_LIST", "")

	if !viewerListNeedsAuth(nodeFlavorFanout) {
		t.Error("a fleet node must authenticate GET /api/viewers — the response names every " +
			"viewer, and viewer ids embed Matrix user ids")
	}
	if viewerListNeedsAuth(nodeFlavorOrigin) {
		t.Error("the origin must stay open by default: apps already in both stores poll it " +
			"unauthenticated for the live viewer count")
	}

	t.Setenv("MM_SWITCH_PRIVATE_VIEWER_LIST", "true")
	if !viewerListNeedsAuth(nodeFlavorOrigin) {
		t.Error("an operator who opts in must be able to close the origin's viewer list")
	}
}

// The Go half of the release-safety pair. mm-core's fleet::release_safety_tests
// asserts the two Rust-side defaults; these are the two that live here, and both
// sides passing is what makes the claim true — one side alone is a guess.
//
// A server given no fleet configuration at all must behave exactly as it did
// before any of this landed.
func TestAnUnconfiguredSwitchChangesNoBehaviour(t *testing.T) {
	t.Setenv("MM_SWITCH_NODE_FLAVOR", "")
	t.Setenv("MM_SWITCH_PRIVATE_VIEWER_LIST", "")

	flavor, err := resolveNodeFlavor(os.Getenv(nodeFlavorEnv))
	if err != nil {
		t.Fatalf("an unset flavor must resolve, got %v", err)
	}
	if flavor != nodeFlavorOrigin {
		t.Fatalf("an unset flavor must be origin, got %q — every other flavor refuses "+
			"to boot without a secret, so this would break every single-host install", flavor)
	}
	if err := requireAuthSecretOnFleetNode(flavor, ""); err != nil {
		t.Fatalf("an unconfigured origin must still start: %v", err)
	}
	if viewerListNeedsAuth(flavor) {
		t.Error("an unconfigured origin keeps its open viewer list, because the apps " +
			"already in both stores poll it unauthenticated — closing it is the " +
			"operator's opt-in (the compose template sets it; the code default does not)")
	}
}

// And the deployed configuration: flavor origin with the list closed, which is
// what docker-compose.tmpl.yml sets and what the live server runs.
func TestTheDeployedConfigurationClosesTheViewerList(t *testing.T) {
	t.Setenv("MM_SWITCH_PRIVATE_VIEWER_LIST", "true")
	if !viewerListNeedsAuth(nodeFlavorOrigin) {
		t.Error("MM_SWITCH_PRIVATE_VIEWER_LIST=true must close the list on an origin — " +
			"this is the setting deployed on 2026-09-25 to stop the endpoint " +
			"disclosing which Matrix users are watching which stream")
	}
}
