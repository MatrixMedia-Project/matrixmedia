load helper

setup() {
  setup_tmp
  source "$DEPLOY_ROOT/lib/common.sh"
  source "$DEPLOY_ROOT/lib/build.sh"
  source "$DEPLOY_ROOT/lib/up.sh"
  # Deterministic pins regardless of the developer's exported env.
  export MM_REGISTRY=reg.example MM_VERSION=9.9.9 MM_SWITCH_VERSION=5.5.5
  export MM_FAKESTRIPE_IMAGE=reg.example/matrixmedia-mm-fakestripe:1.2.3
}
teardown() { teardown_tmp; PATH="$ORIG_PATH"; }

ORIG_PATH="$PATH"

@test "firstparty_build_list emits the five CI-matrix images with versions.env tags" {
  out="$(firstparty_build_list /src)"
  [ "$(printf '%s\n' "$out" | wc -l | tr -d ' ')" = "5" ]
  grep -qxF 'reg.example/matrixmedia-mm-core:9.9.9|/src/infra/docker/Dockerfile|/src|mm-core' <<<"$out"
  grep -qxF 'reg.example/matrixmedia-mm-media-worker:9.9.9|/src/infra/docker/Dockerfile|/src|media-worker' <<<"$out"
  grep -qxF 'reg.example/matrixmedia-mm-switch:5.5.5|/src/services/mm-switch/Dockerfile|/src/services/mm-switch|' <<<"$out"
  grep -qxF 'reg.example/matrixmedia-mm-web:9.9.9|/src/infra/docker/Dockerfile.web|/src|' <<<"$out"
  grep -qxF 'reg.example/matrixmedia-mm-fakestripe:1.2.3|/src/infra/docker/Dockerfile.fakestripe|/src|' <<<"$out"
}

@test "build list Dockerfiles and contexts all exist in this repo" {
  firstparty_build_list "$DEPLOY_ROOT/.." | while IFS='|' read -r _ df ctx _; do
    [ -f "$df" ] || { echo "missing Dockerfile $df"; return 1; }
    [ -d "$ctx" ] || { echo "missing context $ctx"; return 1; }
  done
}

@test "build_firstparty refuses a non-checkout source without touching docker" {
  mkdir -p "$MM_ROOT/bin"
  printf '#!/bin/sh\necho "docker must not run" >> "$MM_ROOT/docker-calls"; exit 99\n' > "$MM_ROOT/bin/docker"
  chmod +x "$MM_ROOT/bin/docker"
  PATH="$MM_ROOT/bin:$PATH"
  run build_firstparty "$MM_ROOT/empty"
  [ "$status" -ne 0 ]
  [[ "$output" == *"needs the repo checkout"* ]]
  [ ! -f "$MM_ROOT/docker-calls" ]
}

@test "install.sh wires --build to a native build + skips the registry pull" {
  grep -q -- '--build) BUILD_NATIVE=true;;' "$DEPLOY_ROOT/install.sh"
  grep -q 'build_firstparty "$HERE/.."' "$DEPLOY_ROOT/install.sh"
  grep -q 'MM_BUILD_NATIVE=true' "$DEPLOY_ROOT/install.sh"
  grep -q 'MM_BUILD_NATIVE' "$DEPLOY_ROOT/lib/up.sh"
  grep -q 'docs/native-build.md' "$DEPLOY_ROOT/install.sh"
  [ -f "$DEPLOY_ROOT/docs/native-build.md" ]
}

@test "stack_up pulls by default but skips the pull when MM_BUILD_NATIVE=true" {
  mkdir -p "$MM_ROOT/bin"
  cat > "$MM_ROOT/bin/docker" <<SH
#!/bin/sh
printf '%s\\n' "\$*" >> "$MM_ROOT/docker-calls"
exit 0
SH
  chmod +x "$MM_ROOT/bin/docker"
  PATH="$MM_ROOT/bin:$PATH"
  cp "$DEPLOY_ROOT/docker-compose.tmpl.yml" "$MM_ROOT/docker-compose.yml"
  printf 'MM_DOMAIN=example.com\n' > "$MM_ROOT/.env"
  : > "$MM_ROOT/.env.secrets"
  # wait_healthy would poll the stub forever: fake a healthy project by making
  # `docker ps` report nothing bad — the stub already exits 0 with no output,
  # so starting=0 and bad=0 on the first poll.
  unset MM_BUILD_NATIVE
  stack_up mmtest
  grep -q ' pull$' "$MM_ROOT/docker-calls"
  : > "$MM_ROOT/docker-calls"
  export MM_BUILD_NATIVE=true
  stack_up mmtest
  ! grep -q ' pull$' "$MM_ROOT/docker-calls"
  grep -q 'up -d --remove-orphans' "$MM_ROOT/docker-calls"
}
