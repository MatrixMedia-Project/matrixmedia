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

@test "mmctl usage advertises the rotate subcommand" {
  run bash "$DEPLOY_ROOT/mmctl"
  [ "$status" -ne 0 ]
  [[ "$output" == *"rotate <secret> [--dry-run]"* ]]
}

@test "mmctl version prints a version line" {
  run bash "$DEPLOY_ROOT/mmctl" version
  [ "$status" -eq 0 ]
  [[ "$output" == mmctl\ * ]]
}

# Found on a clean server (2026-11-08): each of these failed on a healthy install.

@test "mmctl version names the downloaded ref when there is no git checkout" {
  T="$(mktemp -d)"; mkdir -p "$T/src"; cp -R "$DEPLOY_ROOT" "$T/src/deploy"
  echo v0.10.1 > "$T/src/.deploy-ref"
  run env MM_ROOT="$T/root" bash "$T/src/deploy/mmctl" version
  rm -rf "$T"
  [ "$status" -eq 0 ]
  [ "$output" = "mmctl v0.10.1" ]
}

# `mmctl check` died with "check_min: command not found": it lives in preflight.sh,
# which mmctl did not load.
@test "mmctl loads the lib that defines check_min (mmctl check uses it)" {
  libs="$(sed -n 's/^for f in \(.*\); do source "\$HERE\/lib\/\$f.sh"; done$/\1/p' "$DEPLOY_ROOT/mmctl")"
  [ -n "$libs" ]
  run bash -c 'for f in '"$libs"'; do source "$1/lib/$f.sh"; done; declare -F check_min' _ "$DEPLOY_ROOT"
  [ "$status" -eq 0 ]
}

# mm-core's /_mm/admin/v1/health takes MM_ADMIN_TOKEN. doctor, check and upgrade sent
# the Synapse admin token first and always got 401: doctor and check failed on healthy
# installs, and every upgrade "failed" its smoke and printed the restore command.
@test "every self_smoke caller probes mm-core's admin health with MM_ADMIN_TOKEN" {
  run bash -c "grep -rn 'self_smoke \"' '$DEPLOY_ROOT/install.sh' '$DEPLOY_ROOT/mmctl' '$DEPLOY_ROOT/lib'"
  [ "${#lines[@]}" -ge 4 ]   # install, doctor, check, upgrade
  for l in "${lines[@]}"; do
    [[ "$l" == *'"${MM_ADMIN_TOKEN:-}"'* ]] || { echo "wrong token: $l"; return 1; }
  done
}
