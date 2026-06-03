load helper

@test "mmctl with no args prints usage and exits non-zero" {
  run bash "$DEPLOY_ROOT/mmctl"
  [ "$status" -ne 0 ]
  [[ "$output" == *"usage:"* ]]
}

@test "mmctl with an unknown command prints usage and exits non-zero" {
  run bash "$DEPLOY_ROOT/mmctl" flibbertigibbet
  [ "$status" -ne 0 ]
  [[ "$output" == *"usage:"* ]]
}

@test "mmctl version prints a version line" {
  run bash "$DEPLOY_ROOT/mmctl" version
  [ "$status" -eq 0 ]
  [[ "$output" == mmctl\ * ]]
}
