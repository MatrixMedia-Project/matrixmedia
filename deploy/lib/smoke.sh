# shellcheck shell=bash
# Post-deploy self-smoke against the running stack.

# probe_http URL EXPECTED_CODE [HEADER] -- 0 if the response code matches.
probe_http() {
  local url="$1" want="$2" hdr="${3:-}" code
  if [ -n "$hdr" ]; then
    code="$(curl -ksS -o /dev/null -w '%{http_code}' -H "$hdr" "$url")"
  else
    code="$(curl -ksS -o /dev/null -w '%{http_code}' "$url")"
  fi
  [ "$code" = "$want" ] || { warn "smoke: $url returned $code, expected $want"; return 1; }
}

# body_looks_like_spa BODY -- 0 if BODY is a real SPA shell, not an error page.
#
# THE reason smoke was useless: it only ever checked status codes, and it never probed the
# SPAs at all. An install where nginx served an EMPTY html root returned 200 on / and
# 404 on every SPA — and smoke passed, reported success, and handed the operator a
# deployment whose entire UI was broken. A status code is not evidence that a UI exists.
body_looks_like_spa() {
  local body="$1"
  # A built Vite/React SPA shell always has a mount point and a module script. An nginx
  # 404, a Traefik error page and an empty directory listing have neither.
  printf '%s' "$body" | grep -qiE '<div id="root"|<script[^>]+type="module"|<script[^>]+src="[^"]*assets/'
}

# probe_spa URL -- 0 if URL serves a real SPA (200 AND a plausible shell body).
probe_spa() {
  local url="$1" code body tmp
  tmp="$(mktemp)"
  code="$(curl -ksS -o "$tmp" -w '%{http_code}' "$url" 2>/dev/null || echo 000)"
  body="$(cat "$tmp")"; rm -f "$tmp"
  [ "$code" = "200" ] || { warn "smoke: SPA $url returned $code — the web UI is not being served"; return 1; }
  body_looks_like_spa "$body" || { warn "smoke: SPA $url returned 200 but the body is not an app shell — nginx is serving something, but not the UI"; return 1; }
}

# code_is_gated CODE -- 0 if CODE means "correctly NOT public".
#
# mm-switch's /metrics and /health must not be reachable from the internet: they leak
# stream/viewer topology and give an attacker a free liveness oracle. Traefik is supposed
# to gate them. A 200 here means the gate is missing — and no existing probe would notice,
# because nothing ever looked.
code_is_gated() {
  case "$1" in 401|403|404) return 0 ;; *) return 1 ;; esac
}

# probe_gated URL -- 0 if URL is NOT publicly readable.
probe_gated() {
  local url="$1" code
  code="$(curl -ksS -o /dev/null -w '%{http_code}' "$url" 2>/dev/null || echo 000)"
  code_is_gated "$code" || { warn "smoke: $url is PUBLIC (returned $code) — it must be gated"; return 1; }
}

# federation_report_ok JSON -- 0 if the federation tester says we federate.
federation_report_ok() {
  printf '%s' "$1" | grep -q '"FederationOK"[[:space:]]*:[[:space:]]*true'
}

# probe_federation DOMAIN -- ask matrix.org's federation tester whether we actually federate.
#
# Advisory (never fatal): it depends on a third-party service and on DNS having propagated.
# But it is the only check that answers the question an operator actually cares about —
# "can the rest of Matrix talk to me?" — which every local probe can pass while the answer
# is no.
probe_federation() {
  local domain="$1" body
  body="$(curl -fsS --max-time 20 "https://federationtester.matrix.org/api/report?server_name=${domain}" 2>/dev/null)" || {
    warn "smoke: could not reach the federation tester (advisory) — verify manually at https://federationtester.matrix.org/#${domain}"
    return 0
  }
  federation_report_ok "$body" ||     warn "smoke: federation tester says this server does NOT federate — check DNS, .well-known and port 8448. https://federationtester.matrix.org/#${domain}"
  return 0
}

# probe_cert HOST -- verify the served certificate actually validates (NO -k).
# Every other probe deliberately tolerates self-signed certs so smoke can run in
# temp mode; this one exists so a FAILED ACME issuance cannot masquerade as a
# working install. Skipped when MM_TEMP_MODE=true (self-signed is the contract).
probe_cert() {
  local host="$1"
  curl -sS --max-time 15 -o /dev/null "https://$host/" \
    || { warn "smoke: certificate for $host does not validate — ACME issuance failed (check Traefik logs: mmctl logs traefik)"; return 1; }
}

# self_smoke DOMAIN ADMIN_TOKEN -- run all probes; die if any critical one fails.
self_smoke() {
  local domain="$1" admin_token="$2" fail=0
  probe_http "https://matrix.$domain/_matrix/client/versions" 200 || fail=1
  [ "${MM_TEMP_MODE:-false}" = "true" ] || probe_cert "matrix.$domain" || fail=1
  probe_http "https://matrix.$domain/mm/v1/announcements/active" 200 || fail=1
  probe_http "https://matrix.$domain/_mm/admin/v1/health" 200 "Authorization: Bearer $admin_token" || fail=1
  probe_http "https://$domain/.well-known/matrix/server" 200 || fail=1
  probe_http "https://$domain/.well-known/matrix/client" 200 || fail=1
  probe_http "https://call.$domain/" 200 || fail=1
  probe_http "https://matrix.$domain/_matrix/federation/v1/version" 200 || fail=1

  # The web UI. Nothing used to probe this at all, which is how an install whose entire
  # dashboard/viewer 404'd could pass smoke and be reported as a success.
  probe_spa "https://matrix.$domain/_mm/dashboard/" || fail=1
  probe_spa "https://matrix.$domain/_mm/viewer/"    || fail=1

  # mm-switch internals must NOT be on the public internet.
  probe_gated "https://matrix.$domain/_mm/switch/metrics" || fail=1
  probe_gated "https://matrix.$domain/_mm/switch/health"  || fail=1

  # Advisory: never fails the install, but it is the only probe that answers "can the rest
  # of Matrix actually reach me?"
  probe_federation "$domain"

  [ "$fail" -eq 0 ] || die "self-smoke failed (see warnings above); run: mmctl logs"
  log "self-smoke passed"
}
