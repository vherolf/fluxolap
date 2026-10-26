"""The fluxolap REST API for the Calc functions: no LibreOffice code here,
so it's tested with plain Python (tests/test_client.py).

Calc evaluates one formula at a time, so a sheet with thousands of
FLUX.DATA cells would make thousands of requests. Two things prevent that:

- Answers are cached for a moment (`FRESH_FOR`): within one recalculation
  every formula is answered from what was read for it.
- Each cube remembers which cells a sheet asked for. When its cache has
  gone stale (the next recalculation), the first request reads *all* of
  them again in one batch, so only the very first calculation of a new
  sheet asks cell by cell.

Standard library only (LibreOffice's bundled Python has nothing else).
"""

import http.client
import json
import threading
import time
import urllib.parse

# How long a value read from the server is used without asking again, in
# seconds. The cache is this client's own (one per server address, in
# LibreOffice's process); the server caches nothing for it. Long enough for
# one recalculation even when it reads consolidated values: the server sums
# them from one scan of the cube's stored cells per request (~35 ms for
# WorldPopulation's 304,369 cells, however many totals the request asks
# for), so a new sheet's first cell-by-cell formulas can take seconds. The price: a
# manual recalculation (Ctrl+Shift+F9) within this time of the last one
# answers from the cache; "Refresh values" (`forget`) always reads the
# server's current values.
FRESH_FOR = 10.0
# Cells per batch request.
BATCH = 50_000
# Connect/read timeout in seconds.
TIMEOUT = 30


class FluxError(Exception):
    """A problem worth showing in the cell (the server's problem `detail`);
    `index`: which item of a batch request it was about, if the server said."""

    def __init__(self, message, index=None):
        super().__init__(message)
        self.index = index


def _problem(status, text):
    try:
        p = json.loads(text)
    except ValueError:
        p = None
    if not isinstance(p, dict):
        p = {}
    return FluxError(p.get("detail") or f"HTTP {status}", p.get("index"))


def element_name(value):
    """A spreadsheet value as an element name: 1950.0 -> "1950"."""
    if isinstance(value, float) and value.is_integer():
        return str(int(value))
    return str(value).strip()


def split_url(server):
    """`localhost:7777`, `http://host:7777/` or `https://host` -> (scheme, host, port)."""
    server = server.strip()
    if "://" not in server:
        server = "http://" + server
    u = urllib.parse.urlsplit(server)
    if u.scheme not in ("http", "https") or not u.hostname:
        raise FluxError(f"not a server address: {server}")
    return u.scheme, u.hostname, u.port or (443 if u.scheme == "https" else 80)


class Server:
    """One fluxolap server: a kept-alive connection, ids by name, cached cells."""

    def __init__(self, address):
        self.scheme, self.host, self.port = split_url(address)
        self._conn = None
        self._lock = threading.RLock()
        self._ids = {}  # cache keys -> ids (databases, cubes, dimensions)
        self._cells = {}  # (db, cube) -> {path tuple: (value, read at)}
        self._asked = {}  # (db, cube) -> {path tuple: None}, in the order first asked

    # -- HTTP --------------------------------------------------------------

    def _connection(self):
        if self._conn is None:
            cls = http.client.HTTPSConnection if self.scheme == "https" else http.client.HTTPConnection
            self._conn = cls(self.host, self.port, timeout=TIMEOUT)
        return self._conn

    def close(self):
        with self._lock:
            if self._conn is not None:
                self._conn.close()
                self._conn = None

    def request(self, method, path, body=None):
        """JSON in, JSON out; a problem response becomes a FluxError."""
        data = None if body is None else json.dumps(body).encode()
        headers = {"Accept": "application/json"}
        if data is not None:
            headers["Content-Type"] = "application/json"
        with self._lock:
            for attempt in (1, 2):  # a kept-alive connection the server closed: once more
                conn = self._connection()
                try:
                    conn.request(method, "/api/v1" + path, body=data, headers=headers)
                    resp = conn.getresponse()
                    raw = resp.read()
                    break
                except (http.client.HTTPException, OSError) as e:
                    conn.close()
                    self._conn = None
                    if attempt == 2:
                        raise FluxError(f"can't reach {self.host}:{self.port} ({e})") from None
        text = raw.decode("utf-8", "replace")
        if resp.status >= 400:
            raise _problem(resp.status, text)
        return json.loads(text) if text else None

    # -- names to ids --------------------------------------------------------

    def _find(self, key, path, name, what):
        ids = self._ids.get(key)
        if ids is None or name not in ids:
            items = self.request("GET", path)["items"]
            ids = {i["name"]: i for i in items}
            self._ids[key] = ids
        if name not in ids:
            raise FluxError(f"{what} '{name}' not found")
        return ids[name]

    def database(self, name):
        return self._find(("db",), "/databases", name, "database")["id"]

    def cube(self, db, name):
        return self._find(("cube", db), f"/databases/{db}/cubes", name, "cube")

    def dimension(self, db, name):
        return self._find(("dim", db), f"/databases/{db}/dimensions", name, "dimension")["id"]

    # -- cells ---------------------------------------------------------------

    def values(self, database, cube, paths):
        """The values at `paths` (tuples of element names) of `database`.`cube`:
        a number, a string, or None for an empty cell. A path the server
        refuses (an unknown element) raises its FluxError."""
        db = self.database(database)
        c = self.cube(db, cube)
        n = len(c["dimensions"])
        for p in paths:
            if len(p) != n:
                raise FluxError(f"cube '{cube}' has {n} dimensions, {len(p)} elements given")
        key = (db, c["id"])
        with self._lock:
            cache = self._cells.setdefault(key, {})
            asked = self._asked.setdefault(key, {})
            now = time.monotonic()
            missing = [p for p in paths if p not in cache or now - cache[p][1] > FRESH_FOR]
            if missing:
                wanted = set(paths)
                for p in paths:
                    asked.setdefault(p, None)
                # Everything this sheet asked for that has gone stale, in one
                # go -- except paths known to be refused, unless asked for now.
                stale = [p for p in asked if (p not in cache or now - cache[p][1] > FRESH_FOR)
                         and (p in wanted or not isinstance(cache[p][0], FluxError))]
                self._read(db, c["id"], stale, cache)
            values = [cache[p][0] for p in paths]
        for v in values:
            if isinstance(v, FluxError):
                raise FluxError(str(v))
        return values

    def peek(self, database, cube, path):
        """(True, value) if `path` has a fresh cached value -- without asking
        the server -- else (False, None). A cached refusal raises it."""
        db = self.database(database)
        key = (db, self.cube(db, cube)["id"])
        with self._lock:
            entry = self._cells.get(key, {}).get(path)
        if entry is None or time.monotonic() - entry[1] > FRESH_FOR:
            return False, None
        if isinstance(entry[0], FluxError):
            raise FluxError(str(entry[0]))
        return True, entry[0]

    def prefetch(self, database, cube, paths):
        """Reads `paths` into the cache in one batch (with every other stale
        cell the sheet asked for); refused paths are remembered, not raised."""
        try:
            self.values(database, cube, list(paths))
        except FluxError:
            pass

    def _read(self, db, cube, paths, cache):
        """Reads `paths` in batches. A refused path fails its whole batch; the
        server says which one (`index`), so that one is remembered as
        refused and the rest of the batch is asked for again."""
        todo = list(paths)
        while todo:
            chunk = todo[:BATCH]
            try:
                items = self.request("POST", f"/databases/{db}/cubes/{cube}/cells/query", {"paths": [list(p) for p in chunk]})["items"]
            except FluxError as e:
                if e.index is not None and 0 <= e.index < len(chunk):
                    cache[todo.pop(e.index)] = (FluxError(str(e)), time.monotonic())
                    continue
                # No position given (shouldn't happen): one by one.
                if len(chunk) == 1:
                    cache[todo.pop(0)] = (FluxError(str(e)), time.monotonic())
                    continue
                for p in chunk:
                    self._read(db, cube, [p], cache)
                todo = todo[len(chunk):]
                continue
            now = time.monotonic()
            for p, item in zip(chunk, items):
                cache[p] = (item["value"], now)
            todo = todo[len(chunk):]

    def write(self, database, cube, path, value):
        """Writes one cell (a number, or a string for a string cell); at a
        consolidated path the server splashes the value down to the base
        cells. Every cached value of the cube goes stale, so the next
        calculation reads them again (in one batch)."""
        db = self.database(database)
        c = self.cube(db, cube)
        if len(path) != len(c["dimensions"]):
            raise FluxError(f"cube '{cube}' has {len(c['dimensions'])} dimensions, {len(path)} elements given")
        self.request("PUT", f"/databases/{db}/cubes/{c['id']}/cells", {"items": [{"path": list(path), "value": value}]})
        with self._lock:
            for k in self._cells.get((db, c["id"]), {}):
                v, _ = self._cells[(db, c["id"])][k]
                self._cells[(db, c["id"])][k] = (v, float("-inf"))

    def area(self, database, cube, lists):
        """A block: `lists` holds one list of element names per cube dimension.
        Returns (row names, column names, values[row][col]); the first
        dimension with several elements is the rows, the second the columns."""
        db = self.database(database)
        c = self.cube(db, cube)
        if len(lists) != len(c["dimensions"]):
            raise FluxError(f"cube '{cube}' has {len(c['dimensions'])} dimensions, {len(lists)} given")
        multi = [i for i, names in enumerate(lists) if len(names) > 1]
        if len(multi) > 2:
            raise FluxError("at most two dimensions may have several elements (rows and columns)")
        row_dim = multi[0] if multi else None
        col_dim = multi[1] if len(multi) > 1 else None
        rows = lists[row_dim] if row_dim is not None else [None]
        cols = lists[col_dim] if col_dim is not None else [None]
        size = len(rows) * len(cols)
        if size > 1_000_000:
            raise FluxError(f"{size:,} cells are too many for one formula")
        body = {"area": [list(names) for names in lists]}
        items = self.request("POST", f"/databases/{db}/cubes/{c['id']}/cells/query", body)["items"]
        # The answer is in area order: the last dimension varies fastest.
        values = {}
        dims = len(lists)
        index = [0] * dims
        for item in items:
            values[tuple(index)] = item["value"]
            for d in reversed(range(dims)):
                index[d] += 1
                if index[d] < len(lists[d]):
                    break
                index[d] = 0
        grid = []
        for r in range(len(rows)):
            line = []
            for k in range(len(cols)):
                at = [0] * dims
                if row_dim is not None:
                    at[row_dim] = r
                if col_dim is not None:
                    at[col_dim] = k
                line.append(values.get(tuple(at)))
            grid.append(line)
        return rows, cols, grid

    def filled(self, database, cube, lists):
        """Which cells of an area (one list of element names per dimension)
        have a value: their paths as element ids (the server leaves the
        empty ones out, so a sparse area comes back small)."""
        db = self.database(database)
        c = self.cube(db, cube)
        body = {"area": [list(names) for names in lists], "skip_empty": True}
        return [tuple(i["path"]) for i in self.request("POST", f"/databases/{db}/cubes/{c['id']}/cells/query", body)["items"]]

    def elements(self, database, dimension, parent=None):
        """Element names of `dimension`, in hierarchy order: all of them, or
        the children of `parent`."""
        db = self.database(database)
        dim = self.dimension(db, dimension)
        items = self.request("GET", f"/databases/{db}/dimensions/{dim}/elements")["items"]
        if parent is None:
            by_id = {e["id"]: e for e in items}
            out, seen = [], set()

            def walk(e):
                if e["id"] in seen:  # an element under two parents: listed once
                    return
                seen.add(e["id"])
                out.append(e["name"])
                for child in e.get("children", []):
                    if child["id"] in by_id:
                        walk(by_id[child["id"]])

            for e in sorted((e for e in items if not e["parents"]), key=lambda e: e["position"]):
                walk(e)
            return out
        by_name = {e["name"]: e for e in items}
        if parent not in by_name:
            raise FluxError(f"element '{parent}' not found in '{dimension}'")
        by_id = {e["id"]: e for e in items}
        return [by_id[c["id"]]["name"] for c in by_name[parent].get("children", [])]

    # -- for the dialogs ----------------------------------------------------

    def databases(self):
        """Database names, in id order."""
        return [d["name"] for d in self.request("GET", "/databases")["items"]]

    def cubes(self, database):
        """The normal cubes of `database`: [(name, [dimension names])]."""
        db = self.database(database)
        dims = {d["id"]: d["name"] for d in self.request("GET", f"/databases/{db}/dimensions")["items"]}
        cubes = self.request("GET", f"/databases/{db}/cubes?type=normal")["items"]
        return [(c["name"], [dims.get(d, f"#{d}") for d in c["dimensions"]]) for c in cubes]

    def element_tree(self, database, dimension):
        """A dimension's elements as the API lists them (with parents and children)."""
        db = self.database(database)
        dim = self.dimension(db, dimension)
        return self.request("GET", f"/databases/{db}/dimensions/{dim}/elements")["items"]

    def examples(self):
        """The example databases the server offers: [{file, name, size, loaded}]."""
        return self.request("GET", "/examples")["items"]

    def load_example(self, file):
        name = self.request("POST", f"/examples/{urllib.parse.quote(file, safe='')}/import")["name"]
        self._ids.clear()
        return name

    def load_file(self, data):
        """Uploads a database file (.bin.zst); returns the database's name."""
        with self._lock:
            conn = self._connection()
            try:
                conn.request("POST", "/api/v1/databases/import", body=data, headers={"Content-Type": "application/octet-stream", "Accept": "application/json"})
                resp = conn.getresponse()
                text = resp.read().decode("utf-8", "replace")
            except (http.client.HTTPException, OSError) as e:
                conn.close()
                self._conn = None
                raise FluxError(f"can't reach {self.host}:{self.port} ({e})") from None
        if resp.status >= 400:
            raise _problem(resp.status, text)
        self._ids.clear()
        return json.loads(text)["name"]

    def forget(self):
        """Every cached value goes stale: the next calculation reads them all again."""
        with self._lock:
            for cells in self._cells.values():
                for k, (v, _) in cells.items():
                    cells[k] = (v, float("-inf"))

    def info(self):
        s = self.request("GET", "")
        return f"{s['name']} {s['version']}"


_servers = {}
_servers_lock = threading.Lock()


def server(address):
    """The `Server` for `address`, shared by every formula that names it."""
    key = address.strip().rstrip("/")
    with _servers_lock:
        if key not in _servers:
            _servers[key] = Server(key)
        return _servers[key]
