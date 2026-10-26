#!/usr/bin/env bash
# Starts fluxolap with the example databases and prints how to connect
# with fluxoscope (the web app) and fluxoterm (the terminal app).
#
# Works from an unpacked release (fluxolap, fluxoterm, ui/ and examples/
# next to this script) and from a source checkout (target/release,
# target/examples, target/ui).
#
#   ./start.sh                         port 7777
#   FLUXOLAP_PORT=8080 ./start.sh      another port
#   FLUXOLAP_DATA_DIR=./data ./start.sh   keep changes (saved on stop)
#
# Ctrl+C stops the server (it saves to FLUXOLAP_DATA_DIR first, if set).

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
port="${FLUXOLAP_PORT:-7777}"

# The first of the given paths that exists (-e), or nothing.
first() {
    for p in "$@"; do
        if [ -e "$p" ]; then
            echo "$p"
            return
        fi
    done
}

server="$(first "$here/fluxolap" "$here/target/release/fluxolap")"
term="$(first "$here/fluxoterm" "$here/target/release/fluxoterm")"
examples="$(first "$here/examples/"*.bin.zst "$here/target/examples/"*.bin.zst)"
ui="$(first "$here/ui/index.html" "$here/target/ui/index.html")"

# A source checkout without the web app built yet: build it once with trunk
# (rustup's toolchain, which has the WebAssembly target, comes first).
# FLUXOLAP_UI_DIR set but empty turns the web app off.
ui_off=""
if [ -n "${FLUXOLAP_UI_DIR+set}" ] && [ -z "$FLUXOLAP_UI_DIR" ]; then
    ui_off=1
fi

if [ -z "$ui" ] && [ -z "$ui_off" ] && [ -d "$here/crates/fluxoscope" ]; then
    trunk="$(command -v trunk || first "$HOME/.cargo/bin/trunk")"
    if [ -n "$trunk" ]; then
        echo "Building fluxoscope (the web app) into target/ui -- only this once, about a minute ..."
        if (cd "$here/crates/fluxoscope" && PATH="$HOME/.cargo/bin:$PATH" "$trunk" build --release --cargo-profile wasm-release --dist ../../target/ui) >"${TMPDIR:-/tmp}/fluxoscope-build.log" 2>&1; then
            ui="$here/target/ui/index.html"
        else
            echo "  the build failed -- see ${TMPDIR:-/tmp}/fluxoscope-build.log; starting without the web app" >&2
        fi
    fi
fi

if [ -z "$server" ]; then
    echo "fluxolap not found next to this script or in target/release." >&2
    echo "In a source checkout, build it first: cargo build --release -p fluxolap-server -p fluxoterm" >&2
    exit 1
fi

# Something on the port already? (bash's /dev/tcp, so curl isn't needed.)
if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
    echo "Port $port is already in use -- is fluxolap running? Use another: FLUXOLAP_PORT=$((port + 1)) $0" >&2
    exit 1
fi

export FLUXOLAP_PORT="$port"
if [ -n "$examples" ]; then
    export FLUXOLAP_EXAMPLES="${FLUXOLAP_EXAMPLES-$(dirname "$examples")}"
fi
if [ -n "$ui" ] && [ -z "${FLUXOLAP_UI_DIR+set}" ]; then
    export FLUXOLAP_UI_DIR="$(dirname "$ui")"
fi

log="${TMPDIR:-/tmp}/fluxolap-$port.log"
"$server" >"$log" 2>&1 &
pid=$!

stop() {
    trap - INT TERM
    echo
    echo "Stopping fluxolap ..."
    kill -TERM "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    exit 0
}
trap stop INT TERM

# Wait until it answers (the examples load before it listens).
for _ in $(seq 1 300); do
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "fluxolap stopped right away -- its log ($log):" >&2
        cat "$log" >&2
        exit 1
    fi
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
        break
    fi
    sleep 0.1
done

# This machine's address on the network, for other computers.
lan="$(hostname -I 2>/dev/null | awk '{print $1}' || true)"
if [ -z "$lan" ] && command -v ipconfig >/dev/null; then
    lan="$(ipconfig getifaddr en0 2>/dev/null || true)"
fi

bold="" dim="" off=""
if [ -t 1 ]; then
    bold=$'\e[1m' dim=$'\e[2m' off=$'\e[0m'
fi

echo
echo "${bold}fluxolap is running${off} on port $port  ${dim}(pid $pid, log: $log)${off}"
if [ -n "$examples" ] && [ -n "${FLUXOLAP_EXAMPLES:-}" ]; then
    names="$(for f in "$FLUXOLAP_EXAMPLES"/*.bin.zst; do n="$(basename "$f" .bin.zst)"; [ "$n" = registry ] || echo "${n#db-*-}"; done | paste -sd ' ' -)"
    echo "  examples:  $names"
else
    echo "  examples:  none found (build them in a source checkout: cargo run --release -p xtask -- examples target/examples)"
fi
[ -n "${FLUXOLAP_DATA_DIR:-}" ] && echo "  data:      saved to $FLUXOLAP_DATA_DIR on stop" || echo "  data:      in memory only (FLUXOLAP_DATA_DIR=<dir> keeps changes)"
echo
echo "${bold}fluxoscope${off} (web app) -- open in a browser:"
if [ -n "$ui_off" ]; then
    echo "  turned off (FLUXOLAP_UI_DIR is empty)"
elif [ -n "${FLUXOLAP_UI_DIR:-}" ]; then
    echo "  http://localhost:$port/ui/"
    [ -n "$lan" ] && echo "  http://$lan:$port/ui/   ${dim}(from other computers on your network)${off}"
else
    echo "  not available: no ui/ folder found, and trunk (to build it) isn't installed:"
    echo "  rustup target add wasm32-unknown-unknown && cargo install trunk --locked, then run $0 again"
fi
echo
echo "${bold}fluxoterm${off} (terminal app) -- in another terminal:"
if [ -n "$term" ]; then
    # Relative when run from its own folder: ./fluxoterm
    [ "$PWD" = "$here" ] && term="./${term#"$here"/}"
    echo "  $term http://localhost:$port"
    echo "  $term +stats http://localhost:$port   ${dim}(starts in the server statistics)${off}"
else
    echo "  cargo run --release -p fluxoterm -- http://localhost:$port"
fi
if [ -n "$lan" ]; then
    echo "  from another computer:  fluxoterm http://$lan:$port"
fi
echo "  over SSH:  ssh -N -L 17777:localhost:$port <user>@<this-host>, then  fluxoterm http://localhost:17777"
echo
echo "${bold}REST API${off}:  http://localhost:$port/api/v1   ${dim}docs: http://localhost:$port/api/v1/docs${off}"
echo
echo "Press Ctrl+C to stop."
wait "$pid"
