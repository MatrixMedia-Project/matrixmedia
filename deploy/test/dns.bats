load helper
setup() {
  setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/dns.sh"
  mkdir -p "$MM_ROOT/bin"; PATH="$MM_ROOT/bin:$PATH"
}
teardown() { teardown_tmp; }
_mock_dig() { printf '#!/usr/bin/env bash\necho "%s"\n' "$1" > "$MM_ROOT/bin/dig"; chmod +x "$MM_ROOT/bin/dig"; }

@test "choose_tls_mode is always http01 until DNS-01 exists" {
  run choose_tls_mode ""; [ "$output" = "http01" ]
  run choose_tls_mode "some-token"; [ "$output" = "http01" ]
}
@test "verify_resolves passes when dig returns the host IP" {
  _mock_dig "203.0.113.5"
  run verify_resolves "matrix.example.com" "203.0.113.5"; [ "$status" -eq 0 ]
}
@test "verify_resolves fails when dig returns a different IP" {
  _mock_dig "198.51.100.9"
  run verify_resolves "matrix.example.com" "203.0.113.5"; [ "$status" -ne 0 ]
}
@test "sslip_domain converts IP to sslip host" {
  run sslip_domain "203.0.113.10"; [ "$output" = "203-0-113-10.sslip.io" ]
}
