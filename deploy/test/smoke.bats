load helper
setup() {
  setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/smoke.sh"
  mkdir -p "$MM_ROOT/bin"; PATH="$MM_ROOT/bin:$PATH"
}
teardown() { teardown_tmp; }
# mock curl: prints 200 unless the URL contains FAILME -> 502
_mock_curl_ok() {
  cat > "$MM_ROOT/bin/curl" <<'EOF'
#!/usr/bin/env bash
url="${!#}"; case "$url" in *FAILME*) echo 502;; *) echo 200;; esac
EOF
  chmod +x "$MM_ROOT/bin/curl"
}

@test "probe_http passes on 200" {
  _mock_curl_ok; run probe_http "https://x/ok" 200; [ "$status" -eq 0 ]
}
@test "probe_http fails on mismatch" {
  _mock_curl_ok; run probe_http "https://x/FAILME" 200; [ "$status" -ne 0 ]
}
@test "probe_http supports a header arg" {
  _mock_curl_ok; run probe_http "https://x/ok" 200 "Authorization: Bearer t"; [ "$status" -eq 0 ]
}

_mock_curl_exit() {   # _mock_curl_exit CODE -- stub curl that always exits CODE
  cat > "$MM_ROOT/bin/curl" <<EOF
#!/usr/bin/env bash
exit $1
EOF
  chmod +x "$MM_ROOT/bin/curl"
}

@test "probe_cert fails on invalid cert, passes when curl trusts it" {
  _mock_curl_exit 60     # 60 = SSL cert problem
  run probe_cert "matrix.example.com"; [ "$status" -ne 0 ]
  _mock_curl_exit 0
  run probe_cert "matrix.example.com"; [ "$status" -eq 0 ]
}
