# What the agent reads and sends

This page lists every input and every network destination, with the place in the source where it
happens.

## Docker Engine API calls

Advisory mode makes three kinds of request, all `GET` ([`src/docker.rs`](../src/docker.rs)):

| Request | Fields kept | Used for |
| --- | --- | --- |
| `/containers/json` | `Id`, `Names`, `Image`, `ImageID`, and the labels `com.docker.compose.project`, `com.docker.compose.service`, `org.opencontainers.image.version` | Recognising apps and grouping them by compose project |
| `/containers/{id}/json` | `Config.Image` and the *names* of `Config.Env` entries (text before `=`) | Rules such as "applies if `DB_HOSTNAME` is set"; the image reference when Docker lists the container by image ID |
| `/images/{id}/json` | `RepoDigests` | Identifying the exact running release |

Every other field in those responses is skipped by the parser. Environment values are discarded by
`EnvName` and non-listed labels by `allowed_labels`, both in `src/docker.rs`; the tests
`environment_values_never_survive_parsing` and `only_allow_listed_labels_are_kept` assert it.

Apply mode additionally calls `POST /images/{id}/tag`, `POST /images/create`, `GET /containers/{id}/json`,
`GET /images/{name}/json`, `POST /containers/create`, `POST /containers/{id}/stop|start|rename` and
`DELETE /containers/{id}` ([`src/update.rs`](../src/update.rs)). The container's own configuration,
including its environment, is passed from the inspect response to the create request on the same
local Docker API and is not stored, logged or sent anywhere else.

## Host

- CPU architecture, from the build target.
- CPU flags, from `/proc/cpuinfo` ([`src/host.rs`](../src/host.rs)).

No hostname, IP address, MAC address or hardware identifier is read.

## Files written

Under `STP_DATA_DIR`:

- `dataset/v1/**` : cached dataset files and their ETags.
- `cloud-link.json` : only when linked. Contains the account address, host id, the name you gave
  the host and its token (mode `0600`).

## Outbound requests

All outbound HTTP goes through `Outbound::send` ([`src/outbound.rs`](../src/outbound.rs)), which
records each request. The dashboard shows that record under **Network activity**.

| When | Request | Body |
| --- | --- | --- |
| Every scan | `GET {STP_DATASET_URL}/v1/index.json` | none |
| Every scan, per tracked app that runs | `GET {STP_DATASET_URL}/v1/apps/{slug}.json` | none |
| When you pair | `POST {cloud}/api/agent/v1/pair` | pairing code, host name, agent version |
| Linked: after every scan | `POST {cloud}/api/agent/v1/report` | the report below |
| Linked: after a requested update | `POST {cloud}/api/agent/v1/commands/{id}/result` | outcome and a message |
| When you unlink | `DELETE {cloud}/api/agent/v1/link` | none |

An agent that was never paired makes only the two `GET` requests. The token is sent only over
HTTPS, or to `localhost` for development.

## The report

Defined by `Report` and `ReportedService` in [`src/cloud.rs`](../src/cloud.rs):

```json
{
  "agent_version": "0.1.0",
  "arch": "amd64",
  "update_mode": "advisory",
  "remote_actions_enabled": false,
  "services": [
    {
      "key": "media/immich-server",
      "name": "immich-server",
      "stack": "media",
      "app": "immich",
      "image": "ghcr.io/immich-app/immich-server:release",
      "current_version": "v1.132.0",
      "target_version": "v1.133.0",
      "verdict": "green",
      "summary": "Safe to update. 1 release checked, nothing to change.",
      "blocking_count": 0,
      "warning_count": 0,
      "can_update": false
    }
  ]
}
```

Only containers of apps in the dataset appear. The test
`a_linked_agent_reports_exactly_the_contract_fields_and_no_secrets` fails if a field is added
without updating this contract.
