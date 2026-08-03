load helper
setup() { setup_tmp; }
teardown() { teardown_tmp; }

# Extracts the MM_INSTALL_ARGS= line as it actually appears in the write_files
# content block of user-data.example (i.e. exactly what cloud-init would drop
# into /etc/mm-firstboot.env on a real VM), strips the YAML block-scalar
# indentation, and writes it to a standalone env file for sourcing.
_extract_install_args_line() {
  local out="$1"
  grep -m1 '^[[:space:]]*MM_INSTALL_ARGS=' "$DEPLOY_ROOT/cloud-init/user-data.example" \
    | sed -e 's/^[[:space:]]*//' > "$out"
}

@test "user-data.example's MM_INSTALL_ARGS line is quoted (sourceable under set -u)" {
  local envf="$MM_ROOT/mm-firstboot.env"
  _extract_install_args_line "$envf"
  [ -s "$envf" ]

  # This mirrors exactly what mm-firstboot.sh does: `source "$ENV_FILE"` under
  # `set -u`. An unquoted MM_INSTALL_ARGS=--domain ... line is parsed by bash
  # as a one-shot assignment prefix on a command named "example.com" (not
  # found -> exit 127) and MM_INSTALL_ARGS is left completely unset, so the
  # very next reference to it under `set -u` aborts with "unbound variable".
  # A properly quoted line sources cleanly and the value round-trips intact.
  run bash -c 'set -u; source "$1"; printf "%s" "$MM_INSTALL_ARGS"' _ "$envf"
  [ "$status" -eq 0 ]
  [ "$output" = "--domain example.com --email you@example.com --non-interactive" ]

  # Belt-and-suspenders: confirm every word survived the round-trip
  # individually (catches partial-splitting regressions, not just total loss).
  read -ra words <<< "$output"
  [ "${#words[@]}" -eq 5 ]
  [ "${words[0]}" = "--domain" ]
  [ "${words[1]}" = "example.com" ]
  [ "${words[2]}" = "--email" ]
  [ "${words[3]}" = "you@example.com" ]
  [ "${words[4]}" = "--non-interactive" ]
}

@test "mm-firstboot.sh aborts loudly if MM_INSTALL_ARGS ends up unset" {
  # Guards against a regression of the fix above landing without a backstop:
  # even if a future edit re-breaks the quoting, mm-firstboot.sh itself must
  # refuse to silently run install.sh with an empty/absent arg list.
  grep -qF ': "${MM_INSTALL_ARGS:?set MM_INSTALL_ARGS in /etc/mm-firstboot.env}"' \
    "$DEPLOY_ROOT/cloud-init/mm-firstboot.sh"
}
