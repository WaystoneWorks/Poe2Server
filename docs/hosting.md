# Hosting: letting other players connect

How to run the server so people on other machines can play on it: getting the server, ports and firewall, config, running it permanently, a TLS proxy for the web API, backups and updates, logs, security, troubleshooting. A remote client logs in with an account and password, needs no shared disk (terrain upload), its side channels are authenticated, and all players in an instance share one world. Rough edges: [Not done yet](#not-done-yet). Related designs: [accounts](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/architecture/accounts.md), [instance world](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/architecture/instance-world.md), [side channel](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/protocol/side-channel.md). Players install and start the client with [client.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/client.md).

**This is an unofficial hobby server.** You need your own legal copy of the game on the server machine (the server reads game tables from it); no game files are distributed with the server. Hosting is at your own risk and responsibility, see the [disclaimer](../README.md#disclaimer).

## Pick a way

| Way | For | Where |
|---|---|---|
| **The launcher** | A server on your own PC without a terminal | [Host through the launcher](#host-through-the-launcher) |
| **A release package** | A machine you manage (home server, VPS) | [Getting a server release](#getting-a-server-release) |
| **From source** | Developers | [Running from source](#running-from-source), [setup.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/setup.md) |

Checklist: (1) a legal 0.5.5e Hotfix 1 client install on the server machine, (2) a config with your `public_host`, (3) ports open and forwarded, (4) accounts, (5) the web API behind TLS if anyone outside a trusted network uses it, (6) the players' clients pointing at your address ([What a remote player needs](#what-a-remote-player-needs)).

## Host through the launcher

The launcher's **Servers > Host** tab does this on the machine you play on, for one server or several: it downloads the server release for your game version (SHA-256 checked) or takes a package you have, asks for name, ports, public host and registration, writes the config from the example, starts and stops the server and shows log and usage. The first admin account is made in the server's web admin (first-run page, only from the server's own computer). Details: [launcher.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/launcher.md#host-a-server). The launcher uses plain http for the web API, so the ports and TLS advice below still apply to anyone outside your own network.

## Getting a server release

Take a package from this repository's GitHub releases (tags `server-v<version>`, built by CI, see [releasing.md](releasing.md)). Each release has, per platform, `poe-server-v<version>-game-<game version>-<linux|windows>-x86_64.tar.gz` (Linux) or `.zip` (Windows), a `.sha256` file and a combined `SHA256SUMS`. The game version in the name must be your players' client build.

```sh
sha256sum -c poe-server-v0.5.1-game-0.5.5e.hotfix1-linux-x86_64.sha256    # Windows: certutil -hashfile <zip> SHA256
tar xzf poe-server-v0.5.1-game-0.5.5e.hotfix1-linux-x86_64.tar.gz && cd poe-server-v0.5.1-game-0.5.5e.hotfix1-linux-x86_64
cp config/server.example.toml config/server.toml
# edit config/server.toml: game_dir (your own game install, the folder with Bundles2), [network] public_host,
# and static_dir = "web-admin" to serve the web admin
./poe-server account create <name> --password-stdin   # type the password, Enter; poe-server.exe on Windows
./poe-server account role <name> admin
./poe-server
```

The package mirrors the repository layout, because every path in `config/server.toml` is relative to the folder you start `poe-server` from: the binary, `config/server.example.toml`, `modules/protocol/versions/`, `modules/protocol/opcodes/`, `third_party/dat-schema/` (the exact file the build is checked against), `web-admin/` (served on `[web] bind`), `README.md`, `HOSTING.md` (this guide) and `release.json` (server version, game version, commit). **No game files are included**: `game_dir` must point at a legal client install. The database defaults to `runtime/server.sqlite` (created on start). Always start from the package folder (a service must set its working directory to it).

The example config listens on all interfaces (`0.0.0.0`), has `open_registration = false` and the web API on loopback; check it against [Security notes](#security-notes) before opening ports.

## Running from source

```sh
git clone --recursive https://github.com/WaystoneWorks/Poe2ModdingProject.git && cd Poe2ModdingProject
third_party/fetch-tools.sh          # tools and the pinned nightly toolchain, see setup.md
cp config/server.example.toml /path/to/my-server.toml     # edit it; do not use the developer config for others
POE_CONFIG=/path/to/my-server.toml cargo run --release -p poe-server
```

The committed `config/server.toml` listens on loopback only (right for a checkout, wrong for a hosted server): start from `config/server.example.toml`. Developer config: [setup.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/setup.md).

## Ports

| What | Default | Protocol | Who connects | Forward? |
|---|---|---|---|---|
| Login server | `[network.login] bind` = `0.0.0.0:20481` (example config; the dev config uses `127.0.0.1`) | TCP | every client (gateway entry), in-game transfers | yes |
| Instance server | `[network.instance] bind` = `0.0.0.0:6112` | TCP | every client (handed over by the login server), the DLL's terrain upload | yes |
| Side channel | the instance port number | **UDP** | client DLL: position reports, debug link (overlay, commands) | yes, UDP and TCP |
| Web API and web admin | `[web] bind` = `127.0.0.1:8080` | TCP (HTTP) | admins, the launcher | no, unless behind TLS |

Nothing else: the terrain upload shares the instance TCP port and the web API is optional. Connections not yet logged in are limited ([Security notes](#security-notes)).

## Server setup

Edit `config/server.toml` (a copy of `config/server.example.toml`; or keep it elsewhere and start with `POE_CONFIG=<file>`):

```toml
[network]
# The address players type: your LAN address for a LAN party, your public IP or DNS name over the internet.
public_host = "203.0.113.7"

[network.login]
bind = "0.0.0.0:20481"
# public_port = 30481      # only when the forwarded port differs from the bind port

[network.instance]
bind = "0.0.0.0:6112"
public_port = 6112         # the port the login server tells clients to connect to
```

- `public_host` feeds three places: the gateway entry that `cargo xtask deploy-client` writes (`gateway_list.txt`), the host in `InstanceDetails` (login server to client), and the DLL's `trusted_servers` list. Never leave it at `127.0.0.1` for remote players; the server warns at startup if it listens beyond loopback but advertises a loopback address.
- Behind port forwarding the client-visible ports can differ: `[network.instance] public_port` (also where the DLL sends its side channel) and `[network.login] public_port` (optional). Forward the same external port to the internal one for TCP and UDP of the instance port.
- Names work for `public_host`: the client resolves it, and `deploy-client` resolves it to IPv4 for `trusted_servers` (the DLL only reads IP literals). Prefer a fixed address, or redeploy when it changes. Names under the official game domains are refused for the trust list.
- A player on the server's LAN using the public address needs NAT loopback (hairpin) on the router, or the LAN address in their own deploy (`public_host` is read from the `server.toml` they deploy with).
- `[game] game_dir` is the folder with `Bundles2` of the server machine's own client; `[game] version` selects `modules/protocol/versions/<version>.toml`.
- `[cluster] id` is part of every login token; change it to log everyone out.
- `[database] path` is the SQLite file (accounts, characters, tokens).

### Terrain (`[spawn]`, `[spawn.upload]`)

By default the server generates terrain itself: the collision grid and the spawn layout come from its generator, so remote players spawn correctly without uploading anything. The client's layout and grid are only a fallback for areas the generator cannot make (for example `G_login` has no grid) or when its tile hash differs from the client's. A remote client's DLL can offer the tile hash over the instance TCP port and upload (zlib-compressed, about a quarter of a megabyte for the Riverbank) if `[spawn.upload] enabled`; the server validates it and caches it per tile hash in `[spawn] layout_dir` (default `dumps/0.5.5e.hotfix1/terrain/{layouts,grids}`). Local files in that directory keep working.

- `[terrain] generated_grid = true` (default): the server builds the collision grid and the spawn layout itself, only when its tile hash equals the client's; otherwise the log says `generated collision grid unavailable` and the client's files are used. `using the generated collision grid` and `spawn from layout ... source="generated"` show it worked. Generation takes about 13 s per new instance in a dev build, inside the loading screen.
- `[spawn.upload] enabled = false` (default); set `true` only on a server for people you trust, as the fallback. `max_bytes`, `max_layout_bytes`, `max_grid_bytes` cap an upload as sent and after decompression; `wait_ms` is how long a spawn waits for a layout nobody announced, `wait_upload_secs` how long once an upload was announced (raise for slow uplinks).
- Without a layout the player spawns at the origin, outside the walkable area on most maps. For a wrong spawn look for `spawn from layout` (`source="generated"` or `"client"`) and, with uploads on, `terrain upload cached` in the log.
- Uploads (when enabled) need a trusted connection and the DLL; the player must not be on an official server. See [Security notes](#security-notes).

### Firewall

Open the ports (inbound), TCP and UDP of the instance port:

```
# ufw
ufw allow 20481/tcp
ufw allow 6112/tcp
ufw allow 6112/udp
# firewalld
firewall-cmd --permanent --add-port=20481/tcp --add-port=6112/tcp --add-port=6112/udp && firewall-cmd --reload
# Windows
New-NetFirewallRule -DisplayName "PoE2 server" -Direction Inbound -Protocol TCP -LocalPort 20481,6112 -Action Allow
New-NetFirewallRule -DisplayName "PoE2 server UDP" -Direction Inbound -Protocol UDP -LocalPort 6112 -Action Allow
```

On a router and in cloud firewalls forward the same three (TCP 20481, TCP 6112, UDP 6112). Blocked UDP does not stop play: only position reports (drift checker) and the overlay's debug link stop.

Check from outside: `nc -vz <host> 20481`, `nc -vz <host> 6112`; the UDP port answers nothing by design. The log shows `client position reports started` and `side-channel session` when a client gets through.

### Accounts

Create accounts offline with `account create <name> --password-stdin` ([setup.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/setup.md#accounts-and-characters-offline); other commands: [server-admin.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/server-admin.md#accounts)), or turn on `[accounts] open_registration` so players register on the server's register page (the web API must be reachable, behind TLS). Make yourself admin with `account role <name> admin`. There is no password-less login; the Steam `0x0005` login is always refused.

## Running it permanently

One process, no daemon mode; stop it with Ctrl+C or SIGTERM (players disconnected and saved, exit code 0). Admins can also stop and restart it from the web admin with an in-game countdown ([server-admin.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/server-admin.md#stop-and-restart-the-server)), on a schedule ([scheduled restarts](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/server-admin.md#scheduled-tasks): set `TZ` in the service environment if needed), and edit the config from the browser ([config editor](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/server-admin.md#config-editor): point it at your copy with `POE_CONFIG`; give the service user write access to that file and to `[web] config_backup_dir`). Run from the package folder.

**Linux (systemd).** `/etc/systemd/system/poe-server.service` for a dedicated user (adapt paths):

```ini
[Unit]
Description=PoE2 private server
After=network-online.target
Wants=network-online.target

[Service]
User=poe
WorkingDirectory=/opt/poe-server
ExecStart=/opt/poe-server/poe-server
Environment=RUST_LOG=info
# A crash restarts the server. The web admin's "Restart" (with `[server] restart_mode = "exit"`
# in config/server.toml) exits with code 75: count it as a normal exit and restart on it.
# The web admin's "Stop" and `systemctl stop` exit 0 and stay stopped.
Restart=on-failure
RestartForceExitStatus=75
SuccessExitStatus=75
RestartSec=2
# Optional hardening: the server only writes below runtime/ and (terrain cache) dumps/.
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/opt/poe-server/runtime /opt/poe-server/dumps
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

`systemctl daemon-reload && systemctl enable --now poe-server`; log: `journalctl -u poe-server -f`. Set `restart_mode = "exit"` in `[server]` so the Restart button hands the restart to systemd (exit code 75); with the default `"exec"` the server restarts itself inside the service (same pid), which works too. `systemctl restart poe-server` is a graceful stop (5 s drain) and start. Account commands work against the database directly, service running or not (`sudo -u poe /opt/poe-server/poe-server account create ...` from the same folder).

**Windows.** Run `poe-server.exe` from the package folder, or register a scheduled task at boot with "Start in" set to the folder (`schtasks /Create /TN poe-server /SC ONSTART /RU <user> /TR "cmd /c cd /d C:\poe-server && poe-server.exe >> runtime\server.log 2>&1"`). A wrapper such as NSSM works the same way; with a wrapper that restarts the program on exit set `[server] restart_mode = "exit"` (code 75), because Windows has no `exec` and the default mode starts a new process and exits 0.

**Anywhere.** `tmux`/`screen` is fine for a few friends; the working directory must hold `config/` and `modules/`.

## TLS reverse proxy for the web API

The web API and web admin carry passwords and admin actions, and the server has no TLS of its own. Keep `[web] bind` on loopback and publish it through a reverse proxy that terminates TLS: forward `/api/` and the WebSocket upgrade, send the real client address, and set `[security] trust_forwarded_for = true` **only** when the proxy is the only way to reach the port. With it on, the server uses the **last** `X-Forwarded-For` entry (the one your proxy appended), so a client cannot choose the address the login limiter sees. Without it every web client appears as the proxy and one attacker can lock the web login for everyone (the game login on port 20481 is unaffected).

Caddy (`Caddyfile`; certificates automatic for a public DNS name; appends `X-Forwarded-For` and passes WebSockets by itself):

```
play.example.org {
    reverse_proxy 127.0.0.1:8080
}
```

nginx (inside a `server { listen 443 ssl; ... }` block with your certificate):

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;   # appends the real peer: what trust_forwarded_for reads
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;                         # the live log WebSocket
    proxy_set_header Connection $connection_upgrade;                # with: map $http_upgrade $connection_upgrade { default upgrade; "" close; }
}
```

Then set `[security] trust_forwarded_for = true` and add the server in the launcher as `https://play.example.org`. The server sends security headers (CSP, `nosniff`, no referrer, no framing) with the web admin. `[web] cors_origins` stays empty unless another site must call the API from a browser. Do not forward port 8080 itself. The snippets are standard patterns, not run against a release package.

## Backups, updates and maintenance

- **Back up** `runtime/server.sqlite` (accounts, tokens, characters) and `config/server.toml`; the terrain cache in `dumps/<version>/terrain/` is rebuilt from clients. Online: `sqlite3 runtime/server.sqlite ".backup 'backup-$(date +%F).sqlite'"`, or copy the file while stopped. Passwords are argon2id hashes, tokens SHA-256; still treat the file as private.
- **Updating the server:** stop it, unpack the new release next to (or over) the old one, keep `config/server.toml` and `runtime/`, start it. Migrations run at start-up. Compare `config/server.example.toml` for new keys; missing keys have defaults, removed keys are ignored. A release names its client build in the file name: when GGG ships a new build, wait for a release for it and tell players not to update the game ([new-game-version.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/new-game-version.md)).
- **Updating players:** the launcher or `deploy-client` deploys hook DLLs and gateway entry; a release that changes the supported build needs a new launcher/hook ([client.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/client.md)).
- **Resetting:** change `[cluster] id` to log everyone out; `account disable <name>` locks an account; delete a bad terrain entry's `terrain_layout_<hash>.tsv` and the grid beside it.

## Logs and monitoring

- Standard output; level `[logging] filter` (`tracing` syntax, default `info,poe_net=debug`), `RUST_LOG` overrides. Useful lines: `login server listening`, `authenticated`, `ticket redeemed`, `player joined ... players=N`, `terrain upload cached`, `side-channel session`, warnings such as `too many connections before login: refused`.
- Admin commands (kick, announce, role changes) go to `[commands] audit_log` (time, account, command, result).
- The web admin's **Logs** page shows the live log, the dashboard players and instances (charts keep 24 h in memory, a few MB). `curl http://127.0.0.1:8080/api/v1/server` returns realm name, game version, players online, uptime and the addresses the launcher uses.
- Nothing is sent anywhere: no telemetry, no update checks.

## What a remote player needs

1. **Their own legal install** of Path of Exile 2 0.5.5e Hotfix 1, and Steam running when they start the client (otherwise `Unable to init Steam API`).
2. **The client hook** (`version.dll`, `poe_hook.dll`, `poe_hook_core.dll`, `poe-hook.toml`) with a gateway entry and trust list pointing at your server. Either:
   - **the launcher** ([launcher.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/launcher.md)): installs the game from Steam depots, deploys the hook, takes the server's web API address (which must be reachable, behind TLS outside a trusted network); or
   - **`cargo xtask deploy-client`** from a checkout, with a `config/server.toml` holding your `public_host` and ports, and the client files in `GameFiles/`: `cargo xtask build-dll` (once), then `cargo xtask deploy-client --no-build --login-mode standalone`. It writes the gateway entry `public_host:login port`, `login_mode`, `position_sync_port` and `trusted_servers = ["<public_host>:<login port>", "<public_host>:<instance port>"]`. Add servers by hand in `GameFiles/poe-hook.toml`, e.g. `"203.0.113.7:*"`.
3. **Start the client** (`cargo xtask run-client`, or Steam with the deployed folder) and log in with account name and password ([setup.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/setup.md#remember-me-token-launcher-auto-login)).

Trusted entries mean: connections are not blocked, the read-only latch stays off, the login crypto bypass runs, and the side channel (position reports, debug link, terrain upload) goes to them. Official servers, the shop and telemetry stay refused or read-only, and no config key can trust anything in official mode ([client-dll.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/architecture/client-dll.md#login-mode-and-trusted-servers)). A client deployed with `--official` never sends anything to any server of ours. The player needs no `dumps/`, `GameFiles/Bundles2` copy or shared folder.

## Security notes

- **Pre-login limits.** Before login (login server) or ticket (instance server) one message must arrive complete within 15 s, the login must finish within 60 s, at most 64 KiB are buffered for an incomplete message, and at most 256 such connections exist in total and 16 per client IP. Config values in `[network.login]` and `[network.instance]` (`pre_auth_*`, `max_pre_auth_*`; `0` turns one off; [accounts.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/architecture/accounts.md#limits-before-the-login)). Players behind one NAT share the per-IP limit: raise `max_pre_auth_per_ip` for a LAN party. This is protection against careless load, not a DDoS defence.
- **Side channel.** Position reports, the overlay's debug link and the terrain upload are authenticated per session: an HMAC-SHA256 key derived from the instance session's ticket keys (the 64 bytes only that client gets); every frame carries the session id, a counter and a 16-byte tag, and the server drops datagrams with no valid tag, an unknown session or a replayed counter. Nobody else can move a session's reports, inject overlay commands (which can be admin commands) or redirect its debug state. Details: [side-channel.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/protocol/side-channel.md). There is no unauthenticated fallback.
- **What it does not protect.** Frames are authenticated, not encrypted: someone on the path can read positions and debug state, not forge them. The game stream is encrypted, but with the crypto bypass its keys derive from a value the server sends in clear ([crypto.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/re/crypto.md)), so on the open internet someone on the path can decrypt a session and its side-channel keys. Fine on a LAN or over a VPN; for the open internet use a VPN (WireGuard or similar) until the real key exchange is restored.
- **The terrain upload** is accepted only from a session that redeemed a ticket, for the tile hash it reported, size-limited, decompression-limited and format-checked; it cannot overwrite an existing entry. A player can still upload a wrong layout for their own area (the hash cannot be recomputed from the export), so a wrong entry is a server-side file to delete: `dumps/0.5.5e.hotfix1/terrain/layouts/terrain_layout_<hash>.tsv` and the grid beside it. The upload is off by default; if you enable it for people you trust, the risk below applies.
  **Audit S-08 (closed for the default config):** `[spawn.upload] enabled` is **off** by default because the server generates the terrain and spawn layout itself. With it on, a modified client can plant a well-formed but wrong layout for its own area hash and later players of that area spawn by it; it cannot overwrite an existing entry, touch other areas or reach other players' game state. Enable it only for trusted hosts, as a fallback for areas the generator cannot make (e.g. `G_login`).
- **Web API.** `[web] bind` defaults to loopback because passwords travel in request bodies and admin endpoints act on the whole server. Use a TLS reverse proxy ([above](#tls-reverse-proxy-for-the-web-api)); never forward 8080 without TLS. `[accounts] open_registration` stays off unless you want strangers to create accounts. The log WebSocket authenticates with a single-use ticket, not the session token in the URL.
- **Admins.** Overlay server commands and `!` chat commands need the admin role (`account role <name> admin`); nobody is admin by default. (A database created before account roles existed may have an account `Exile1`, made by the removed machine-hash login, promoted by the migration; check `account list`.)
- **Password logins** are rate limited per account name and client IP (`[security]`, [accounts.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/architecture/accounts.md#password-attempt-limits)). Someone who knows an account name can lock it for the lockout time by failing on purpose; the owner can still use a saved token.
- **Dev settings.** `[dev] known_seeds`, `sweep`, `movement_trace`, `debug_link`, `position_sync` and the `POE_*` environment variables are for development; the committed config and the developer config (`runtime/server.dev.toml`, [setup.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/setup.md#developer-config)) are not for a shared server. `[dev] drift_check` is harmless.

## Troubleshooting

| Symptom | Likely cause | What to do |
|---|---|---|
| Log: `[network] public_host = "127.0.0.1" is a loopback address` | Listening beyond loopback but advertising localhost | Set `public_host` to the LAN/public address or name |
| Client cannot reach the realm / "Local Server" never answers | Port 20481 not open or forwarded; wrong `public_host` in the gateway entry | `nc -vz <host> 20481` from outside; check the player's `gateway_list.txt` |
| Login works, then the client hangs on loading or disconnects | Port 6112 TCP blocked, or `public_port` differs from the forwarded port | `nc -vz <host> 6112`; set `[network.instance] public_port` to the external port |
| Player spawns in a wrong place / at the origin | The generator cannot make the area and no client layout is cached (uploads off by default) | Look for `spawn from layout` and `terrain upload cached`; see [Terrain](#terrain-spawn-spawnupload) |
| Overlay shows no state, no position drift | UDP 6112 blocked | Forward UDP too; play is not affected |
| Launcher: "cannot reach the server" | Web API on loopback or not behind TLS | [TLS reverse proxy](#tls-reverse-proxy-for-the-web-api); `curl https://<host>/api/v1/server` |
| "You have attempted to log in too often" / HTTP 429 | Rate limiter (5 failures per account or 20 per IP in 5 minutes) | Wait for the lockout (`login_lockout_secs`) or restart the server; behind a proxy set `trust_forwarded_for` |
| "There has been a patch that you need to update to" | The client exe is not a known build of `[game] version` | Use the supported build; to try another `[security] allow_unknown_client = true` ([new-game-version.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/new-game-version.md)) |
| Server will not start: `... is not a supported build` / version file errors | `[game] version` has no `modules/protocol/versions/<version>.toml` in the folder you started from | Start from the package folder; check `[game] version` |
| `Too many connections before login: refused` | More than 16 unauthenticated connections from one address, or 256 in all | Normal under a flood; raise `max_pre_auth_per_ip` for many players behind one NAT |

## Local two-client test

`scripts/test-mp.sh` runs two real game clients against one server on one machine (Steam must be running), connecting through the LAN IP so the trusted-remote path of the DLL runs, each as its own account (`mp1`, `mp2`) with a remember-me token. Commands (`setup`, `server`, `clients`, `keys`, `shot`, `stop`) and environment variables: [setup.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/guides/setup.md#two-client-test-lan). Verified with client 0.5.5e Hotfix 1: both accounts log in through the LAN IP and share one `G1_town` instance (entities 12000 and 12001, `players=2`), see each other, relay walk, roll and chat, and spawn at least `[spawn] player_spacing` (8) cells apart. A transfer (`POST /admin/players/<name>/transfer {"area":"G1_1"}`) sends the leave sequence, and a duplicate login kicks the first session. The remote roll order and look are in [entities.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/protocol/entities.md#remote-roll-check-wp-c2).

Town freeze: `InitialState` flags `04` for towns and hideouts. Flags `7` start the client's world paused (`+0x868/+0x869`); in a zone the client sends `PauseRequest(1)` and `PauseRequest(0)` itself and the server releases it, in a town it sends none.

## Web API

`[web]` is optional ([web-api.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/architecture/web-api.md)). The game never needs it, but the launcher does (`GET /server`, the server's register page), so a server for launcher users exposes it through a TLS proxy. Default `127.0.0.1:8080`.

## Not done yet

- **Remote players:** no equipment on remote players, no party, trade or combat sync between players.
- **Real login crypto.** The bypass keys derive from a public value; fine on a LAN, not over the open internet ([crypto.md](https://github.com/WaystoneWorks/Poe2ModdingProject/blob/main/docs/re/crypto.md)). Later: the DLL swaps the client's baked-in DSA public key for ours and keeps verification on.
- **Per-client address.** One `public_host` serves everyone; LAN and internet players of the same server need NAT loopback or their own `server.toml` for deploy.
