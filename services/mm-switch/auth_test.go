package main

import (
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

func TestGenerateAndValidateToken(t *testing.T) {
	secret := "test-secret-key"

	token := generateToken(secret, "server", "mm-core", 60)
	if token == "" {
		t.Fatal("generateToken returned empty string")
	}

	role, sub, _, err := validateToken(secret, token, []string{"server"})
	if err != nil {
		t.Fatalf("validateToken failed: %v", err)
	}
	if role != "server" {
		t.Errorf("expected role=server, got %q", role)
	}
	if sub != "mm-core" {
		t.Errorf("expected sub=mm-core, got %q", sub)
	}
}

func TestValidateToken_WrongSecret(t *testing.T) {
	token := generateToken("secret-a", "server", "mm-core", 60)
	_, _, _, err := validateToken("secret-b", token, []string{"server"})
	if err == nil {
		t.Fatal("expected error for wrong secret")
	}
}

func TestValidateToken_Expired(t *testing.T) {
	// TTL of -10 means it expired 10 seconds ago
	token := generateToken("secret", "server", "mm-core", -10)
	_, _, _, err := validateToken("secret", token, []string{"server"})
	if err == nil {
		t.Fatal("expected error for expired token")
	}
}

func TestValidateToken_WrongRole(t *testing.T) {
	token := generateToken("secret", "viewer", "viewer-1", 60)
	_, _, _, err := validateToken("secret", token, []string{"server", "publisher"})
	if err == nil {
		t.Fatal("expected error for wrong role")
	}
}

func TestValidateToken_MalformedToken(t *testing.T) {
	_, _, _, err := validateToken("secret", "not-a-valid-token", []string{"server"})
	if err == nil {
		t.Fatal("expected error for malformed token")
	}
}

func TestAuthMiddleware_NoSecret_PassThrough(t *testing.T) {
	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
	})

	mw := authMiddleware("", "server")
	wrapped := mw(handler)

	req := httptest.NewRequest("GET", "/test", nil)
	rec := httptest.NewRecorder()
	wrapped.ServeHTTP(rec, req)

	if rec.Code != 200 {
		t.Errorf("expected 200 in dev mode, got %d", rec.Code)
	}
}

func TestAuthMiddleware_MissingHeader(t *testing.T) {
	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
	})

	mw := authMiddleware("secret", "server")
	wrapped := mw(handler)

	req := httptest.NewRequest("GET", "/test", nil)
	rec := httptest.NewRecorder()
	wrapped.ServeHTTP(rec, req)

	if rec.Code != 401 {
		t.Errorf("expected 401 for missing header, got %d", rec.Code)
	}
}

func TestAuthMiddleware_ValidToken(t *testing.T) {
	secret := "my-secret"
	token := generateToken(secret, "publisher", "streamer-1", 60)

	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
	})

	mw := authMiddleware(secret, "server", "publisher")
	wrapped := mw(handler)

	req := httptest.NewRequest("POST", "/api/publish/offer", nil)
	req.Header.Set("Authorization", "Bearer "+token)
	rec := httptest.NewRecorder()
	wrapped.ServeHTTP(rec, req)

	if rec.Code != 200 {
		t.Errorf("expected 200 for valid token, got %d", rec.Code)
	}
}

func TestAuthMiddleware_InvalidToken(t *testing.T) {
	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
	})

	mw := authMiddleware("secret", "server")
	wrapped := mw(handler)

	req := httptest.NewRequest("GET", "/test", nil)
	req.Header.Set("Authorization", "Bearer garbage.token.here")
	rec := httptest.NewRecorder()
	wrapped.ServeHTTP(rec, req)

	if rec.Code != 401 {
		t.Errorf("expected 401 for invalid token, got %d", rec.Code)
	}
}

func TestWrapAuth(t *testing.T) {
	secret := "wrap-test"
	token := generateToken(secret, "viewer", "v-42", 60)

	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
		w.Write([]byte("ok"))
	})

	wrapped := wrapAuth(secret, []string{"server", "viewer"}, handler)

	// With valid token
	req := httptest.NewRequest("POST", "/api/viewers/offer", nil)
	req.Header.Set("Authorization", "Bearer "+token)
	rec := httptest.NewRecorder()
	wrapped.ServeHTTP(rec, req)
	if rec.Code != 200 {
		t.Errorf("expected 200, got %d", rec.Code)
	}

	// Without token
	req2 := httptest.NewRequest("POST", "/api/viewers/offer", nil)
	rec2 := httptest.NewRecorder()
	wrapped.ServeHTTP(rec2, req2)
	if rec2.Code != 401 {
		t.Errorf("expected 401 without token, got %d", rec2.Code)
	}
}

// Suppress unused import warning for time
var _ = time.Now
