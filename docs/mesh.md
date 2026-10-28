# The fluxolap mesh

Friends, fluxolap nodes that are allowed to talk to each other, tell each other which databases and cubes they have. A node can see what a friend has calculated and fetch it, by itself or when asked. Nodes never write to each other: a friend's database arrives as a **copy** on your node, and the only thing that changes your cubes besides your own writes is such an import.

Also here: **sensor data in** over MQTT, on a separate local broker ([below](#sensor-data)).

## How it works

There is no central broker. Every node runs its own small MQTT broker and connects, as a client, to the brokers of its friends.

```
        node a                         node b
   ┌──────────────┐               ┌──────────────┐
   │ broker (a)   │◄──subscribe───│ client       │
   │  retained:   │               │              │
   │  fluxolap/a/ │               │ broker (b)   │
   │  catalog     │───subscribe──►│  fluxolap/b/ │
   │ client       │               │  catalog     │
   └──────────────┘               └──────────────┘
```

MQTT carries only the small things. The database itself travels over the REST API that every node already has.

| Topic (on the broker of `n`) | Direction | Meaning |
|---|---|---|
| `fluxolap/n/catalog` | `n` publishes, retained | `n`'s catalog, again whenever it changes (checked every 2 seconds). A friend that connects later gets it at once. |
| `fluxolap/n/req/<f>` | friend `f` publishes | "Do you have this database?" `{"id": 7, "database": "Sales"}` |
| `fluxolap/n/resp/<f>` | `n` publishes | The answer, as it is *now*: `{"id": 7, "found": true, "db_id": 3, "modified": 1761650000}` |

Every `FLUXOLAP_MESH_POLL_SECS` seconds a node asks the **friends service** who its friends are, and connects to new ones and drops removed ones. A friend that is down stays in the list with its last known catalog and `connected = false`; the node reconnects by itself.

### The catalog

JSON, one message per node:

```json
{
  "node": "berlin",
  "api": "http://10.0.0.5:7777",
  "databases": [
    {
      "id": 3,
      "name": "Sales",
      "modified": 1761650000,
      "cubes": [
        { "name": "Revenue", "modified": 1761650000, "cells": 120431 }
      ]
    }
  ]
}
```

- `modified` is Unix seconds, the time of the last change to a cube's cells (a database: to any cube or dimension). `null` means unknown, for example a database that isn't loaded.
- Only normal cubes are listed, not system or attribute cubes.
- **Copies of friends' databases are not listed.** Each node gets a database from the node that calculated it. Otherwise three friends would keep importing each other's copies.

### Importing: when and how

A friend's database `Sales` becomes the database **`<friend>/Sales`** on your node, for example `berlin/Sales`. The node fetches it with the friend's `GET {api}/api/v1/databases/{id}/export` (`api` is the address from the friends service, as *your* node reaches it) and loads it with the engine's import. If a copy from an earlier import exists, its contents are replaced **in place**: it keeps its id, and requests that are already running finish on the old contents.

The node remembers, per friend and database, the friend's `modified` as it was announced **when the copy was made**. A database counts as newer when the friend announces a higher `modified` than that one. Clocks of different nodes are never compared. The memory is kept in `mesh-imports.json` in `FLUXOLAP_DATA_DIR`, so a restart doesn't fetch everything again (without a data dir it is in memory, like the databases).

What happens with a new or newer database is decided by **import rules**, `FLUXOLAP_MESH_IMPORT`: a comma-separated list of `friend/database=mode`, first match wins, `*` matches anything:

```
FLUXOLAP_MESH_IMPORT='berlin/Sales=auto,*/Weather=auto,*/*=notify'
```

| Mode | Meaning |
|---|---|
| `auto` | Imported as soon as the friend announces it. |
| `notify` | Only shown (`state` `new` or `update`); imported when you ask. **The default.** |
| `off` | Not imported by itself; shown, and an explicit request still works. |

A bare mode, `FLUXOLAP_MESH_IMPORT=auto`, means `*/*=auto`. A database name can't contain a comma.

Safeguards:
- A local database that is named like a copy would be (`berlin/Sales`) but wasn't made by the mesh is **never overwritten**; the import fails and says so in `error`.
- A failed import isn't retried until the friend's database changes (or you ask again).

### REST

Both are `404` (`mesh_disabled`) on a server without the mesh.

- `GET /api/v1/mesh`: this node and its friends. Per friend `id`, `mqtt`, `api`, `connected`, and its databases, each with `name`, `id`, `modified`, `cubes`, `local_name`, `policy`, `state`, `imported_modified`, `imported_at`, `error`.
  `state` compares with your copy: `new` (none yet), `current`, `update` (the friend's database changed since), `unknown` (the friend doesn't know when it changed), `importing`.
- `POST /api/v1/mesh/friends/{friend}/databases/{database}/import`: import now, whatever the rules say. The friend is asked over the mesh first (the answer is current, not the last catalog), then the database is fetched and imported. `404` if there's no such friend or the friend has no such database; `502` if the friend can't be reached or the transfer failed. Answers with the database as `GET /api/v1/mesh` shows it.

## Setting up a node

Build the server with the mesh (off by default, it adds the MQTT broker to the binary):

```bash
cargo build --release -p fluxolap-server --features mesh
```

It is active only when `FLUXOLAP_MESH_NODE` is set:

| Variable | Meaning |
|---|---|
| `FLUXOLAP_MESH_NODE` | This node's id, as the friends service knows it. No `/`, `+`, `#` or spaces. Setting it turns the mesh on. |
| `FLUXOLAP_MESH_API` | **Required.** Base URL of this node's REST API as friends reach it, e.g. `http://10.0.0.5:7777`. |
| `FLUXOLAP_MESH_FRIENDS_URL` | **Required.** URL of the friends service (below). |
| `FLUXOLAP_MESH_LISTEN` | Where this node's broker listens for friends. Default `0.0.0.0:1883`. |
| `FLUXOLAP_MESH_IMPORT` | The import rules (above). Default: nothing is imported by itself. |
| `FLUXOLAP_MESH_FRIENDS_TOKEN` | Optional. Sent to the friends service as a bearer token. |
| `FLUXOLAP_MESH_POLL_SECS` | How often to ask the friends service. Default 10. |

For a try-out on one machine, with the minimal friends service:

```bash
cat > nodes.json <<'JSON'
[
  {"id": "a", "mqtt": "127.0.0.1:1883", "api": "http://127.0.0.1:7777"},
  {"id": "b", "mqtt": "127.0.0.1:1884", "api": "http://127.0.0.1:7778"}
]
JSON
cargo run -p fluxolap-mesh --bin fluxolap-friends -- nodes.json 7780 &

# a imports everything b has; b only shows what a has
FLUXOLAP_MESH_NODE=a FLUXOLAP_MESH_API=http://127.0.0.1:7777 FLUXOLAP_MESH_LISTEN=0.0.0.0:1883 \
FLUXOLAP_MESH_FRIENDS_URL=http://127.0.0.1:7780/friends FLUXOLAP_MESH_IMPORT='b/*=auto' FLUXOLAP_PORT=7777 \
  cargo run -p fluxolap-server --features mesh &
FLUXOLAP_MESH_NODE=b FLUXOLAP_MESH_API=http://127.0.0.1:7778 FLUXOLAP_MESH_LISTEN=0.0.0.0:1884 \
FLUXOLAP_MESH_FRIENDS_URL=http://127.0.0.1:7780/friends FLUXOLAP_PORT=7778 \
  cargo run -p fluxolap-server --features mesh &

curl localhost:7777/api/v1/mesh        # what a sees of b
```

Any MQTT client can watch the catalogs, e.g. `mosquitto_sub -h 127.0.0.1 -p 1883 -t 'fluxolap/+/catalog'`.

## The friends service API

fluxolap does not decide who is a friend. An external service does, the same layer that handles authentication. It is one endpoint:

```
GET <FLUXOLAP_MESH_FRIENDS_URL>?node=<node id>
Authorization: Bearer <FLUXOLAP_MESH_FRIENDS_TOKEN>      (only if a token is configured)
```

`node` is the id of the node asking. The answer is `200` with JSON, the friends of **that** node:

```json
{
  "friends": [
    { "id": "berlin", "mqtt": "10.0.0.5:1883", "api": "http://10.0.0.5:7777" },
    { "id": "wien",   "mqtt": "10.0.0.9:1883", "api": "http://10.0.0.9:7777" }
  ]
}
```

| Field | Meaning |
|---|---|
| `id` | The friend's node id (its `FLUXOLAP_MESH_NODE`). Same rules as above. |
| `mqtt` | `host:port` of the friend's mesh broker, as **this** node reaches it. |
| `api` | Base URL of the friend's REST API, as **this** node reaches it. Databases are fetched from here. |

Rules for an implementation:

- The list may differ per asking node, and friendship does not have to be symmetric.
- The node itself may be in the list; it ignores it.
- It is polled, not pushed: a change takes effect within `FLUXOLAP_MESH_POLL_SECS`. A friend that disappears from the list is dropped, a friend whose address changes is reconnected.
- If the service is unreachable or answers with an error, the node keeps the friends it has and tries again.
- Entries with an invalid `id` are ignored (with a warning).
- Requests over the mesh are answered only for nodes the service lists as friends.

`fluxolap-friends <nodes.json> [port]` is the minimal implementation: every node in the file is a friend of every other. Use it for tests; implement the above for real use.

## Sensor data

A second broker, **on the local machine**, that devices publish to. It is a different broker from the one friends connect to, so a friend can't inject readings. Off unless `FLUXOLAP_SENSOR_LISTEN` is set (it needs the `mesh` build feature, but not the mesh itself):

| Variable | Meaning |
|---|---|
| `FLUXOLAP_SENSOR_LISTEN` | Where the sensor broker listens, e.g. `127.0.0.1:1885`. Keep it on `127.0.0.1` unless the network in front of it decides who may publish. |
| `FLUXOLAP_SENSOR_API` | This node's own REST API. Default `http://127.0.0.1:$FLUXOLAP_PORT`. |

A message on `fluxolap/sensor/<database>/<cube>/cells` with the body of `PUT /api/v1/databases/{db}/cubes/{cube}/cells` is written to that cube:

```bash
mosquitto_pub -p 1885 -t fluxolap/sensor/Plant/Readings/cells \
  -m '{"items": [{"path": ["boiler-1"], "value": 21.5}]}'
```

The node writes through its own REST API, so a sensor write behaves like any other write (the same checks, errors and splashing). Database and cube are addressed by name, so their names can't contain `/`, `+` or `#`. A message that can't be written (unknown database, cube or element, bad body) is logged and dropped. Messages are written in the order they arrive; if more arrive than can be written, the excess is dropped (with a warning).

## Trust

- **No authentication in fluxolap.** Who may connect to a node's broker or REST port is decided by the layer above (network, VPN, reverse proxy). Everyone who can reach a node's mesh port is treated as a friend; the friends service says which nodes that are.
- **The broker can't forbid a friend to publish.** The embedded broker (`rumqttd`) has login-only authentication and no per-topic access control. What keeps your cubes safe is that nothing turns a received message into a write; the only thing a friend can cause is an import of its own database when you asked for it or said `auto`. A misbehaving friend can at worst publish a wrong catalog to the topic on your broker, so other friends would be told wrong things about you. If that matters, sign catalogs or check them against the REST API. A catalog whose `node` is not the friend it came from is ignored.
- **`auto` trusts the friend's data.** An imported database replaces your copy of it, whatever it contains. Use `auto` only for friends and databases you'd import anyway.

## Not there

- Release builds with `--features mesh`.
- A `notify` push to a UI; today `GET /api/v1/mesh` is polled.
- More than a handful of friends hasn't been tried (a full mesh of 12 is 11 connections per node).
