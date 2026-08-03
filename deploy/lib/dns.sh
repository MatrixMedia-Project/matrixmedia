# shellcheck shell=bash
# TLS-mode selection + DNS resolution gate for the installer.

# sslip_domain IP -- throwaway wildcard DNS name for the instant/no-domain mode.
# sslip.io resolves A-B-C-D.sslip.io (and any subdomain of it) to A.B.C.D, so the
# apex/matrix./call. hostname structure works with zero DNS setup. Let's Encrypt
# per-domain rate limits on sslip.io are shared globally and usually exhausted:
# temp mode therefore expects a self-signed cert and is labeled THROWAWAY.
sslip_domain() { echo "${1//./-}.sslip.io"; }

choose_tls_mode() { [ -n "$1" ] && echo dns01 || echo http01; }

verify_resolves() { local host="$1" want="$2" got; got="$(dig +short A "$host" | tail -n1)"; [ "$got" = "$want" ]; }

# dns_gate DOMAIN PUBLIC_IP [DNS_TOKEN]
#   token present -> DNS-01 (no resolve poll needed; cert issued via DNS challenge).
#   no token      -> HTTP-01: block until apex/matrix/call resolve to PUBLIC_IP.
dns_gate() {
  local domain="$1" ip="$2" token="${3:-}"
  MM_TLS_MODE="$(choose_tls_mode "$token")"; export MM_TLS_MODE
  [ "$MM_TLS_MODE" = "dns01" ] && { log "TLS: DNS-01 wildcard (*.$domain + $domain)"; return 0; }
  require_cmd dig
  local tries=0 host
  for host in "$domain" "matrix.$domain" "call.$domain"; do
    until verify_resolves "$host" "$ip"; do
      tries=$((tries+1))
      [ "$tries" -gt 20 ] && die "DNS for $host does not point to $ip after ~10m; create the A record and retry"
      warn "waiting for $host -> $ip (attempt $tries)"; sleep 30
    done
    log "DNS OK: $host -> $ip"
  done
}
