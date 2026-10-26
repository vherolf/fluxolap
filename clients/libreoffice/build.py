"""Builds the LibreOffice extension: target/fluxolap.oxt (or the path given).

    python3 clients/libreoffice/build.py [OUT.oxt]
    python3 clients/libreoffice/build.py --idl     # also recompile idl/fluxolap.idl

--idl needs the LibreOffice SDK (unoidl-write; on Debian/Ubuntu the
libreoffice-dev package); without it the committed fluxolap.rdb is used.
Standard library only.
"""

import os
import re
import subprocess
import sys
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, "..", ".."))
EXT = os.path.join(HERE, "extension")
RDB = os.path.join(HERE, "fluxolap.rdb")
PROGRAM = "/usr/lib/libreoffice/program"


def compile_idl():
    tool = os.environ.get("UNOIDL_WRITE") or next(
        (p for p in ("/usr/lib/libreoffice/sdk/bin/unoidl-write", "/opt/libreoffice/sdk/bin/unoidl-write") if os.path.exists(p)), "unoidl-write")
    types = [os.path.join(PROGRAM, "types.rdb"), os.path.join(PROGRAM, "types", "offapi.rdb")]
    subprocess.run([tool, *types, os.path.join(HERE, "idl", "fluxolap.idl"), RDB], check=True)
    print(f"compiled {os.path.relpath(RDB, ROOT)}")


def version():
    with open(os.path.join(ROOT, "Cargo.toml"), encoding="utf-8") as f:
        m = re.search(r'^\[workspace\.package\][^\[]*?^version\s*=\s*"([^"]+)"', f.read(), re.M | re.S)
    return m.group(1) if m else "0.0.0"


def build(out):
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        for folder, _, files in os.walk(EXT):
            for name in sorted(files):
                if name.endswith(".pyc"):
                    continue
                path = os.path.join(folder, name)
                arc = os.path.relpath(path, EXT)
                if arc == "description.xml":
                    with open(path, encoding="utf-8") as f:
                        z.writestr(arc, f.read().replace("@VERSION@", version()))
                else:
                    z.write(path, arc)
        z.write(RDB, "fluxolap.rdb")
    print(f"built {os.path.relpath(out, ROOT)} ({os.path.getsize(out)} bytes, version {version()})")


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if a != "--idl"]
    if "--idl" in sys.argv or not os.path.exists(RDB):
        compile_idl()
    build(os.path.abspath(args[0]) if args else os.path.join(ROOT, "target", "fluxolap.oxt"))
