# MatrixMedia Cloud-Init / First-Boot

Unattended installation for cloud VMs (DigitalOcean, Hetzner, Vultr, etc.).

## Usage

1. Copy the contents of `user-data.example` into your provider's user-data field at VM creation.
2. Edit `MM_INSTALL_ARGS` in the user-data before launch — supply `--domain example.com --email you@example.com`, or use `--no-domain` for a throwaway server. Do **not** put `--admin-pass` in `MM_INSTALL_ARGS` — it lands in process argv (visible via `ps`); omit it and the installer generates owner credentials into `/opt/mm/admin.credentials` instead.
3. After VM boots, monitor progress: `journalctl -u mm-firstboot -f`

The installer itself waits up to ~30 min for DNS internally; the wrapper retries up to 10 times on top — it converges whenever you point the DNS records. Fix any DNS/port issues, and the installation will resume automatically.
