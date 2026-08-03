load helper
setup() { source "$DEPLOY_ROOT/lib/common.sh"; }

@test "profiles_from_env returns demo for MM_DEMO_MODE=true" {
  f="$BATS_TEST_TMPDIR/env"; echo "MM_DEMO_MODE=true" > "$f"
  run profiles_from_env "$f"; [ "$output" = "demo" ]
}
@test "profiles_from_env empty for false/absent" {
  f="$BATS_TEST_TMPDIR/env"; echo "MM_DEMO_MODE=false" > "$f"
  run profiles_from_env "$f"; [ -z "$output" ]
  run profiles_from_env "/nonexistent"; [ -z "$output" ]
}
