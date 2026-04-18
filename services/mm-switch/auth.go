package main

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"strconv"
	"strings"
	"time"
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
func validateToken(secret, tokenStr string, allowedRoles []string) (role, subject string, err error) {
	parts := strings.SplitN(tokenStr, ".", 3)
	if len(parts) != 3 {
		return "", "", fmt.Errorf("malformed token: expected 3 parts, got %d", len(parts))
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
		return "", "", fmt.Errorf("invalid signature")
	}

	// Decode payload
	payloadJSON, err := base64.RawURLEncoding.DecodeString(b64Payload)
	if err != nil {
		return "", "", fmt.Errorf("invalid payload encoding: %w", err)
	}

	var payload tokenPayload
	if err := json.Unmarshal(payloadJSON, &payload); err != nil {
		return "", "", fmt.Errorf("invalid payload JSON: %w", err)
	}

	// Check expiry
	if time.Now().Unix() > payload.Exp {
		return "", "", fmt.Errorf("token expired")
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
		return "", "", fmt.Errorf("role %q not allowed (need one of %v)", payload.Role, allowedRoles)
	}

	return payload.Role, payload.Subject, nil
}

// authMiddleware returns HTTP middleware that validates Bearer tokens with HMAC.
// If secret is empty, the middleware is a no-op (dev/backward-compat mode).
func authMiddleware(secret string, allowedRoles ...string) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			// Dev mode: no secret configured, pass through
			if secret == "" {
				next.ServeHTTP(w, r)
				return
			}

			authHeader := r.Header.Get("Authorization")
			if authHeader == "" {
				http.Error(w, `{"error":"missing Authorization header"}`, http.StatusUnauthorized)
				return
			}

			const prefix = "Bearer "
			if !strings.HasPrefix(authHeader, prefix) {
				http.Error(w, `{"error":"Authorization header must use Bearer scheme"}`, http.StatusUnauthorized)
				return
			}

			tokenStr := strings.TrimPrefix(authHeader, prefix)

			role, sub, err := validateToken(secret, tokenStr, allowedRoles)
			if err != nil {
				log.Printf("[auth] rejected %s %s: %v", r.Method, r.URL.Path, err)
				http.Error(w, fmt.Sprintf(`{"error":"%s"}`, err.Error()), http.StatusUnauthorized)
				return
			}

			_ = role
			_ = sub
			next.ServeHTTP(w, r)
		})
	}
}

// wrapAuth is a convenience to apply authMiddleware to a single HandlerFunc.
func wrapAuth(secret string, allowedRoles []string, handler http.HandlerFunc) http.Handler {
	mw := authMiddleware(secret, allowedRoles...)
	return mw(handler)
}
