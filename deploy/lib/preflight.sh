# shellcheck shell=bash
# Pure helpers (unit-tested) + preflight() orchestrator (runs on the target VPS).

# check_public_ip IP -- 0 if a routable public IPv4, 1 for RFC1918/CGNAT/loopback/garbage.
check_public_ip() {
  local ip="$1"
  case "$ip" in
    10.*|192.168.*|127.*|169.254.*) return 1 ;;
    172.1[6-9].*|172.2[0-9].*|172.3[0-1].*) return 1 ;;
    100.6[4-9].*|100.[7-9][0-9].*|100.1[01][0-9].*|100.12[0-7].*) return 1 ;;  # CGNAT 100.64/10
    *.*.*.*) return 0 ;;
    *) return 1 ;;
  esac
}

# check_min LABEL HAVE NEED -- 0 if HAVE>=NEED, else warn + 1.
check_min() { local label="$1" have="$2" need="$3"; [ "$have" -ge "$need" ] || { warn "$label too low: have $have, need $need"; return 1; }; }

# ── Pure helpers (unit-tested) ────────────────────────────────────────────────

# clock_skew_secs REMOTE_EPOCH LOCAL_EPOCH -- absolute difference in seconds.
#
# Synapse federation signs events with a timestamp and remote servers reject anything too
# far out. A VPS with a dead/absent NTP client silently federates with nobody, and the
# operator sees "my server works but nobody can talk to me" with no error pointing at the
# clock.
clock_skew_secs() {
  local a="$1"
  local b="$2"
  # Separate statements deliberately: under `set -u`, referencing `a` in an arithmetic
  # expansion within the SAME `local` declaration is an unbound-variable error.
  local d=$(( a - b ))
  [ "$d" -lt 0 ] && d=$(( -d ))
  echo "$d"
}

# check_clock_skew MAX_SECS -- 0 if the host clock is within MAX_SECS of a public source.
check_clock_skew() {
  local max="${1:-30}" remote local_e skew
  # HTTP Date from a stable host. Not NTP-accurate, but a minute-scale drift is the thing
  # that breaks federation, and this catches it with no extra dependency.
  remote="$(curl -fsSI https://matrix.org 2>/dev/null | awk -F': ' 'tolower($1)=="date"{print $2}' | tr -d '\r')"
  [ -n "$remote" ] || { warn "clock: could not reach a time source; skipping skew check"; return 0; }
  remote="$(date -u -d "$remote" +%s 2>/dev/null || date -u -jf '%a, %d %b %Y %T %Z' "$remote" +%s 2>/dev/null)"
  [ -n "$remote" ] || { warn "clock: could not parse a remote date; skipping skew check"; return 0; }
  local_e="$(date -u +%s)"
  skew="$(clock_skew_secs "$remote" "$local_e")"
  [ "$skew" -le "$max" ] || {
    warn "clock skew ${skew}s (max ${max}s) — Synapse federation will reject your events. Fix NTP: systemctl enable --now systemd-timesyncd"
    return 1
  }
}

# udp_range_valid START END -- 0 if a usable, correctly-ordered UDP port range.
#
# LiveKit and mm-switch carry ALL media over UDP in this range. If it is malformed, or the
# host firewall does not pass it, every call and every stream connects and then plays
# nothing — audio/video simply never arrives, while HTTP health checks stay green. It is
# the single most common self-host failure and the hardest to diagnose from the symptom.
udp_range_valid() {
  local start="$1" end="$2"
  case "$start$end" in *[!0-9]*|'') return 1 ;; esac
  [ "$start" -ge 1024 ] && [ "$end" -le 65535 ] && [ "$start" -lt "$end" ]
}

# check_udp_range START END -- validate the range and warn if a firewall obviously blocks it.
check_udp_range() {
  local start="$1" end="$2"
  udp_range_valid "$start" "$end" || die "invalid media UDP range ${start}-${end} (need 1024 <= start < end <= 65535)"
  # ufw is the default on Ubuntu, which is what most operators run.
  if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -qi '^Status: active'; then
    ufw status 2>/dev/null | grep -q "${start}:${end}/udp" ||       warn "ufw is active but does not allow ${start}:${end}/udp — calls and streams will connect and then carry NO media. Fix: ufw allow ${start}:${end}/udp"
  fi
}

# aaaa_is_a_trap HAS_AAAA HAS_PUBLIC_V6 -- 0 (a problem) when the domain advertises AAAA
# but this host has no public IPv6.
#
# Clients and federating servers prefer AAAA. If DNS advertises one that does not lead
# here, they connect to nothing — and the operator, testing over IPv4, sees a working
# server. Silent, and invisible from the box.
aaaa_is_a_trap() {
  local has_aaaa="$1" has_v6="$2"
  [ "$has_aaaa" = "yes" ] && [ "$has_v6" != "yes" ]
}

# check_aaaa DOMAIN -- warn when DNS advertises IPv6 this host cannot serve.
check_aaaa() {
  local domain="$1" has_aaaa=no has_v6=no
  command -v getent >/dev/null 2>&1 || return 0
  getent ahostsv6 "$domain" 2>/dev/null | grep -q . && has_aaaa=yes
  curl -fsS6 --max-time 5 https://api64.ipify.org >/dev/null 2>&1 && has_v6=yes
  if aaaa_is_a_trap "$has_aaaa" "$has_v6"; then
    warn "$domain has an AAAA record but this host has no public IPv6 — v6-preferring clients and federating servers will reach nothing. Remove the AAAA, or give the host working IPv6."
  fi
}

# port_is_ours PORT -- 0 if the listener on PORT belongs to OUR compose project.
#
# Re-running the installer on a host that already runs MatrixMedia must CONVERGE, not
# abort. The old check died on "port 80 already in use" — which is true, and it is our own
# Traefik. So the installer could be run exactly once, ever, and never again to repair a
# half-finished install.
port_is_ours() {
  local port="$1"
  docker ps --filter 'label=com.docker.compose.project=matrixmedia' --format '{{.Ports}}' 2>/dev/null     | grep -q ":${port}->"
}

# preflight -- full host validation; sets/export PUBLIC_IP; die on hard failures.
preflight() {
  require_cmd docker; require_cmd openssl; require_cmd curl
  command -v envsubst >/dev/null || { log "installing gettext-base/jq"; apt-get update -qq && apt-get install -y -qq gettext-base jq; }
  docker compose version >/dev/null 2>&1 || die "docker compose v2 plugin required"
  local mem_gb disk_gb
  mem_gb=$(( $(awk '/MemTotal/{print $2}' /proc/meminfo) / 1024 / 1024 ))
  disk_gb=$(( $(df -Pk /opt 2>/dev/null | awk 'NR==2{print $4}') / 1024 / 1024 ))
  check_min "RAM(GB)" "$mem_gb" 4 || die "need >=4GB RAM"
  check_min "Disk(GB)" "$disk_gb" 20 || die "need >=20GB free on /opt"
  PUBLIC_IP="$(curl -fsS4 https://ifconfig.me || curl -fsS4 https://api.ipify.org)"
  check_public_ip "$PUBLIC_IP" || die "host has no public IPv4 ($PUBLIC_IP); calls need a public IP"
  export PUBLIC_IP
  local p
  for p in 80 443; do
    if ss -ltn "( sport = :$p )" 2>/dev/null | grep -q LISTEN; then
      # Converge, don't abort: our own Traefik holding :80 is the expected state on a
      # re-run (repairing a half-finished install), not a conflict.
      if port_is_ours "$p"; then
        log "port $p is held by our own stack — converging"
      else
        die "port $p already in use by something that is not MatrixMedia"
      fi
    fi
  done

  check_clock_skew 30 || true          # warns; a skewed clock breaks federation silently
  check_udp_range "${MM_UDP_START:-50000}" "${MM_UDP_END:-50200}"
  [ -n "${MM_DOMAIN:-}" ] && check_aaaa "$MM_DOMAIN"

  log "preflight OK: ${mem_gb}GB RAM, ${disk_gb}GB disk, public IP $PUBLIC_IP"
}
