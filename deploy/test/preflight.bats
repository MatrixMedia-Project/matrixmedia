load helper
setup() { source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/preflight.sh"; }

@test "check_public_ip accepts a public IP" {
  run check_public_ip "203.0.113.5"; [ "$status" -eq 0 ]
}
@test "check_public_ip rejects RFC1918 / CGNAT / loopback" {
  run check_public_ip "192.168.1.10"; [ "$status" -ne 0 ]
  run check_public_ip "10.0.0.4";     [ "$status" -ne 0 ]
  run check_public_ip "172.16.0.9";   [ "$status" -ne 0 ]
  run check_public_ip "100.64.0.1";   [ "$status" -ne 0 ]
  run check_public_ip "127.0.0.1";    [ "$status" -ne 0 ]
}
@test "check_min returns ok when value >= min, fails otherwise" {
  run check_min "RAM" 8 4; [ "$status" -eq 0 ]
  run check_min "RAM" 2 4; [ "$status" -ne 0 ]
}

@test "check_ram accepts what a 4 GB VPS reports after the kernel takes its share" {
  run check_ram 3911212 4; [ "$status" -eq 0 ]   # plain 4 GiB VM: MemTotal ~3.73 GiB
  run check_ram 3570000 4; [ "$status" -eq 0 ]   # 4 GiB VM with a 512 MiB crash kernel: ~3.4 GiB
  run check_ram 8039000 4; [ "$status" -eq 0 ]
}
@test "check_ram rejects a machine smaller than the minimum" {
  run check_ram 2900000 4; [ "$status" -ne 0 ]   # 3 GiB VM: ~2.77 GiB
  [[ "$output" == *"RAM too low"* ]]
  run check_ram 1968000 4; [ "$status" -ne 0 ]   # 2 GiB VM
  [[ "$output" == *"RAM too low"* ]]
}

@test "docker_install_needed is 0 when docker missing" {
  docker() { return 127; }; export -f docker
  PATH=/nonexistent run docker_install_needed
  [ "$status" -eq 0 ]
}

@test "docker_install_needed is 1 when docker + compose v2 present" {
  docker() { [ "$1" = "compose" ] && return 0; return 0; }; export -f docker
  command() { return 0; }; export -f command
  run docker_install_needed
  [ "$status" -ne 0 ]
}

@test "check_all_udp_ranges validates the three published media ranges" {
  ufw() { return 1; }; export -f ufw
  run check_all_udp_ranges; [ "$status" -eq 0 ]
}

# ── preflight() itself, with every host probe stubbed ─────────────────────────
# What is under test is which checks preflight runs and in what order, not the probes.
# `[[ ]] || false`: on bash < 4.1 (macOS /bin/bash) a failing [[ ]] that is not a
# test's last command does not fail the test.
_stub_host() {
  require_cmd() { :; }; docker_install_needed() { return 1; }; docker() { return 0; }
  apt-get() { :; }; ss() { :; }; port_is_ours() { return 1; }
  awk() { echo 8000000; }; df() { :; }          # MemTotal / free disk, whatever the host
  check_ram() { :; }; check_min() { :; }; check_clock_skew() { :; }; check_all_udp_ranges() { :; }
  curl() { echo 203.0.113.5; }                  # the public-IP probe
  uname() { echo x86_64; }
  install_docker() { echo DOCKER-INSTALLED; }
  check_aaaa() { echo "AAAA-CHECKED ${1:-}"; }
}

# install.sh sets MM_DOMAIN only after preflight has run, so the AAAA check, which
# preflight ran only when MM_DOMAIN was set, never ran on any install.
@test "preflight checks the domain's AAAA record when it is handed the domain" {
  _stub_host
  run preflight example.com
  [ "$status" -eq 0 ]
  [[ "$output" == *"AAAA-CHECKED example.com"* ]] || false
}
@test "preflight skips the AAAA check when there is no domain yet (--no-domain)" {
  _stub_host
  run preflight ""
  [ "$status" -eq 0 ]
  [[ "$output" != *"AAAA-CHECKED"* ]] || false
}
@test "install.sh resolves the domain before preflight and hands it over" {
  pf_at="$(grep -n '^preflight "\$DOMAIN"$' "$DEPLOY_ROOT/install.sh" | cut -d: -f1)"
  sub_at="$(grep -n '^if \[ -n "\$SUBDOMAIN" \]; then DOMAIN=' "$DEPLOY_ROOT/install.sh" | cut -d: -f1)"
  [ -n "$pf_at" ] && [ -n "$sub_at" ]
  [ "$sub_at" -lt "$pf_at" ]
}

# First-party images are published for linux/amd64 only (build-images.yml), so on an
# ARM host every one of them failed at pull, minutes into the install.
@test "check_arch accepts x86-64" {
  run check_arch x86_64; [ "$status" -eq 0 ]
  run check_arch amd64;  [ "$status" -eq 0 ]
}
@test "check_arch rejects ARM and says which architecture it found" {
  for a in aarch64 arm64 armv7l; do
    run check_arch "$a"
    [ "$status" -ne 0 ] || { echo "accepted $a"; return 1; }
    [[ "$output" == *"$a"* ]] || { echo "message lacks $a: $output"; return 1; }
    [[ "$output" == *"amd64"* ]] || { echo "message lacks amd64: $output"; return 1; }
  done
}
@test "preflight stops on an ARM host before it installs anything" {
  _stub_host
  uname() { echo aarch64; }
  docker_install_needed() { return 0; }
  run preflight example.com
  [ "$status" -ne 0 ]
  [[ "$output" == *"aarch64"* ]] || false
  [[ "$output" != *"DOCKER-INSTALLED"* ]] || false
}
