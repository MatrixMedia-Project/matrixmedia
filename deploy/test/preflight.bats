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
