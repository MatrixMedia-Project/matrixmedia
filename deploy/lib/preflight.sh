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
    ss -ltn "( sport = :$p )" 2>/dev/null | grep -q LISTEN && die "port $p already in use"
  done
  log "preflight OK: ${mem_gb}GB RAM, ${disk_gb}GB disk, public IP $PUBLIC_IP"
}
