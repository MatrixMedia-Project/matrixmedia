load helper
# Caption serving + job hooks (V057): the .vtt siblings ride the existing
# nginx /_mm/recordings/ static route, the recording API only hands out
# caption URLs for ready tracks, and the end path enqueues the caption job
# next to MP4 tracking. Runs in the deploy-lint bats gate next to
# render-validate. No live stack needed: template + source assertions only.

TEMPLATE="$DEPLOY_ROOT/templates/nginx.tmpl.conf"
API_ROOT="$DEPLOY_ROOT/../crates"

_rec_loc() {
  awk '$0 == "    location /_mm/recordings/ {" {on=1} on{print} on&&/^    \}/{exit}' "$TEMPLATE"
}

@test "recordings route serves .vtt siblings as text/vtt" {
  loc="$(_rec_loc)"
  [ -n "$loc" ]
  grep -qxF '        types { video/webm webm; video/mp4 mp4; image/jpeg jpg jpeg; text/vtt vtt; }' <<<"$loc"
}

@test "recordings route keeps serving webm/mp4/jpg with an opaque fallback" {
  loc="$(_rec_loc)"
  [ -n "$loc" ]
  # the pre-caption media types are still in the explicit table (a `types`
  # block replaces the inherited mime.types, so dropping one would
  # silently change what players receive)
  grep -q 'video/webm webm' <<<"$loc"
  grep -q 'video/mp4 mp4' <<<"$loc"
  grep -q 'image/jpeg jpg' <<<"$loc"
  # unknown siblings are opaque bytes, never sniffed as HTML on this origin
  grep -qxF '        default_type application/octet-stream;' <<<"$loc"
  run grep -E 'mime\.types|default_type[[:space:]]+(text|image|application/(xhtml|xml|javascript))' <<<"$loc"
  [ "$status" -ne 0 ]
}

@test "recordings route still allows cross-origin track fetches" {
  loc="$(_rec_loc)"
  [ -n "$loc" ]
  grep -qxF '        add_header Access-Control-Allow-Origin *;' <<<"$loc"
}

@test "caption_urls only ever names ready tracks (no URL that 404s)" {
  client="$API_ROOT/mm-api/src/client.rs"
  # the map is built from get_ready_caption_tracks output — ready rows with
  # a storage key only (see the V057 query) — via the one constructor
  grep -q 'get_ready_caption_tracks' "$client"
  grep -q 'caption_tracker::caption_urls_for' "$client"
  # tier-gated viewers lose the subtitle URLs with the playback URLs
  grep -q 'self.caption_urls.clear();' "$client"
  grep -q 'self.summaries.clear();' "$client"
  # the helper itself skips tracks without a storage key
  grep -q 'storage_key.as_deref()?' "$API_ROOT/mm-api/src/caption_tracker.rs"
}

@test "feed recording.available announces captions + summary" {
  events="$API_ROOT/mm-matrix/src/events.rs"
  grep -q 'pub caption_url_hint: Option<String>,' "$events"
  grep -q 'pub summary: Option<String>,' "$events"
  # the end path feeds both from the ready caption rows (not constants)
  grep -q 'caption_url_hint,' "$API_ROOT/mm-api/src/stream_lifecycle.rs"
  grep -q 'mm_recording_captions' "$API_ROOT/mm-api/src/stream_lifecycle.rs"
}

@test "end path enqueues the caption job next to MP4 tracking" {
  lifecycle="$API_ROOT/mm-api/src/stream_lifecycle.rs"
  grep -q 'track_switch_mp4s(pool, switch, &stream.id).await;' "$lifecycle"
  grep -q 'caption_tracker::enqueue_caption_jobs(pool, &stream.id).await;' "$lifecycle"
  # the worker owns resolution ( polls pending itself, crash-safe), so no
  # in-process resolver or boot sweep may exist to race it
  ! grep -q 'track_caption_job' "$API_ROOT/mm-api/src/caption_tracker.rs"
  ! grep -q 'resume_pending' "$API_ROOT/mm-api/src/caption_tracker.rs"
  ! grep -q 'caption_tracker::resume_pending' "$API_ROOT/mm-server/src/startup.rs"
  # enqueue is consent-gated: no opt-in, no caption job
  grep -q 'for_broadcast' "$lifecycle"
}

@test "captions consent API is wired (creator default + per-broadcast + rerun)" {
  client="$API_ROOT/mm-api/src/client.rs"
  creator="$API_ROOT/mm-api/src/creator.rs"
  # creator default on its own endpoint (never folded into PUT /defaults)
  grep -q '"/creator/me/captions", get(get_my_captions_default)' "$creator"
  grep -q '"/creator/me/captions", put(put_my_captions_default)' "$creator"
  grep -q 'captions opt-in requires the PostgreSQL backend' "$creator"
  # per-broadcast opt-in next to the transcode opt-in, same auth shape
  grep -q 'routes!(get_stream_captions, put_stream_captions)' "$client"
  grep -q 'only the stream host can change its captions setting' "$client"
  grep -q 'captions opt-in requires the PostgreSQL backend' "$client"
  # operator re-run resets the source row; demo role refused
  grep -q 'routes!(rerun_recording_captions)' "$client"
  grep -q 'rerun_source_captions' "$client"
  grep -q 'require_admin' "$client"
}

@test "captions routes are in both specs (generated + hand-written)" {
  gen="$API_ROOT/../contracts/api/generated/mm_api_gen.yaml"
  hand="$API_ROOT/../contracts/api/mm_api_v1.yaml"
  for path in 'streams/{id}/captions' 'recordings/{id}/captions/rerun'; do
    grep -q "$path" "$gen"
    grep -q "$path" "$hand"
  done
  grep -q 'creator/me/captions' "$hand"
  for schema in 'StreamCaptionsResponse' 'SetStreamCaptionsRequest' 'RerunCaptionsResponse'; do
    grep -q "$schema" "$gen"
  done
  for schema in 'CaptionsOptIn' 'CaptionsDefault' 'StreamCaptions' 'SetStreamCaptionsRequest'; do
    grep -q "$schema:" "$hand"
  done
}

@test "V057 captions migration exists and is registered" {
  [ -f "$API_ROOT/mm-db/migrations/V057__recording_captions.sql" ]
  grep -q 'V057__recording_captions.sql' "$API_ROOT/mm-db/src/lib.rs"
  grep -q 'CREATE TABLE IF NOT EXISTS mm_recording_captions' \
    "$API_ROOT/mm-db/migrations/V057__recording_captions.sql"
}

@test "caption worker ships as an always-on compose service" {
  tmpl="$API_ROOT/../deploy/docker-compose.tmpl.yml"
  grep -q 'mm-caption-worker:' "$tmpl"
  grep -q 'matrixmedia-mm-caption-worker:${MM_VERSION}' "$tmpl"
  grep -q 'mm-data:/data' "$tmpl"
  # core, not ads: no profile gate
  worker="$(sed -n '/^  mm-caption-worker:/,/^  [a-z-]*:/p' "$tmpl" | head -n 30)"
  [ -n "$worker" ]
  ! grep -q 'profiles:' <<<"$worker" || ! grep -q 'profiles: \["ads"\]' <<<"$worker"
}

@test "caption worker image builds from the shared Dockerfile" {
  grep -q 'target: caption-worker' "$API_ROOT/../.github/workflows/build-images.yml"
  grep -q 'FROM runtime AS caption-worker' "$API_ROOT/../infra/docker/Dockerfile"
  grep -q 'whisper.cpp' "$API_ROOT/../infra/docker/Dockerfile"
  # The builder-time --help check passes for shared-linked binaries too; the
  # runtime ldd guard is what catches a missing libwhisper.so (stage-proven).
  grep -q "ldd /usr/local/bin/whisper-cli" "$API_ROOT/../infra/docker/Dockerfile"
}

@test "caption worker is built natively and rolled on stage" {
  # The stage box builds first-party images natively (never pulls CI tags),
  # so build.sh must list the worker next to the other five images — and the
  # stage roll must name the service, or `up -d` skips it forever the way it
  # did on the first E1 deploy (worker live in CI, absent on stage).
  source "$DEPLOY_ROOT/lib/build.sh"
  firstparty_build_list /src | grep -q 'matrixmedia-mm-caption-worker:.*|.*/infra/docker/Dockerfile|.*|caption-worker'
  wf="$API_ROOT/../.github/workflows/deploy-stage.yml"
  grep -q 'mm-caption-worker' "$wf"
  # The worker talks to mm-postgres, which listens on mm-db-net only: the
  # service must join that network or it crash-loops on DNS (stage-proven).
  worker="$(sed -n '/^  mm-caption-worker:/,/^  [a-z-]*:/p' "$API_ROOT/../deploy/docker-compose.tmpl.yml")"
  grep -q '\- mm-db-net' <<<"$worker"
  # Recording bytes live in the recordings volume, not mm-data: without the
  # nested mount the worker sees no media (stage-proven) and the .vtt would
  # land where nginx does not serve it.
  grep -q 'recordings-data:/data/recordings' <<<"$worker"
  # No blind template refresh on deploy: the repo template drifts with
  # fresh-install fixes that break existing stage volumes (stage postgres
  # went down 2026-11-09). Server compose is server state (install.sh path).
  ! grep -q 'cp "${GITHUB_WORKSPACE}/deploy/docker-compose.tmpl.yml" /opt/mm' "$wf"
}
