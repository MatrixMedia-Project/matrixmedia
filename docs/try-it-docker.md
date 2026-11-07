# Try MatrixMedia in one command

One container with the whole stack in it: Synapse, Postgres, LiveKit, mm-core, mm-switch,
the dashboard and viewer, and a fake Stripe for demo payments. Pull it, run it, and in a
couple of minutes you have a working MatrixMedia to click around in or point an agent at.

> **For trying only.** Plain HTTP, generated secrets kept in the data volume, demo payments,
> no federation. The image is built from the `development` branch on every push, so it moves
> fast and can break. To run MatrixMedia for real, use the installer in [`deploy/`](../deploy/README.md).

## Run it

```bash
docker run -d --name matrixmedia \
  -p 8008:8008 -p 7881:7881 -p 7882:7882/udp -p 50100:50100/udp \
  -e MM_PUBLIC_HOST=127.0.0.1 \
  -v matrixmedia-data:/data \
  ghcr.io/matrixmedia-project/matrixmedia-allinone:dev
```

Wait for `healthy` (the first start creates the databases and the admin account; give it a
minute or two, longer on a slow machine):

```bash
docker ps --filter name=matrixmedia --format '{{.Status}}'
```

Then read the login it made for you:

```bash
docker exec matrixmedia mm-allinone credentials
```

Open <http://127.0.0.1:8008/> (the dashboard) and sign in with that admin user and password.
The same details are printed in `docker logs matrixmedia` under **MatrixMedia is ready**.

## Phones and other devices: use the LAN IP

`MM_PUBLIC_HOST` is the address clients use to reach the container, and LiveKit and mm-switch
hand it to clients for the media (WebRTC) connection. `127.0.0.1` only works from the same
machine. To join from a phone or another computer, use this machine's LAN IP:

```bash
# macOS
ipconfig getifaddr en0
# Linux
hostname -I | awk '{print $1}'
```

and run with `-e MM_PUBLIC_HOST=192.168.1.20` (your address). The homeserver URL for clients
is then `http://192.168.1.20:8008`. Keep the published ports the same on both sides of each
`-p`: the media ports are advertised to clients as they are inside the container.

## What is where

| | |
|---|---|
| Dashboard (Creator Studio, Operator Console) | `http://<host>:8008/_mm/dashboard/` |
| Viewer | `http://<host>:8008/_mm/viewer/` |
| Matrix homeserver (client API) | `http://<host>:8008` (server name `localhost`) |
| mm-core client API | `http://<host>:8008/_mm/client/…` |
| mm-core admin API | `http://<host>:8008/_mm/admin/v1/…` (bearer: the admin API token) |
| LiveKit media | TCP 7881, UDP 7882 |
| mm-switch media | UDP 50100 |

## Settings

| Variable | Default | |
|---|---|---|
| `MM_PUBLIC_HOST` | `localhost` | Address clients use; set your LAN IP for other devices. |
| `MM_PUBLIC_URL` | `http://$MM_PUBLIC_HOST:8008` | Override if you publish port 8008 elsewhere or put a proxy in front. |
| `MM_DEMO_MODE` | `true` | Demo payments through the fake Stripe in the container. `false` turns monetization off. Read on the first start only (mm-core then keeps its settings in its database). Real Stripe keys are never used by this image. |
| `MM_SERVER_NAME` | `localhost` | Matrix server name. Fixed on the first start; a volume cannot change it. |
| `MM_ADMIN_USER`, `MM_ADMIN_PASSWORD` | `admin`, random | The first admin account. Read on the first start only. |

## For AI agents

```bash
docker run -d --name matrixmedia -p 8008:8008 -p 7881:7881 -p 7882:7882/udp -p 50100:50100/udp \
  -e MM_PUBLIC_HOST=127.0.0.1 -v matrixmedia-data:/data \
  ghcr.io/matrixmedia-project/matrixmedia-allinone:dev
until [ "$(docker inspect -f '{{.State.Health.Status}}' matrixmedia)" = healthy ]; do sleep 5; done
docker exec matrixmedia mm-allinone credentials       # admin user, password, admin API token
curl -s http://127.0.0.1:8008/_matrix/client/versions # Synapse
curl -s -H "Authorization: Bearer <admin API token>" http://127.0.0.1:8008/_mm/admin/v1/health
```

`docker logs matrixmedia` prefixes every line with the process that wrote it
(`[synapse]`, `[mm-core]`, `[livekit]`, …), so a failure is easy to place.

## Start over, stop, update

```bash
docker stop matrixmedia && docker start matrixmedia   # keeps all data
docker rm -f matrixmedia && docker volume rm matrixmedia-data   # wipes everything
docker pull ghcr.io/matrixmedia-project/matrixmedia-allinone:dev  # newest development build
```

Each build is also tagged `sha-<commit>` if you need to stay on one.

## What it does not do

- No TLS. Browsers allow camera and microphone on `http://localhost` and `127.0.0.1` only, so to
  go live from another device's browser you need HTTPS in front.
- No federation, no TURN server, no push notifications, no ad conversion, no fleet.
- One machine, one container; nothing here is tuned or backed up.

The image is built by [`.github/workflows/allinone-image.yml`](../.github/workflows/allinone-image.yml)
from [`infra/docker/Dockerfile.allinone`](../infra/docker/Dockerfile.allinone).
