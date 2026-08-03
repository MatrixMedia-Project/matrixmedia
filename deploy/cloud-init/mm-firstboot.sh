#!/usr/bin/env bash
# First-boot wrapper for cloud-init/marketplace images. install.sh itself
# waits up to ~30 min internally for DNS to resolve (its own gate); this
# wrapper retries the whole install up to 10 times on top of that, so it
# converges whenever you point the DNS records — watch progress with
# `journalctl -u mm-firstboot -f`. Runs once: on success it drops a marker
# file so the enabled oneshot unit is a no-op on subsequent boots.
set -u
ENV_FILE=/etc/mm-firstboot.env
[ -f "$ENV_FILE" ] || { echo "missing $ENV_FILE"; exit 1; }
# shellcheck disable=SC1090
source "$ENV_FILE"   # MM_INSTALL_ARGS, e.g.: --domain x.com --email a@b --non-interactive
for attempt in $(seq 1 10); do
  # shellcheck disable=SC2086
  if /opt/mm-src/deploy/install.sh $MM_INSTALL_ARGS; then
    echo "mm-firstboot: converged on attempt $attempt"
    touch /var/lib/mm-firstboot.done
    exit 0
  fi
  echo "mm-firstboot: attempt $attempt failed; retrying in 60s (fix DNS/ports and it will converge)"; sleep 60
done
echo "mm-firstboot: gave up after 10 attempts; run install.sh manually"; exit 1
