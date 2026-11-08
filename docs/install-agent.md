# Install MatrixMedia: instructions for AI agents

You are an AI agent installing MatrixMedia on a Linux server for a human operator. Follow these steps in order. Every command has an **expect** line. If you get anything else, stop at that step and report it; don't improvise around it.

The same install for people is in [install.md](install.md). The installer's reference is [deploy/README.md](../deploy/README.md).

> **Tested** on 2026-11-08: fresh Hetzner Cloud `cx23` (2 vCPU, 4 GB, x86-64), Ubuntu 24.04. `try`, `domain` (real Let's Encrypt certificates; matrix.org's federation tester passed) and `--demo` were all run end to end.

## Rules

1. **Ask the human first** before you:
   - create or pay for a server
   - change DNS records
   - change a firewall
   - stop a service you didn't start
   - use `--demo`
   - upgrade to a new release
   - run `mmctl uninstall --purge`, `mmctl restore`, or `rm -rf /opt/mm`. These delete data.
2. **Never put secrets in the conversation.** Don't print any of these:
   - `/opt/mm/.env` (holds the Stripe keys)
   - `/opt/mm/.env.secrets`
   - `/opt/mm/secrets/*`
   - `/opt/mm/config/*` (rendered configs contain passwords)
   - anything in `/opt/mm/backups/`
   - the `owner_pass` line of `/opt/mm/admin.credentials`

   Give the human the file path; they read it.
3. **Don't sign in with the owner password yourself.** The human does the first sign-in.
4. **Don't hand-edit** `/opt/mm/docker-compose.yml`, `/opt/mm/config/*` or `/opt/mm/versions.env`. To change the install, re-run the installer. Runtime settings live in the Operator Console.
5. **Never use `mmctl update`.** It restarts without a backup, and the database cannot roll back.
6. **Never combine `--demo` with real Stripe keys.**
7. **Never purge to "fix" a failed install.** Re-running the install command keeps secrets and data. If an install already completed here, the re-run backs up first, because a re-run without a pinned release also upgrades.

## Inputs to get from the human

| Input | Values | Notes |
|---|---|---|
| mode | `try` or `domain` | `try`: no domain, self-signed TLS, throwaway identity, no federation, phone apps can't connect. `domain`: the real thing. |
| `DOMAIN` | e.g. `chat.example.org` | `domain` mode only. Becomes the Matrix server name (`@user:DOMAIN`) and **can't be changed later**. If the bare domain already hosts a website elsewhere, suggest a subdomain. |
| `EMAIL` | an address the human reads | `domain` mode only. Used for the Let's Encrypt account. |
| server | SSH target, or "you're on it" | Run everything below **as root on that server**. |
| release | default: latest | Optional: a release tag to install instead (see Step 2). |

Set the values once in the shell, so nothing is pasted literally:

```bash
DOMAIN=chat.example.org      # domain mode only
EMAIL=admin@example.org      # domain mode only
```

In `try` mode the installer picks the domain itself: the server's IPv4 with dots replaced by dashes, plus `.sslip.io` (for example, `203.0.113.7` becomes `203-0-113-7.sslip.io`). Set it after Step 1 with `DOMAIN="$(curl -fsS4 https://api.ipify.org | tr . -).sslip.io"`.

## Step 1: Check the server (read-only)

```bash
id -u                                                    # expect: 0
uname -m                                                 # expect: x86_64   (on ARM the installer stops: images are amd64-only)
. /etc/os-release && echo "$ID"                          # expect: ubuntu or debian
awk '/MemTotal/{print int($2/1024)}' /proc/meminfo       # expect: >= 3277  (a "4 GB" server reports ~3800)
df -Pk /opt | awk 'NR==2{print int($4/1048576)}'         # expect: >= 20
curl -fsS4 https://api.ipify.org; echo                   # expect: a public IPv4 (not 10.x, 172.16-31.x, 192.168.x, 100.64-127.x)
ss -ltnH '( sport = :80 or sport = :443 )'               # expect: no output
```

If any line fails, stop. Tell the human which requirement isn't met (minimums: x86-64 Ubuntu/Debian, 4 GB RAM, 20 GB free under `/opt`, public IPv4, ports 80/443 free).

**Firewall.** Run `ufw status 2>/dev/null | head -1` and check the provider's firewall if there is one. These must be open inbound:

- TCP: 80, 443, 3478, 5349, 7881
- UDP: 3478, 50000–50020, 50021–50040, 50100–50300

The installer's `ufw` check looks for those three UDP ranges by name. If anything is closed, ask the human before opening it.

**`domain` mode only: DNS.** All three names must resolve to the IPv4 address from above:

```bash
for h in "$DOMAIN" "matrix.$DOMAIN" "call.$DOMAIN"; do echo "$h $(getent ahostsv4 "$h" | awk '{print $1; exit}')"; done
# expect: each name followed by the server's public IPv4
getent ahostsv6 "$DOMAIN" | grep -v '::ffff:' | head -1
# expect: no output (an AAAA record), unless the server has a public IPv6
```

If DNS is wrong, ask the human to create A records for `DOMAIN`, `matrix.DOMAIN` and `call.DOMAIN` pointing at the server's IPv4. Re-check until all three resolve. If an AAAA record exists and the server has no IPv6, ask the human to remove it. The installer only warns about it.

## Step 2: Install

The install takes several minutes (3 to 4 on the test server, including installing Docker and pulling images). Run it in the background so neither a command timeout nor a dropped SSH session kills it. The `set -o pipefail` makes a failed download count as a failure.

`try` mode:

```bash
nohup bash -c 'set -o pipefail; curl -fsSL https://raw.githubusercontent.com/MatrixMedia-Project/matrixmedia/main/deploy/install.sh | bash -s -- --no-domain --non-interactive; echo "exit=$?"' > /root/mm-install.log 2>&1 &
```

`domain` mode:

```bash
nohup bash -c 'set -o pipefail; curl -fsSL https://raw.githubusercontent.com/MatrixMedia-Project/matrixmedia/main/deploy/install.sh | bash -s -- --domain "$1" --email "$2" --non-interactive; echo "exit=$?"' _ "$DOMAIN" "$EMAIL" > /root/mm-install.log 2>&1 &
```

That installs the latest release. For a specific release, add `MM_DEPLOY_REF=<tag>` right before the second `bash`: `| MM_DEPLOY_REF=<tag> bash -s -- …`.

Then poll every 30 seconds, for up to 20 minutes, until an `exit=` line appears:

```bash
grep -m1 '^exit=' /root/mm-install.log || tail -n 3 /root/mm-install.log
```

**Success requires all three:**

```bash
grep -c '^exit=0$' /root/mm-install.log                  # expect: 1
grep -c '\[mm\] self-smoke passed' /root/mm-install.log  # expect: 1
grep -o '\[mm\] DONE\..*' /root/mm-install.log           # expect: one line starting "[mm] DONE. Operator Console: https://matrix.DOMAIN/_mm/dashboard/"
```

**`WARN` lines alone are not failures.** Judge only by the three checks above. These warnings are normal:

- `TEMP MODE …` and `TEMP install …` in `try` mode
- `smoke: federation tester says this server does NOT federate` or `could not reach the federation tester` (advisory; most likely in `try` mode)
- `clock: could not …`

On any failure, go to [If something fails](#if-something-fails).

## Step 3: Verify

On the server:

```bash
mmctl doctor; echo "doctor=$?"      # expect: "[mm] self-smoke passed" and doctor=0
mmctl status                        # expect: every container "Up"; those with a health check "(healthy)"
                                    #         (14 containers; 16 with --demo)
```

From outside the server (your own machine, if that's a different one). In `try` mode keep `-k`; in `domain` mode **remove `-k`**, so a bad certificate shows up as `000`:

```bash
for u in "https://$DOMAIN/.well-known/matrix/client" \
         "https://matrix.$DOMAIN/_matrix/client/versions" \
         "https://matrix.$DOMAIN/_mm/dashboard/" \
         "https://matrix.$DOMAIN/_mm/viewer/" \
         "https://call.$DOMAIN/"; do
  echo "$(curl -ks -m 20 -o /dev/null -w '%{http_code}' "$u") $u"
done
# expect: 200 on every line
```

`https://matrix.DOMAIN/` on its own answers `403`. That's expected: it's reserved for an optional landing page.

## Step 4: Hand over to the human

Report this (fill in DOMAIN; never the password):

- **Operator Console:** `https://matrix.DOMAIN/_mm/dashboard/`. Sign in as `@admin:DOMAIN`.
- **Owner password:** on the server, run `sudo cat /opt/mm/admin.credentials`. Change it after the first sign-in (any Matrix app's account settings). Re-runs and upgrades don't need it.
- **`@mm-core:DOMAIN`** is the server's own admin account (mm-core uses it to talk to Synapse). Don't delete it or change its password.
- **Matrix apps** (Element, FluffyChat, MatrixMedia): custom homeserver `DOMAIN`.
- **Calls:** `https://call.DOMAIN`. **Viewer:** `https://matrix.DOMAIN/_mm/viewer/`.
- **Payments are off** until Stripe keys are added in Operator Console → System → Settings → Monetization.
- **Backups:**
  - `mmctl backup` saves the config, every secret, the signing key and both databases to `/opt/mm/backups/` (root-only files). `mmctl backup --media` adds uploaded media and recordings, which can be large.
  - Copies must leave the server and be kept like passwords.
  - Backups older than 14 days are deleted when a backup succeeds (`MM_BACKUP_KEEP_DAYS` in `/opt/mm/.env`; `0` keeps all).
  - Offer to add a nightly cron, which needs the human's OK: `/etc/cron.d/matrixmedia-backup` with `0 3 * * * root /usr/local/bin/mmctl backup`.
  - `mmctl check` reports a failure until the first backup exists.
- In `try` mode, add: the browser shows a certificate warning, phone apps won't connect, and moving to a real domain means a fresh install.

## If something fails

Find the line in `/root/mm-install.log` and act on it:

| Log line contains | Do this |
|---|---|
| `unsupported CPU architecture` | Stop. The images are amd64-only; ask the human for an x86-64 server. |
| `need a server with at least 4 GB of RAM` | Stop. Ask the human for a server with 4 GB or more. |
| `need >=20GB free on /opt` | Stop. Report `df -h /opt`; ask the human to free space or resize. |
| `host has no public IPv4` | Stop. The server needs a public IPv4; ask the human. |
| `port 80 already in use by something that is not MatrixMedia` (or 443) | Run `ss -ltnp '( sport = :80 or sport = :443 )'` and report the process. Ask before stopping it, then re-run Step 2. |
| `DNS for … does not point to … after ~10m` | Report which host. Ask the human to fix that A record, re-check with `getent` (Step 1), then re-run Step 2. |
| `docker install failed` | Stop. Report the lines above it; the human installs Docker, then re-run Step 2. |
| `could not download https://github.com/…/archive/….tar.gz` | The `MM_DEPLOY_REF` tag doesn't exist, or GitHub is unreachable. Fix the tag or drop it. |
| `exit=` is not 0 and the log is nearly empty | The script download failed (network or URL). Check `curl -fsSI https://raw.githubusercontent.com/MatrixMedia-Project/matrixmedia/main/deploy/install.sh`. |
| `stack did not become healthy` | Run `mmctl status`. For each container that isn't Up or healthy, run `mmctl logs <service> 2>&1 \| tail -40`. Report them; don't edit files. |
| `BACKUP FAILED, so nothing was changed` | A re-run found a completed install and could not back it up. Run `mmctl status`. If the stack is stopped, ask the human, run `mmctl start`, then re-run Step 2. Never set `MM_SKIP_BACKUP=1` without the human's explicit OK. |
| `could not capture admin token: … answered: …` | The server's own admin account (`@mm-core`) could not sign in. Report the text after `answered:`, plus `mmctl logs traefik 2>&1 \| tail -20` and `mmctl logs synapse 2>&1 \| tail -20`. |
| `the owner account @… cannot sign in` | That account already exists with another password. Ask the human to re-run with `--admin-pass '<current>'` themselves, since you must not handle the password. |
| `smoke: certificate for matrix.… does not validate` | Let's Encrypt didn't issue. Re-check DNS (Step 1) and that port 80 is reachable from the internet, then report `mmctl logs traefik 2>&1 \| grep -i acme \| tail -20`. Retry with `mmctl renew-certs` and `mmctl doctor`. |
| `self-smoke failed` (other `smoke:` lines above it) | Wait 60 s, then run `mmctl doctor`. If it still fails, report the `WARN: smoke:` lines (each names a URL and a status code). |
| `mm-core did not become healthy after its restart` | Report `mmctl logs mm-core 2>&1 \| tail -40`. |
| `WARN: ufw is active but does not allow …/udp` | A warning: the install can still succeed, but calls will have no media. Ask the human, then run the `ufw allow …` command it prints. No re-run is needed. |
| `WARN: clock skew` | A warning. Run `systemctl enable --now systemd-timesyncd`. No re-run is needed. |
| `WARN: … has an AAAA record but this host has no public IPv6` | A warning. Ask the human to remove the AAAA record (or give the server IPv6). No re-run is needed. |

After the human fixes something, run the **same** Step 2 command again. Never use `mmctl uninstall --purge` to recover unless the human explicitly says to delete all data.

## Later tasks

| Task | Command | Notes |
|---|---|---|
| Health check | `mmctl check` | Containers, disk, self-test and backup age. Changes nothing. Fails until a first backup exists. |
| Back up | `mmctl backup` | Config, secrets, signing key and both databases → `/opt/mm/backups/`. `--media` adds uploads and recordings. Keeps 14 days. |
| Re-pull the installed release | `mmctl upgrade` | Backs up first, then pulls the **same** pinned images, restarts and self-tests. On a smoke failure it prints the restore command; ask the human before restoring. |
| Move to a new release | `mmctl upgrade --to <tag>`, then `mmctl doctor` | Ask the human first. `main` means the newest release. It downloads the tag (a wrong tag changes nothing), refuses an older release, backs up, then runs that release's installer with this install's settings. On failure it prints the restore command; ask the human before restoring. |
| Logs | `mmctl logs <service>` | Services: `traefik synapse postgres mm-postgres mm-core mm-switch mm-web livekit livekit-egress livekit-ingress lk-jwt-service lk-redis coturn element-call` |
| Version | `mmctl version` | |
| Stop / start | `mmctl stop`, `mmctl start` | Data is kept. |
| Remove | `mmctl uninstall` keeps data; `--purge` deletes it | `--purge` asks for a typed `yes`. Needs the human's explicit consent. `/opt/mm` (including backups) stays until deleted. |

## Facts for planning

- **Files:**
  - `/opt/mm/`: config, secrets (`.env.secrets` 0600, `secrets/*` 0640), backups (0600)
  - `/opt/mm/src/`: the downloaded installer; `mmctl` links into it, so keep it
  - `/usr/local/bin/mmctl`
- **After install:** about 1.8 GB RAM and 9 GB disk used on the test server.
- **The installer writes no log file of its own.** `/root/mm-install.log` from Step 2 is the record.
