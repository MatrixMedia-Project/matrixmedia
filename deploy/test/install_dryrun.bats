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

@test "install.sh accepts owner --admin-user / --admin-pass flags" {
  run bash "$DEPLOY_ROOT/install.sh" --dry-run --domain ci.example.com --email a@b.co \
    --admin-user alice --admin-pass s3cret
  [ "$status" -eq 0 ]
}

@test "install.sh --help documents the owner admin flags" {
  run bash "$DEPLOY_ROOT/install.sh" --help
  [ "$status" -eq 0 ]
  [[ "$output" == *"--admin-user"* ]]
}

@test "no TTY on stdin forces non-interactive (curl|bash guard)" {
  # curl|bash leaves stdin attached to the script text, not a keyboard: an
  # interactive `read` for the admin prompts would silently consume our own
  # source lines instead of prompting. `echo |` gives the subshell a
  # non-terminal stdin, reproducing that pipe shape without a full install.
  run bash -c "echo | bash '$DEPLOY_ROOT/install.sh' --dry-run --no-domain 2>&1"
  [ "$status" -eq 0 ]
}

@test "the pipe-safe NONINT guard line is present in install.sh" {
  # Pin the exact fixed form so a future edit can't silently drop the guard
  # and reintroduce the curl|bash prompt landmine.
  grep -qF '[ -t 0 ] || NONINT=1' "$DEPLOY_ROOT/install.sh"
}

@test "install.sh --dry-run --no-domain --domain x.com rejects the conflicting combo" {
  # --no-domain synthesizes its own DOMAIN; --domain says otherwise. This must be
  # caught even under --dry-run — the conflict check runs before the dry-run
  # early-exit specifically so a dry run surfaces argv mistakes, not just a
  # "libs sourced OK" false positive.
  run bash "$DEPLOY_ROOT/install.sh" --dry-run --no-domain --domain x.com
  [ "$status" -ne 0 ]
  [[ "$output" == *"conflicts"* ]]
}
