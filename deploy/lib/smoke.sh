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

# self_smoke DOMAIN ADMIN_TOKEN -- run all probes; die if any critical one fails.
self_smoke() {
  local domain="$1" admin_token="$2" fail=0
  probe_http "https://matrix.$domain/_matrix/client/versions" 200 || fail=1
  probe_http "https://matrix.$domain/mm/v1/announcements/active" 200 || fail=1
  probe_http "https://matrix.$domain/_mm/admin/v1/health" 200 "Authorization: Bearer $admin_token" || fail=1
  probe_http "https://$domain/.well-known/matrix/server" 200 || fail=1
  probe_http "https://$domain/.well-known/matrix/client" 200 || fail=1
  probe_http "https://call.$domain/" 200 || fail=1
  probe_http "https://matrix.$domain/_matrix/federation/v1/version" 200 || fail=1
  [ "$fail" -eq 0 ] || die "self-smoke failed (see warnings above); run: mmctl logs"
  log "self-smoke passed"
}
