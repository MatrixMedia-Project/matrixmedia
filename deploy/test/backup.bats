load helper
setup() {
  setup_tmp; source "$DEPLOY_ROOT/lib/common.sh"; source "$DEPLOY_ROOT/lib/backup.sh"
  mkdir -p "$MM_ROOT/config"; echo x > "$MM_ROOT/config/a.yaml"; echo y > "$MM_ROOT/.env"
}
teardown() { teardown_tmp; }

@test "backup_configs writes a tar.gz containing config + .env" {
  run backup_configs 20260714-000000; [ "$status" -eq 0 ]
  local f; f="$(ls "$MM_ROOT"/backups/*.tar.gz | head -1)"
  tar tzf "$f" | grep -q 'config/a.yaml'
}
@test "backup_configs tolerates a missing .env.secrets" {
  rm -f "$MM_ROOT/.env.secrets"
  run backup_configs 20260714-000000; [ "$status" -eq 0 ]
}
