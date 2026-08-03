# MatrixMedia Cloud-Init / First-Boot

Unattended installation for cloud VMs (DigitalOcean, Hetzner, Vultr, etc.).

## Usage

1. Copy the contents of `user-data.example` into your provider's user-data field at VM creation.
2. Edit `MM_INSTALL_ARGS` in the user-data before launch — supply `--domain example.com --email you@example.com`, or use `--no-domain` for a throwaway server.
3. After VM boots, monitor progress: `journalctl -u mm-firstboot -f`

The first-boot service retries until DNS resolves and the installer converges (up to 30 attempts, 60s apart). Fix any DNS/port issues, and the installation will resume automatically.
