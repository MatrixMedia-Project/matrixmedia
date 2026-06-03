load helper

@test "install.sh --dry-run sources all libs and exits 0" {
  run bash "$DEPLOY_ROOT/install.sh" --dry-run --domain ci.example.com --email a@b.co
  [ "$status" -eq 0 ]
  [[ "$output" == *"dry-run OK"* ]]
}

@test "install.sh --help exits 0 and prints usage" {
  run bash "$DEPLOY_ROOT/install.sh" --help
  [ "$status" -eq 0 ]
  [[ "$output" == *"usage:"* ]]
}

@test "install.sh rejects an unknown arg" {
  run bash "$DEPLOY_ROOT/install.sh" --dry-run --bogus
  [ "$status" -ne 0 ]
}

@test "install.sh --dry-run carries the demo flag" {
  run bash "$DEPLOY_ROOT/install.sh" --dry-run --domain ci.example.com --email a@b.co --demo
  [ "$status" -eq 0 ]
  [[ "$output" == *"demo=true"* ]]
}
