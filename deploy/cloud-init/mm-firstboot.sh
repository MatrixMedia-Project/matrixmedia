#!/usr/bin/env bash
# First-boot wrapper for cloud-init/marketplace images. Retries until DNS is
# pointed and the install converges (the installer itself is idempotent).
set -u
ENV_FILE=/etc/mm-firstboot.env
[ -f "$ENV_FILE" ] || { echo "missing $ENV_FILE"; exit 1; }
# shellcheck disable=SC1090
source "$ENV_FILE"   # MM_INSTALL_ARGS, e.g.: --domain x.com --email a@b --non-interactive
for attempt in $(seq 1 30); do
  # shellcheck disable=SC2086
  if /opt/mm-src/deploy/install.sh $MM_INSTALL_ARGS; then
    echo "mm-firstboot: converged on attempt $attempt"; exit 0
  fi
  echo "mm-firstboot: attempt $attempt failed; retrying in 60s (fix DNS/ports and it will converge)"; sleep 60
done
echo "mm-firstboot: gave up after 30 attempts; run install.sh manually"; exit 1
