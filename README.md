# nat-server

Rendezvous and relay server for **Delickate Remote Access**.

Every running desktop app keeps a secure WebSocket open to this server. When
someone types a partner's device ID and clicks **Request connection**, the server
forwards the request, the partner clicks **Allow** or **Deny**, and while the
session is active the server relays traffic between the two PCs. This lets two
PCs behind home or office routers (NAT) reach each other without port forwarding.

> **Important:** this is a long-running program, not a website. It cannot run on
> shared PHP/cPanel hosting. You need a **VPS** (a small Linux server) that you
> control: DigitalOcean, Hetzner, Vultr, Linode, AWS Lightsail, Contabo, etc.
> 1 vCPU and 1 GB RAM is enough to start.

---

## Step-by-step deployment

The commands below assume **Ubuntu 22.04 or 24.04**. The example domain is
`remote.example.com`; replace it with your own everywhere.

### Step 1: Create the server

1. Create an Ubuntu VPS with your provider.
2. Note its **public IPv4 address**, e.g. `203.0.113.10`.
3. Make sure you can log in from Windows (PowerShell):
   ```powershell
   ssh root@203.0.113.10
   ```

### Step 2: Point a domain at it

In your domain's DNS settings (registrar or Cloudflare), add a record:

| Type | Name     | Value          |
|------|----------|----------------|
| A    | `remote` | `203.0.113.10` |

This creates `remote.example.com`. If you use Cloudflare, set it to **DNS only
(grey cloud)** for now, so Caddy can get the certificate directly.

Check it from Windows (it can take a few minutes):
```powershell
nslookup remote.example.com
```
It must answer with your VPS IP before you continue.

### Step 3: Install Docker on the server

On the VPS:
```bash
curl -fsSL https://get.docker.com | sh
docker --version
docker compose version
```

### Step 4: Open the firewall

On the VPS (if `ufw` is used):
```bash
ufw allow OpenSSH
ufw allow 80/tcp
ufw allow 443/tcp
ufw enable
```
Also allow ports **80** and **443** in your provider's cloud firewall, if it has one.
Port 8080 does **not** need to be open; only Caddy talks to it.

### Step 5: Upload the code

**Option A: Git (recommended).** Push this folder to a private GitHub/GitLab
repository, then on the VPS:
```bash
git clone https://github.com/<you>/nat-server.git /opt/nat-server
cd /opt/nat-server
```

**Option B: copy directly from Windows** (PowerShell, from `C:\wamp64\www\electron`):
```powershell
scp -r nat-server\src nat-server\Cargo.toml nat-server\Cargo.lock nat-server\Dockerfile nat-server\docker-compose.yml nat-server\Caddyfile nat-server\.env.example nat-server\.dockerignore root@203.0.113.10:/opt/nat-server/
```
(Create the folder first with `ssh root@203.0.113.10 mkdir -p /opt/nat-server`.)

### Step 6: Configure your domain

On the VPS:
```bash
cd /opt/nat-server
cp .env.example .env
nano .env          # set DOMAIN=remote.example.com, save with Ctrl+O, Enter, Ctrl+X
```

### Step 7: Start it

```bash
docker compose up -d --build
```
The first build takes a few minutes (it compiles the Rust code). Caddy then
gets a free HTTPS certificate from Let's Encrypt automatically.

Watch the logs:
```bash
docker compose logs -f
```
You should see `nat-server listening` and Caddy reporting `certificate obtained successfully`.
Press Ctrl+C to stop watching (the server keeps running).

### Step 8: Check it works

Open in a browser: `https://remote.example.com/health`

You should see something like:
```json
{"online_devices":0,"protocol_version":1,"status":"ok","version":"0.1.0"}
```

### Step 9: Point the desktop app at your server

The app's server address is **`wss://remote.example.com/ws`** (note `wss://` and `/ws`).

**For distributing the app (recommended):** build it with the address baked in.
In PowerShell, in the `remote access` folder:
```powershell
$env:RD_SERVER_URL = "wss://remote.example.com/ws"
cargo build --release -p rd-client
Remove-Item Env:RD_SERVER_URL
```
Share `target\release\rd-client.exe` with the people who will use it.

**To change it on one PC without rebuilding:** create the file
`%APPDATA%\Delickate\RemoteAccess\server-url.txt` containing only:
```text
wss://remote.example.com/ws
```
and restart the app. (The environment variable `RD_SERVER_URL`, if set when
the app starts, overrides both.)

The server address is shown at the bottom-right of the app window.

### Step 10: Connect two PCs

1. Run the app on **both** PCs. Each shows **Status: Ready** once connected to the server.
2. On the PC to be helped: click **Copy ID** and send the ID to the helper.
3. On the helper's PC: paste the ID and click **Request connection**.
4. The other PC shows **"Someone wants to connect to this PC"** → click **Allow**.
5. Both PCs now list the session under **Sessions**, with an **End** button.

Screen sharing and remote control are the next development steps; this
release establishes the approved connection between the two PCs.

---

## Day-to-day operations

| Task | Command (on the VPS, in `/opt/nat-server`) |
|------|------|
| View logs | `docker compose logs -f nat-server` |
| Restart | `docker compose restart` |
| Stop | `docker compose down` |
| Update to new code | `git pull && docker compose up -d --build` |
| Status | `docker compose ps` |

The containers restart automatically after a crash or a server reboot.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| App says *Could not reach the server* | Check `https://your-domain/health` in a browser. Check the URL is `wss://…/ws`. |
| `/health` doesn't load | DNS not pointing at the VPS yet, or ports 80/443 blocked. See `docker compose logs caddy`. |
| Caddy can't get a certificate | DNS must resolve to the VPS **before** starting; with Cloudflare use grey cloud. Then `docker compose restart caddy`. |
| *That device is offline or the ID is wrong* | The partner's app must be running and show **Ready**. IDs must be copied exactly. |
| *This device signed in again from another connection* | The same PC is running the app twice. Close one. |
| Status flips between Connecting and Offline | The server is unreachable or restarting; check the logs. |

---

## Configuration

Set in `.env` (Docker) or as environment variables:

| Variable | Default | Meaning |
|----------|---------|---------|
| `DOMAIN` | (none) | Your domain, used by Caddy for HTTPS (Docker only). |
| `BIND_ADDR` | `0.0.0.0:8080` | Address the server listens on. |
| `MAX_CONNECTIONS` | `5000` | Maximum devices connected at once. |
| `REQUEST_TIMEOUT_SECS` | `60` | How long a request waits for Allow/Deny. |
| `RUST_LOG` | `nat_server=info` | Log level (`debug` for more detail). |

### Without Docker

Build on the VPS with Rust installed (`cargo build --release`), install the
binary and `deploy/nat-server.service` (systemd), and put Nginx in front using
`deploy/nginx.conf` with a certificate from `certbot --nginx`. Instructions are
at the top of each file.

---

## Development

```text
cargo test                 # unit + end-to-end tests
cargo run                  # listens on 0.0.0.0:8080
```
Run the desktop app against it with `RD_SERVER_URL=ws://127.0.0.1:8080/ws`
(the app's default when nothing else is configured).

## How it works

* **Identity.** Each app generates a private 32-byte secret on first launch and
  stores it in `%APPDATA%\Delickate\RemoteAccess\device-secret`. The public device
  ID is a hash of that secret, so the server can verify that a device owns its ID
  without storing anything. Nobody can register under someone else's ID. Treat
  the `device-secret` file like a password.
* **Consent.** No session starts until the target clicks **Allow**. Unanswered
  requests expire after `REQUEST_TIMEOUT_SECS`.
* **Relay.** Only the two participants of an *active* session can send each other
  traffic. JSON messages use `relay`; binary frames start with the 16-byte
  session ID and are forwarded unchanged (for screen frames later).
* **Limits.** 10 connection requests per minute per device, 3 pending outgoing
  and 5 pending incoming requests, 8 active sessions, 8 MB per message, and slow
  clients are disconnected instead of buffering without bound.
* **State is in memory.** Restarting the server disconnects everyone briefly;
  the apps reconnect automatically.

The protocol lives in `src/protocol.rs` and `src/identity.rs`. Keep them in sync
with `crates/rd-core/src/` in the desktop app.
