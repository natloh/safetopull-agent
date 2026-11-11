# SafeToPull agent

A small Rust service that runs next to your containers and answers one question for each app:
**is it safe to update, and if not, what do I have to do first?**

It reads which images and versions run on the host, compares them with the public
[SafeToPull](https://safetopull.com) release dataset, and serves the result on a dashboard on your
own network. It works without an account and without any cloud service other than the static
dataset files.

- **Read-only by default.** It advises and holds back. It only changes containers if you start it
  with `STP_UPDATE_MODE=apply`, and then only when you ask.
- **No secrets.** Environment variables are reduced to their names while the Docker response is
  parsed. Values are never stored, logged or sent.
- **Works offline.** The dataset is cached on disk; if the CDN is unreachable the last copy is used.
- **Optional account link.** Pair it with a SafeToPull account to see several hosts in one place.
  Unlink from either side at any time.

## Quick start

```sh
curl -fsSLO https://raw.githubusercontent.com/natloh/safetopull-agent/main/compose.yaml
docker compose up -d
```

Open `http://<your-host>:8484`.

[`compose.yaml`](compose.yaml) runs two containers: the agent (non-root, read-only filesystem, no
capabilities) and a [socket proxy](https://github.com/Tecnativa/docker-socket-proxy) that forwards
only read requests for containers and images. The agent itself never gets the Docker socket.

### Without a socket proxy

The agent can also read the socket directly:

```yaml
    environment:
      STP_DOCKER_HOST: unix:///var/run/docker.sock
    group_add: ["<gid that owns the socket>"]
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
```

The container runs as UID 65532, so it needs the socket's group. Access to the socket is full
control of Docker, and mounting it `:ro` does not restrict what can be requested through it. The
proxy is what makes the advisory setup read-only in fact; prefer it.

## Configuration

All settings are environment variables.

| Variable | Default | Meaning |
| --- | --- | --- |
| `STP_DOCKER_HOST` | `unix:///var/run/docker.sock` | Docker Engine API: `tcp://host:port` or `unix:///path` |
| `STP_LISTEN` | `0.0.0.0:8484` | Address of the local dashboard and API |
| `STP_DATA_DIR` | `/data` | Dataset cache and, if linked, the account link |
| `STP_DATASET_URL` | `https://safetopull.com` | Where `/v1/index.json` and `/v1/apps/{slug}.json` are fetched |
| `STP_SCAN_INTERVAL` | `300` | Seconds between scans (minimum 30) |
| `STP_UPDATE_MODE` | `advisory` | `advisory` never writes; `apply` allows updates you trigger |
| `STP_API_TOKEN` | unset | When set, the local API requires `Authorization: Bearer <token>` |
| `STP_CLOUD_URL` | `https://safetopull.com` | The account service this agent pairs with. It cannot be changed from the dashboard or the API |
| `STP_REMOTE_ACTIONS` | `false` | Whether updates requested from a linked account are honoured |

Unknown values are rejected at startup rather than guessed.

## How a verdict is reached

1. `GET /containers/json` lists running containers. Each image reference is matched against the
   dataset's app index; containers of apps the dataset does not track are counted and ignored.
2. For each tracked app the manifest is fetched (`If-None-Match`, cached on disk).
3. The running version is identified by image digest, then by the
   `org.opencontainers.image.version` label, then by a version-shaped tag.
4. Every release between the running and the newest version is checked. Rules that depend on your
   setup are evaluated against environment variable *names*, companion images in the same compose
   project, CPU flags and architecture.
5. The result is `green` (safe), `yellow` (steps to follow), `red` (held back) or `unknown`.
   Anything the agent cannot verify, including rule types newer than the agent, counts against
   the update.

When a release requires passing through an intermediate version, that version becomes the target.

## Updating from the agent (opt-in)

With `STP_UPDATE_MODE=apply` (see [`compose.apply.yaml`](compose.apply.yaml); the socket proxy
then needs `POST: 1`) the dashboard offers an update button for services that

- have a `green` or `yellow` verdict (`yellow` asks you to confirm the steps),
- run a rolling tag such as `latest` or `release`, and
- do not need an intermediate version first.

The agent pulls the tag, checks that the pulled digest is exactly the release it assessed, then
recreates the container from its current settings. The image it replaces stays on the host as
`<tag>-stp-previous`. If the pulled image is not the assessed release, or the new container does
not start, the previous container is restored and the tag is pointed back at the previous image.

A `red` verdict is never applied. Services pinned to a version tag, to a digest, or to a tag the
dataset does not list as rolling are not changed; the dashboard tells you what to set. A container whose image is no longer on the host (the tag was already
moved by a manual pull) is not rebuilt; recreate it yourself and the agent picks up from there.

The labels Docker Compose uses to detect drift are kept consistent, so a later
`docker compose up -d` leaves an updated container alone. Settings the old image supplied
(command, healthcheck, exposed ports, declared volumes) are taken from the new image; ones you set
are kept. Anonymous volumes are re-attached by name so their data carries over, which also means
`docker compose down -v` no longer removes them for you.

## Linking to an account (optional)

Create a pairing code in your SafeToPull account and enter it under **Cloud link** in the
dashboard. The agent then reports after each scan and receives any update you requested there.
What is sent is defined by one struct, `Report` in [`src/cloud.rs`](src/cloud.rs), and described in
[docs/data-and-network.md](docs/data-and-network.md). Requested updates are refused unless both
`STP_UPDATE_MODE=apply` and `STP_REMOTE_ACTIONS=true` are set on the host.

## Local API

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/healthz` | Liveness |
| `GET` | `/api/status` | Snapshot, configuration summary, link state, outbound request log |
| `POST` | `/api/refresh` | Scan now |
| `POST` | `/api/update` | `{"key": "...", "acknowledged": bool}` (apply mode only) |
| `POST` | `/api/cloud/pair` | `{"code": "...", "name": "..."}`; pairs with `STP_CLOUD_URL` |
| `POST` | `/api/cloud/unlink` | Remove the account link |

`POST` requests must carry an `X-STP-Request` header and are refused when their `Origin` differs
from the `Host`, so another website cannot trigger them through your browser. Every response
forbids framing and inline scripts.

### Who can use the dashboard

Without `STP_API_TOKEN`, anyone who can reach port 8484 can read the dashboard, link or unlink the
agent and, in apply mode, start updates. That suits a trusted home network. Otherwise set
`STP_API_TOKEN`, or publish the port on localhost only (`127.0.0.1:8484:8484`) and reach it through
your own reverse proxy or VPN.

## Building

```sh
cargo test
docker build -t stp-agent .
```

## License

MIT
