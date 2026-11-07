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
