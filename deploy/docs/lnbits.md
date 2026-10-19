# LNbits (demo-profile Lightning)

LNbits ships in the `demo` compose profile only (`MM_DEMO_MODE=true` in `.env`):
it backs demo-mode Lightning tips with a `FakeWallet` funding source. Real-money
production does not run it. Image pin: `LNBITS_IMAGE` in
[`versions.env`](../versions.env) — the maintained `lnbits/lnbits` repo, semver
tag (the old `lnbitsdocker/lnbits` repo is abandoned since 2021).

## Why there is no `LNBITS_ADMIN_USERS`

The compose template deliberately sets no `LNBITS_ADMIN_USERS`. On a fresh
`lnbits-data` volume no `admin` account exists yet, so pre-naming one leaves the
Admin UI with no matching admin and locks the operator out. Instead, **the first
account created through the web UI becomes the superuser**. Create it (below),
then hand its wallet keys to mm-core.

## Fresh-volume bootstrap

Run on the host as root, with the stack already up in demo mode:

1. Open `https://matrix.$MM_DOMAIN/_mm/lnbits` and create the first account.
   This account is the superuser — use a strong password; it guards real money
   in money mode.
2. Inside that account, create the `MatrixMedia` wallet (the funding source is
   already `FakeWallet` via `LNBITS_BACKEND_WALLET_CLASS` — demo only, no real
   funds move).
3. In the wallet, open **API keys** and copy the **Invoice key** and the
   **Admin key**.
4. In Operator Console → System → **Settings** → Monetization, set
   `monetization.lnbits_invoice_key` and `monetization.lnbits_admin_key`,
   **Save**, then **Apply & restart** (same flow as
   [settings.md](settings.md#lnbits-keys-stored-as-placeholder)).
5. Verify: Settings shows both keys as *Set*; a demo tip completes end to end.

## Starting over (fresh volume again)

This destroys every wallet and key on the volume — re-bootstrap and re-enter the
keys in Settings afterwards:

```bash
: "${MM_ROOT:=/opt/mm}"
docker compose --env-file "$MM_ROOT/versions.env" --env-file "$MM_ROOT/.env" \
  --env-file "$MM_ROOT/.env.secrets" -f "$MM_ROOT/docker-compose.yml" \
  -p matrixmedia stop lnbits
docker volume rm matrixmedia_lnbits-data
mmctl start   # recreates the volume; run the bootstrap above
```

## Upgrading from the 2021-era image

Installs predating this pin run a 2021 `lnbitsdocker` build. `mmctl backup`
covers configs + both Postgres instances — **not** `lnbits-data` (SQLite in the
volume), so snapshot it first:

```bash
docker run --rm -v matrixmedia_lnbits-data:/v -v "$PWD":/b alpine \
  tar -czf /b/lnbits-data-backup.tgz -C /v .
```

Then pull the new pin (`mmctl update` with the demo profile) and run the
bootstrap above — on a 2021 data dir, prefer the fresh-volume procedure over
carrying the old SQLite forward.
