"""Writes one example as a fluxolap *source package*: the small, generic
format `crates/fluxolap-examples` turns into a fluxolap database.

A package is a directory with a `manifest.json` and gzipped CSV files:

- dimension file, columns `element,type,parent,weight`: one row per
  element, plus one more row per extra parent. `type` is `numeric` or
  `string` (taken from the element's first row); an empty `parent` makes
  a root. Elements are created in the order they first appear.
- cube file, columns `<dimension>...,value`: one row per stored base cell,
  elements by name. A cell whose path contains a `string` element holds
  a string, every other cell a number.
- attribute file, columns `element,value`: string attribute values,
  written into the dimension's automatic `#_<dimension>_` attribute cube.

Maintainer-only: `fetch.py` scripts use this with the Python standard
library alone, so no pip install is needed to refresh the data.
"""

import csv
import gzip
import io
import json
import os


def _open(path):
    # mtime=0 keeps the gzip bytes identical across runs with identical data.
    return gzip.GzipFile(path, "wb", compresslevel=9, mtime=0)


def _csv(path, header, rows):
    with _open(path) as raw:
        text = io.TextIOWrapper(raw, encoding="utf-8", newline="")
        w = csv.writer(text, lineterminator="\n")
        w.writerow(header)
        n = 0
        for row in rows:
            w.writerow(row)
            n += 1
        text.flush()
        text.detach()
    return n


def fmt(v):
    """Numbers without float noise: integers bare, others shortest round-trip."""
    if isinstance(v, float):
        if v == int(v) and abs(v) < 1e15:
            return str(int(v))
        return repr(v)
    return str(v)


class Package:
    def __init__(self, directory, database, order, description, sources):
        self.dir = directory
        self.manifest = {
            "database": database,
            "order": order,
            "description": description,
            "sources": sources,
            "dimensions": [],
            "cubes": [],
            "attributes": [],
        }

    def dimension(self, name, rows):
        """rows: iterable of (element, type, parent, weight) -- parent '' for a root."""
        file = f"dim_{_slug(name)}.csv.gz"
        n = _csv(os.path.join(self.dir, file), ["element", "type", "parent", "weight"], ((e, t, p, fmt(w)) for e, t, p, w in rows))
        self.manifest["dimensions"].append({"name": name, "file": file})
        print(f"  dimension {name}: {n} rows")

    def cube(self, name, dimensions, cells):
        """cells: iterable of (element per dimension..., value)."""
        file = f"cube_{_slug(name)}.csv.gz"
        n = _csv(os.path.join(self.dir, file), list(dimensions) + ["value"], ((*c[:-1], fmt(c[-1])) for c in cells))
        self.manifest["cubes"].append({"name": name, "dimensions": list(dimensions), "file": file})
        print(f"  cube {name}: {n} cells")

    def attribute(self, dimension, attribute, values):
        """values: iterable of (element, string value)."""
        file = f"attr_{_slug(dimension)}_{_slug(attribute)}.csv.gz"
        n = _csv(os.path.join(self.dir, file), ["element", "value"], values)
        self.manifest["attributes"].append({"dimension": dimension, "attribute": attribute, "file": file})
        print(f"  attribute {dimension}.{attribute}: {n} values")

    def write(self):
        with open(os.path.join(self.dir, "manifest.json"), "w", encoding="utf-8") as f:
            json.dump(self.manifest, f, indent=2, ensure_ascii=False)
            f.write("\n")


def _slug(name):
    return "".join(c if c.isalnum() else "_" for c in name.lower()).strip("_")


def cache_path(name):
    """Where fetch scripts keep raw downloads: outside the repo, reused across runs."""
    d = os.environ.get("FLUXOLAP_EXAMPLES_CACHE", os.path.join(os.path.expanduser("~"), ".cache", "fluxolap-examples"))
    os.makedirs(d, exist_ok=True)
    return os.path.join(d, name)


def download(url, name):
    """Downloads `url` into the cache once; returns the local path."""
    import urllib.request

    path = cache_path(name)
    if not os.path.exists(path):
        print(f"  downloading {url}")
        req = urllib.request.Request(url, headers={"User-Agent": "fluxolap-examples"})
        with urllib.request.urlopen(req, timeout=600) as r, open(path + ".part", "wb") as f:
            while chunk := r.read(1 << 20):
                f.write(chunk)
        os.replace(path + ".part", path)
    return path
