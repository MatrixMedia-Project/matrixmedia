# Install MatrixMedia on your own server

This guide sets up a complete MatrixMedia server on one Linux machine with one command. You get your own Matrix homeserver with chat, voice and video calls, live streaming, recordings and the creator tools.

You don't need any AI tools. Every step is a command you can read before you run it, and [one section](#prefer-to-read-the-installer-first) shows how to inspect the installer before it touches your server.

> **Tested** on 2026-11-08 on a fresh Hetzner Cloud `cx23` (2 vCPU, 4 GB RAM, 40 GB disk) running Ubuntu 24.04. All three ways were run end to end: *Try it*, *Your domain* (real Let's Encrypt certificates; matrix.org's federation tester passed) and `--demo`.

Using an AI agent instead? Point it at [install-agent.md](install-agent.md).

---

## What you get

| Address | What it is |
|---|---|
| `https://matrix.<domain>/_mm/dashboard/` | **Operator Console** and Creator Studio. You sign in here as the server owner. |
| `https://matrix.<domain>/_mm/viewer/` | Public stream viewer |
| `https://matrix.<domain>` | Your Matrix homeserver, for Element, FluffyChat or the MatrixMedia apps |
| `https://call.<domain>` | Element Call (voice and video calls in the browser) |
| `https://<domain>/.well-known/matrix/…` | Discovery files, so apps can find your server from `@you:<domain>` |

It all runs in Docker on the one server: Traefik (HTTPS), Synapse, two PostgreSQL databases, LiveKit, coturn, mm-core, mm-switch, the web apps and Element Call.

## What you need

- **A Linux server running Ubuntu or Debian on x86-64 (amd64).**
  - The images are built for amd64 only, so ARM servers can't run them. That includes Hetzner `cax`, AWS Graviton, Oracle Ampere and Raspberry Pi.
  - The installer installs Docker itself if it's missing.
- **At least 4 GB of RAM** (2 vCPU recommended) and **20 GB free disk** under `/opt`. A finished install uses about 1.8 GB of RAM and 9 GB of disk, before any uploads or recordings.
- **A public IPv4 address.** Calls and streams need it.
- **Root access.** The commands below use `sudo`. If you're logged in as root on a system without `sudo` (minimal Debian), leave `sudo` out.
- **Nothing else using ports 80 and 443.**
- **For a real install:** a domain name whose DNS records you can edit. The try-out mode below doesn't need one.

### Firewall ports

If your provider or your server has a firewall, allow these inbound:

| Port | Protocol | Used for |
|---|---|---|
| 80, 443 | TCP | Web, Matrix, certificates (Traefik) |
| 3478 | TCP and UDP | STUN/TURN (coturn) |
| 5349 | TCP | TURN over TLS (coturn) |
| 7881 | TCP | LiveKit calls over TCP |
| 50000–50020 | UDP | LiveKit call media |
| 50021–50040 | UDP | TURN relay (coturn) |
| 50100–50300 | UDP | mm-switch stream media |

With `ufw`, for example. Keep SSH open (`ufw allow OpenSSH`) before you ever run `ufw enable`.

```bash
ufw allow 80,443,7881,5349/tcp
ufw allow 3478
ufw allow 50000:50020/udp && ufw allow 50021:50040/udp && ufw allow 50100:50300/udp
```

Add the three UDP ranges exactly as written. The installer looks for these ranges by name and warns if `ufw` is active without them. If the UDP ranges are blocked, calls and streams connect but carry no audio or video.

---

## Choose how to install

| | **Try it** | **Your domain** |
|---|---|---|
| Needs a domain | No | Yes, plus three DNS records |
| HTTPS certificate | Self-signed (browser warning) | Real, from Let's Encrypt |
| Phone apps can connect | No (they refuse self-signed certificates) | Yes |
| Talks to other Matrix servers | No | Yes |
| Keep it? | No: the identity is throwaway | Yes |

Moving from *Try it* to *Your domain* later means a fresh install. Your Matrix server name can't be changed once accounts exist.

## Step 1: Get a server

Create a server that meets the list above, with any provider. Log in to it over SSH:

```bash
ssh root@<server-ip>
```

The install runs for a few minutes. If your connection is unreliable, start `tmux` (or `screen`) first, so a dropped SSH session doesn't stop it halfway.

## Step 2: Point your domain at the server (Your domain only)

Pick the domain that becomes your **Matrix server name**: people will be `@name:<domain>`. Then create three DNS **A records**, all pointing at the server's IPv4 address:

| Name | Type | Value |
|---|---|---|
| `<domain>` | A | `<server-ip>` |
| `matrix.<domain>` | A | `<server-ip>` |
| `call.<domain>` | A | `<server-ip>` |

- **Already have a website on that domain?** Use a subdomain as the server name instead, for example `chat.example.org`. The records are then `chat.example.org`, `matrix.chat.example.org` and `call.chat.example.org`, and people are `@name:chat.example.org`. The installer serves Matrix's discovery files at the bare server name, so that name has to point at this server.
- **Don't add AAAA (IPv6) records** unless the server has a public IPv6 address. Phone apps and other Matrix servers that prefer IPv6 would reach nothing, while your own browser might still work.

Check the records before you continue. Changes can take a few minutes to show up. If `dig` is missing, install it with `apt install dnsutils`.

```bash
dig +short A matrix.<domain>     # must print <server-ip>
dig +short AAAA <domain>         # should print nothing, unless the server has IPv6
```

## Step 3: Run the installer

**Try it:**

```bash
curl -fsSL https://raw.githubusercontent.com/MatrixMedia-Project/matrixmedia/main/deploy/install.sh | sudo bash -s -- --no-domain --non-interactive
```

**Your domain:** the email address registers your Let's Encrypt account.

```bash
curl -fsSL https://raw.githubusercontent.com/MatrixMedia-Project/matrixmedia/main/deploy/install.sh | sudo bash -s -- --domain <domain> --email you@example.com
```

The installer:

1. Installs Docker if it's missing.
2. Checks the server: RAM, disk, public IP, free ports, clock, firewall.
3. Waits until your DNS records point at the server (up to about 10 minutes).
4. Generates every password and key.
5. Starts the stack.
6. Creates your owner account.
7. Tests itself.

On the test server this took 3 to 4 minutes. When it's done, the last lines look like this:

```text
[mm] self-smoke passed
[mm] payments are off (no Stripe keys given). To take payments: Operator Console → System → Settings → Monetization, …
[mm] DONE. Operator Console: https://matrix.<domain>/_mm/dashboard/ (sign in as @admin:<domain>) | Matrix apps: homeserver https://matrix.<domain> | calls: https://call.<domain> | password: /opt/mm/admin.credentials | status: mmctl status
```

Some yellow `WARN` lines are normal. In *Try it* mode the installer reminds you that the identity is throwaway and the certificate self-signed. It may also report that matrix.org's federation tester couldn't confirm federation (that check is advisory). What matters is that the run ends with `self-smoke passed` and `DONE`.

The one-liner installs the latest release (`main`). To install a specific [release](https://github.com/MatrixMedia-Project/matrixmedia/releases), add `MM_DEPLOY_REF=<tag>` after `sudo`: `| sudo MM_DEPLOY_REF=<tag> bash -s -- …`.

### Prefer to read the installer first?

The one-liner downloads this repository into `/opt/mm/src` and runs `deploy/install.sh` from there. You can do the same by hand and review everything before it runs:

```bash
git clone --depth 1 https://github.com/MatrixMedia-Project/matrixmedia.git /opt/mm-src
#   (a specific release: add  --branch <tag>)
less /opt/mm-src/deploy/install.sh /opt/mm-src/deploy/lib/*.sh
sudo /opt/mm-src/deploy/install.sh --domain <domain> --email you@example.com
```

Keep `/opt/mm-src` afterwards: the `mmctl` command is a link into it.

### Choosing the owner account

By default the owner is `admin`, with a generated password saved in `/opt/mm/admin.credentials` (readable by root only). To choose your own:

- **Run the installer from a file in a terminal** (the clone above). It then asks for a username and a password (typed hidden).
- **Or pass `--admin-user alice --admin-pass '…'`.** The password then ends up in your shell history.

The username must be a lowercase Matrix name: `a-z`, `0-9` and `. _ = / -`.

## Step 4: Sign in

1. Read the owner password:

   ```bash
   sudo cat /opt/mm/admin.credentials
   ```

2. Open the **Operator Console** at `https://matrix.<domain>/_mm/dashboard/`. Sign in with your Matrix ID, `@admin:<domain>` (or the name you chose), and that password.

3. To chat, use any Matrix app (Element, FluffyChat, or the MatrixMedia apps). Choose a custom homeserver, enter `<domain>`, and sign in with the same account.

Some things that look wrong but aren't:

- **In *Try it* mode, the browser warns about the certificate.** It's self-signed. Continue past the warning.
- **`https://matrix.<domain>/` on its own shows "403 Forbidden".** That address is reserved for a landing page you can add. The dashboard, viewer and Matrix API are all at the paths in the table above.

If the password was generated for you, change it in a Matrix app's account settings. Write the new one down: running the installer again later needs it (see [Running the installer again](#running-the-installer-again)).

## Payments

Without Stripe keys, payments start **off**. Everything else works.

To take payments, go to Operator Console → System → Settings → Monetization. Enter your Stripe secret key and webhook signing secret, turn payments on, then press **Apply & restart**. Details are in [deploy/docs/settings.md](../deploy/docs/settings.md).

To demonstrate payments without real money, install with `--demo`. It adds a fake Stripe and a Lightning test wallet. **Never use `--demo` with real money.**

---

## Everyday operations: `mmctl`

The installer adds the `mmctl` command. Run it as root.

| Command | What it does |
|---|---|
| `mmctl status` | Lists the containers and their health |
| `mmctl doctor` | Re-runs the installer's self-test |
| `mmctl check` | Health report: containers, disk, self-test, and how old your newest backup is. Changes nothing. It reports a failure until you've taken your first backup. |
| `mmctl logs <service>` | Recent logs, for example `mmctl logs synapse` or `mmctl logs mm-core` |
| `mmctl backup` | Saves the configuration, secrets and both databases to `/opt/mm/backups/` |
| `mmctl upgrade` | Takes a backup, then re-pulls the images of the **installed** release, restarts and self-tests. To move to a **new** release, see [Upgrading to a new release](#upgrading-to-a-new-release). |
| `mmctl restore [archive]` | Rolls back to a backup (replaces current data; asks first) |
| `mmctl secrets` | Lists which secrets exist (never prints a value) |
| `mmctl version` | Shows which version is installed |

Don't use `mmctl update`: it pulls and restarts without taking a backup first.

### Backups

- **What's in one:** `mmctl backup` saves the configuration, every secret, and both databases (accounts, rooms, messages, MatrixMedia data).
- **What's not:** uploaded media files and stream recordings. They live in the Docker volumes `matrixmedia_synapse-data` and `matrixmedia_recordings-data`, so back those up separately if you need them.
- **Copy backups off the server.** Treat the copies like passwords, because they contain every secret.
- **Old backups are never deleted automatically.** A nightly backup, keeping two weeks:

```bash
cat > /etc/cron.d/matrixmedia-backup <<'EOF'
0 3 * * * root /usr/local/bin/mmctl backup
30 3 * * * root find /opt/mm/backups -type f -mtime +14 -delete
EOF
```

### Upgrading to a new release

The database only moves forward: a newer version can change it in ways an older one can't read. So **the backup is your only way back.**

1. Take a backup:

   ```bash
   mmctl backup
   ```

2. Run the installer again with the new release's tag. Use the same mode, domain and email as the first install:

   ```bash
   curl -fsSL https://raw.githubusercontent.com/MatrixMedia-Project/matrixmedia/main/deploy/install.sh | sudo MM_DEPLOY_REF=<new tag> bash -s -- --domain <domain> --email you@example.com
   ```

   If you've changed the owner password, add `--admin-user <name> --admin-pass '<current password>'`.

3. Check it:

   ```bash
   mmctl doctor
   ```

   If it fails, roll back with `mmctl restore`.

### Running the installer again

Running the installer again (to repair or reconfigure) keeps your secrets and your data. Two things to know:

- **Without `MM_DEPLOY_REF`, it also moves you to the newest release.** That changes the database, so run `mmctl backup` first, or pin `MM_DEPLOY_REF` to the release you have (`mmctl version` shows it).
- **It signs in as the owner.** If you've changed the owner password, pass `--admin-user <name> --admin-pass '<current password>'`. Otherwise it stops with `could not capture admin token`.

## Start over or remove it

- **`mmctl uninstall`** stops everything and **keeps your data**. `mmctl start` brings it back.
- **`mmctl uninstall --purge`** deletes all data: accounts, messages, streams, recordings. You have to type `yes` to confirm.
- **The configuration stays in `/opt/mm`.** That includes your backups in `/opt/mm/backups`, so copy them elsewhere first. To remove everything, delete it as well: `rm -rf /opt/mm`. Also delete `/opt/mm-src` if you cloned the repository by hand.

---

## Troubleshooting

Run `mmctl status` and `mmctl logs <service>` first. Common messages:

| Message | What to do |
|---|---|
| `need a server with at least 4 GB of RAM` | Use a bigger server. |
| `need >=20GB free on /opt` | Free up disk space or use a bigger disk. |
| `host has no public IPv4` | Use a server with a public IPv4 address. Calls need one. |
| `port 80 already in use by something that is not MatrixMedia` (or 443) | Stop the other web server (`ss -ltnp 'sport = :80'` shows which one), then run the installer again. |
| `waiting for <host> -> <ip>`, then `DNS for <host> does not point to <ip> after ~10m` | Fix the A record from step 2, wait until `dig +short A <host>` shows the server's IP, then run the installer again. |
| `WARN: clock skew …` | Run `systemctl enable --now systemd-timesyncd`. Federation rejects events from a server whose clock is off. |
| `WARN: ufw is active but does not allow …/udp` | Run the `ufw allow` command it prints. Until you do, calls connect but carry no media. |
| `stack did not become healthy` | `mmctl status` shows which container is unhealthy. `mmctl logs <that service>` shows why. |
| `could not capture admin token: … answered: …` | The owner sign-in through `https://matrix.<domain>` failed. On a re-run, the usual cause is a changed owner password: pass `--admin-user` and `--admin-pass`. Otherwise read the text after "answered", plus `mmctl logs traefik` and `mmctl logs synapse`. |
| `smoke: certificate for matrix.<domain> does not validate`, then `self-smoke failed` | Let's Encrypt didn't issue a certificate. Check that DNS points here and port 80 is open from the internet, read `mmctl logs traefik`, then `mmctl renew-certs` and `mmctl doctor`. |
| `self-smoke failed` (other warnings above it) | Each `smoke:` warning names an address and the code it returned. Run `mmctl doctor` again after a minute. |
| Calls connect but there's no audio or video | Open the UDP ranges from [Firewall ports](#firewall-ports). |
| Browser works, phone apps or other servers can't connect | Check for an AAAA record pointing nowhere (`dig +short AAAA <domain>`). |

## What this doesn't set up (yet)

- Push notifications for the official MatrixMedia phone apps. Generic Matrix apps such as Element use their own push service and work normally.
- Monitoring dashboards (Prometheus/Grafana). mm-core and mm-switch expose metrics you can scrape.
- Email (password resets, notifications).

## More

- [deploy/README.md](../deploy/README.md): the installer reference (every flag, the files it writes, the rollback contract)
- [deploy/docs/settings.md](../deploy/docs/settings.md): which settings live in the Operator Console and which in `.env`
- [try-it-docker.md](try-it-docker.md): try MatrixMedia on your own computer, in one container
- [install-agent.md](install-agent.md): the same install, written for an AI agent
