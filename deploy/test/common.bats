load helper
setup() { source "$DEPLOY_ROOT/lib/common.sh"; }

@test "log prints to stderr with prefix" {
  run bash -c "source '$DEPLOY_ROOT/lib/common.sh'; log hello 2>&1"
  [ "$status" -eq 0 ]; [[ "$output" == *"[mm] hello"* ]]
}
@test "die exits 1 and prints message" {
  run bash -c "source '$DEPLOY_ROOT/lib/common.sh'; die boom 2>&1"
  [ "$status" -eq 1 ]; [[ "$output" == *"boom"* ]]
}
@test "require_cmd succeeds for bash, fails for nonsuchcmd" {
  run bash -c "source '$DEPLOY_ROOT/lib/common.sh'; require_cmd bash"
  [ "$status" -eq 0 ]
  run bash -c "source '$DEPLOY_ROOT/lib/common.sh'; require_cmd nonsuchcmd_zzz"
  [ "$status" -eq 1 ]
}
