package main

import (
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"
)

// Roles carried in a token's payload.
const (
	roleServer    = "server"
	roleViewer    = "viewer"
	rolePublisher = "publisher"
)

// tokenPayload is the JSON structure inside the base64url-encoded first segment.
type tokenPayload struct {
	Role    string `json:"role"`
	Subject string `json:"sub"`
	Exp     int64  `json:"exp"`
}

// generateToken creates a signed HMAC token: {base64url_payload}.{timestamp}.{hmac_hex}
func generateToken(secret, role, subject string, ttlSecs int) string {
	now := time.Now().Unix()
	exp := now + int64(ttlSecs)

	payload := tokenPayload{
		Role:    role,
		Subject: subject,
		Exp:     exp,
	}
	payloadJSON, _ := json.Marshal(payload)
	b64Payload := base64.RawURLEncoding.EncodeToString(payloadJSON)

	timestamp := strconv.FormatInt(now, 10)

	signingInput := b64Payload + "." + timestamp
	mac := hmac.New(sha256.New, []byte(secret))
	mac.Write([]byte(signingInput))
	sig := hex.EncodeToString(mac.Sum(nil))

	return b64Payload + "." + timestamp + "." + sig
}

// validateToken checks signature, expiry, and role membership.
// Returns the role and subject from the payload on success.
//
// The string returned from a non-nil error is logged + sent to the
// caller. The metrics counter (in metrics.go) is incremented from the
// authMiddleware caller using the rejReason* constants so we don't
// rely on parsing error strings.
func validateToken(secret, tokenStr string, allowedRoles []string) (role, subject, rejReason string, err error) {
	parts := strings.SplitN(tokenStr, ".", 3)
	if len(parts) != 3 {
		return "", "", rejReasonMalformed, fmt.Errorf("malformed token: expected 3 parts, got %d", len(parts))
	}

	b64Payload := parts[0]
	timestamp := parts[1]
	providedSig := parts[2]

	// Verify HMAC signature
	signingInput := b64Payload + "." + timestamp
	mac := hmac.New(sha256.New, []byte(secret))
	mac.Write([]byte(signingInput))
	expectedSig := hex.EncodeToString(mac.Sum(nil))

	if !hmac.Equal([]byte(providedSig), []byte(expectedSig)) {
		return "", "", rejReasonInvalidSig, fmt.Errorf("invalid signature")
	}

	// Decode payload
	payloadJSON, err := base64.RawURLEncoding.DecodeString(b64Payload)
	if err != nil {
		return "", "", rejReasonInvalidPayload, fmt.Errorf("invalid payload encoding: %w", err)
	}

	var payload tokenPayload
	if err := json.Unmarshal(payloadJSON, &payload); err != nil {
		return "", "", rejReasonInvalidPayload, fmt.Errorf("invalid payload JSON: %w", err)
	}

	// Check expiry
	if time.Now().Unix() > payload.Exp {
		return "", "", rejReasonExpired, fmt.Errorf("token expired")
	}

	// Check role
	roleAllowed := false
	for _, r := range allowedRoles {
		if payload.Role == r {
			roleAllowed = true
			break
		}
	}
	if !roleAllowed {
		return "", "", rejReasonWrongRole, fmt.Errorf("role %q not allowed (need one of %v)", payload.Role, allowedRoles)
	}

	return payload.Role, payload.Subject, "", nil
}

// authMiddleware returns HTTP middleware that validates Bearer tokens with HMAC
// and attaches the authenticated identity to the request context, so handlers
// can bind what the body claims to who the token says the caller is (FR-347).
//
// If secret is empty the middleware is a no-op and NO identity is attached.
// That path exists for single-host OSS installs where mm-core and mm-switch
// share a host and nothing else can reach the port. A fleet node must never
// run this way — see requireAuthSecretOnFleetNode (FR-348).
func authMiddleware(secret string, allowedRoles ...string) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			// No secret configured: pass through, leaving the request
			// WITHOUT an identity in its context. Only legitimate on a
			// single-host origin (FR-102); requireAuthSecretOnFleetNode
			// refuses to start a fleet node in this state (FR-348).
			if secret == "" {
				next.ServeHTTP(w, r)
				return
			}

			authHeader := r.Header.Get("Authorization")
			if authHeader == "" {
				authRejectionsTotal.WithLabelValues(rejReasonMissingHeader).Inc()
				http.Error(w, `{"error":"missing Authorization header"}`, http.StatusUnauthorized)
				return
			}

			const prefix = "Bearer "
			if !strings.HasPrefix(authHeader, prefix) {
				authRejectionsTotal.WithLabelValues(rejReasonWrongScheme).Inc()
				http.Error(w, `{"error":"Authorization header must use Bearer scheme"}`, http.StatusUnauthorized)
				return
			}

			tokenStr := strings.TrimPrefix(authHeader, prefix)

			role, sub, rejReason, err := validateToken(secret, tokenStr, allowedRoles)
			if err != nil {
				if rejReason != "" {
					authRejectionsTotal.WithLabelValues(rejReason).Inc()
				}
				log.Printf("[auth] rejected %s %s: %v", r.Method, r.URL.Path, err)
				http.Error(w, fmt.Sprintf(`{"error":"%s"}`, err.Error()), http.StatusUnauthorized)
				return
			}

			next.ServeHTTP(w, withIdentity(r, switchIdentity{
				Role:          role,
				Subject:       sub,
				Authenticated: true,
			}))
		})
	}
}

// wrapAuth is a convenience to apply authMiddleware to a single HandlerFunc.
func wrapAuth(secret string, allowedRoles []string, handler http.HandlerFunc) http.Handler {
	mw := authMiddleware(secret, allowedRoles...)
	return mw(handler)
}

// ─── Caller identity (FR-347) ────────────────────────────────────────────────

// switchIdentity is who the caller is, per their token — not per their request
// body. The distinction is the whole point: before this existed, authMiddleware
// validated the token and then threw the role and subject away with
// `_ = role; _ = sub`, so any holder of any viewer token could send any viewer
// id it liked. Authentication without binding is not authorisation.
type switchIdentity struct {
	Role    string
	Subject string
	// Authenticated is false when no secret was configured and the middleware
	// passed the request through. Handlers MUST treat that as "no identity",
	// never as "trusted".
	Authenticated bool
}

type identityCtxKey struct{}

func withIdentity(r *http.Request, id switchIdentity) *http.Request {
	return r.WithContext(context.WithValue(r.Context(), identityCtxKey{}, id))
}

// identityOf returns the authenticated caller, or the zero value when the
// request never passed a secret-enforcing middleware.
func identityOf(r *http.Request) switchIdentity {
	if id, ok := r.Context().Value(identityCtxKey{}).(switchIdentity); ok {
		return id
	}
	return switchIdentity{}
}

// bindSubject resolves the resource id a request may act on, given what the body
// asked for and who the caller is (FR-347).
//
// Rules, in order:
//
//   - No identity (no secret configured): the body wins, as it always did.
//   - Role `server`: the control plane. mm-core's own token carries
//     sub="mm-core", so binding it to a resource id would be meaningless. The
//     body wins. NOTE for the FR-350 proxy: when mm-core starts forwarding
//     viewer offers on a client's behalf it MUST mint a per-request token with
//     role `viewer` and sub = the server-derived viewer id, not reuse its
//     server token — otherwise the proxy reintroduces exactly this hole.
//   - Any other role (`viewer`, `publisher`): the token is subject-bound. An
//     empty body id adopts the subject; a mismatched one is refused. A
//     subject-bound role with an EMPTY subject is refused outright — there is
//     nothing to bind to, and silently allowing it would restore the hole for
//     any legacy token that omitted `sub`.
func bindSubject(id switchIdentity, requested string) (resolved string, err error) {
	if !id.Authenticated || id.Role == roleServer {
		return requested, nil
	}
	if id.Subject == "" {
		return "", fmt.Errorf("token for role %q carries no subject, so it cannot be bound to a resource id", id.Role)
	}
	if requested == "" {
		return id.Subject, nil
	}
	if requested != id.Subject {
		return "", fmt.Errorf("id %q does not match the authenticated subject", requested)
	}
	return requested, nil
}

// ─── Node flavor and the fail-closed guard (FR-348) ──────────────────────────

const nodeFlavorEnv = "MM_SWITCH_NODE_FLAVOR"

// The flavors a switch process can run as. Only `origin` may run unsecured.
const (
	nodeFlavorOrigin    = "origin"
	nodeFlavorFanout    = "fanout"
	nodeFlavorEdge      = "edge"
	nodeFlavorTranscode = "transcode"
)

var knownNodeFlavors = []string{
	nodeFlavorOrigin, nodeFlavorFanout, nodeFlavorEdge, nodeFlavorTranscode,
}

// resolveNodeFlavor reads MM_SWITCH_NODE_FLAVOR, defaulting to `origin`.
//
// An unrecognised value is an ERROR rather than a fallback to origin. Falling
// back would mean a typo in the node's cloud-init ("fanount") produces a
// publicly reachable switch with authentication disabled — the exact failure
// FR-348 exists to prevent, arriving through the one path nobody tests.
func resolveNodeFlavor(raw string) (string, error) {
	flavor := strings.ToLower(strings.TrimSpace(raw))
	if flavor == "" {
		return nodeFlavorOrigin, nil
	}
	for _, known := range knownNodeFlavors {
		if flavor == known {
			return flavor, nil
		}
	}
	return "", fmt.Errorf("unknown %s %q (expected one of %s)",
		nodeFlavorEnv, raw, strings.Join(knownNodeFlavors, ", "))
}

// requireAuthSecretOnFleetNode returns an error when a non-origin node has no
// auth secret (FR-348). A fleet node is reachable from the public internet and
// is reached only by mm-core, so there is no install in which running it
// unauthenticated is correct.
func requireAuthSecretOnFleetNode(flavor, secret string) error {
	if flavor != nodeFlavorOrigin && secret == "" {
		return fmt.Errorf(
			"node flavor %q requires MM_SWITCH_AUTH_SECRET: a fleet node is publicly reachable "+
				"and is only ever called by mm-core, so passing every request through unauthenticated "+
				"would expose source management, switching and recording to anyone who finds the port",
			flavor)
	}
	return nil
}

// viewerListNeedsAuth decides whether GET /api/viewers is authenticated
// (FR-349).
//
// Fleet nodes: always. The response lists every viewer id on the node, and
// viewer ids are derived from Matrix user ids (`viewer-{stream}-{@user-hs}` in
// mm-core), so an open endpoint on a public node discloses who is watching what.
//
// Origin: open by default, because apps already in both stores poll it
// unauthenticated for the live viewer count (FR-346 — shipped clients must keep
// working). MM_SWITCH_PRIVATE_VIEWER_LIST=true closes it for operators who would
// rather lose that count than publish the audience list.
func viewerListNeedsAuth(flavor string) bool {
	if flavor != nodeFlavorOrigin {
		return true
	}
	return strings.EqualFold(strings.TrimSpace(os.Getenv("MM_SWITCH_PRIVATE_VIEWER_LIST")), "true")
}

// pinViewerSource decides whether a viewer offer may attach to `sourceID` (FR-347f).
//
// mm-core mints a viewer token at /join with sub = "viewer-{stream_id}-{user}" and gives
// the viewer "stream-{stream_id}" to watch. bindSubject already ties the body's viewer id
// to the token, but the source was taken from the body as-is — so a token from a free
// stream opened any paid stream's source. Rules:
//
//   - No identity (no secret configured) or role `server`: not pinned, as before.
//   - No `source_id`: nothing to pin (the control plane attaches the viewer later).
//   - Otherwise the source must be "stream-X" with the subject starting "viewer-X-".
//
// The rule needs no knowledge of the stream id's format. The subject is signed by
// mm-core, so the only "X" an attacker can make match besides the full id are fragments
// of their own stream's id ("viewer-0a1b2c3d-…" also starts with "viewer-0a1b2c3d-"), and
// no source is ever named after a fragment: stream sources are created only through a
// publish offer bound to "stream-{full id}", or by the control plane. Ad and file
// sources are attached by the control plane (POST /api/switch), never by a viewer offer.
func pinViewerSource(id switchIdentity, sourceID string) error {
	if !id.Authenticated || id.Role == roleServer || sourceID == "" {
		return nil
	}
	stream, ok := strings.CutPrefix(sourceID, "stream-")
	if !ok || stream == "" {
		return fmt.Errorf("a viewer token may only open a stream source, not %q", sourceID)
	}
	if !strings.HasPrefix(id.Subject, "viewer-"+stream+"-") {
		return fmt.Errorf("source %q is not the stream this viewer token was issued for", sourceID)
	}
	return nil
}
