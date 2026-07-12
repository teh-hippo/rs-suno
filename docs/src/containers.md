# Containers

The official image is published at:

```text
ghcr.io/teh-hippo/rs-suno
```

It contains `suno`, ffmpeg, CA certificates, and a POSIX shell. This supports
the full audio and artwork feature set without requiring ffmpeg on the host.
The image does not include a scheduler, rclone, or secret-manager clients.

The image is available for Linux amd64 and arm64. Release tags include the
exact release tag, the semantic version, the minor version, and `latest`.
Prefer an exact version for unattended use.

## Quick start with Podman

Create named volumes for the config and library:

```bash
podman volume create suno-config
podman volume create suno-library
```

Create the config interactively:

```bash
podman run --rm -it \
  -v suno-config:/config \
  -v suno-library:/library \
  ghcr.io/teh-hippo/rs-suno:latest \
  config init
```

Use `/library` as the destination when prompted. The config is stored at
`/config/suno/config.toml`.

Run an additive copy:

```bash
podman run --rm \
  -v suno-config:/config \
  -v suno-library:/library \
  ghcr.io/teh-hippo/rs-suno:latest \
  copy /library
```

Run a full mirror:

```bash
podman run --rm \
  -v suno-config:/config \
  -v suno-library:/library \
  ghcr.io/teh-hippo/rs-suno:latest \
  sync --yes /library
```

Docker accepts the same commands with `docker` in place of `podman`.

## Supplying the token

The normal token precedence still applies inside the image. To pass an
environment variable without putting its value in the command line:

```bash
export SUNO_TOKEN='__client=...'

podman run --rm \
  --env SUNO_TOKEN \
  -v suno-library:/library \
  ghcr.io/teh-hippo/rs-suno:latest \
  copy /library

unset SUNO_TOKEN
```

For a mounted secret, put this in the container config:

```toml
[accounts.default]
root = "/library"
token_command = "cat /run/secrets/suno_token"
```

Then mount a root-only token file:

```bash
podman run --rm \
  -v suno-config:/config \
  -v suno-library:/library \
  -v /secure/suno_token:/run/secrets/suno_token:ro \
  ghcr.io/teh-hippo/rs-suno:latest \
  copy
```

The image includes `sh` and BusyBox utilities, so simple `token_command`
pipelines work. A command that uses `bws`, `op`, `pass`, or another external
client requires that executable to be supplied separately.

## Bind mounts and permissions

The image runs as UID and GID `10001` by default. Named volumes inherit the
image directory ownership. For host bind mounts, either make the directories
writable by `10001:10001` or override the container user to match the host.

For rootless Podman:

```bash
mkdir -p "$HOME/.config/rs-suno-container" "$HOME/Music/Suno"

podman run --rm \
  --userns=keep-id \
  --user "$(id -u):$(id -g)" \
  -v "$HOME/.config/rs-suno-container:/config" \
  -v "$HOME/Music/Suno:/library" \
  --env SUNO_TOKEN \
  ghcr.io/teh-hippo/rs-suno:latest \
  copy /library
```

Add the appropriate SELinux relabelling option, such as `:Z`, when the host
requires it.

The manifest, lineage store, audit log, failure log, and last-run state live in
the library mount beside the downloaded media. Keep that mount persistent.

## Scheduling

The image is a one-shot CLI. Let the host scheduler start it, wait for its exit
code, and remove it.

Example systemd service:

```ini
[Unit]
Description=Mirror the Suno library
Wants=network-online.target
After=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/bin/podman run --rm --name rs-suno \
  -v /srv/rs-suno/config:/config:ro \
  -v /srv/rs-suno/library:/library \
  -v /srv/rs-suno/secrets:/run/secrets:ro \
  ghcr.io/teh-hippo/rs-suno:0.37 \
  sync --yes /library
```

Example timer:

```ini
[Unit]
Description=Run the Suno mirror every 30 minutes

[Timer]
OnCalendar=*-*-* *:00,30:00
Persistent=true

[Install]
WantedBy=timers.target
```

Cron can invoke the same `podman run --rm` command. Do not run cron or systemd
inside the image.

## Proxmox VE

Proxmox VE 9 can import OCI images directly as LXC application containers.
Proxmox documents this feature as a technology preview, so validate it with
scratch storage before mounting an existing library.

This image has no init system or DHCP client. A suitable one-shot deployment
uses:

- an unprivileged application container;
- a host-managed network interface;
- the entrypoint overridden to `/bin/sleep infinity`;
- persistent config and library mount points; and
- a host systemd service that starts the CT, runs `suno` with `pct exec`, and
  stops the CT afterwards.

Running `suno` through `pct exec` preserves its exit code for the host service
and monitoring. The CT should remain stopped between runs and should not be
treated as an always-on network service.

If native application containers do not meet the deployment's stability
requirements, run the same OCI image with Podman in a suitably configured
guest. That fallback has more runtime and nesting overhead, but does not
require a different image.

## Updating

Pull or import the new version, run `suno version`, and perform a dry run before
changing the scheduled version:

```bash
podman pull ghcr.io/teh-hippo/rs-suno:0.37.0
podman run --rm ghcr.io/teh-hippo/rs-suno:0.37.0 version
```

Pinning the exact version keeps unattended runs reproducible. The `latest` and
minor-version tags are convenient for manual testing and controlled update
workflows.
