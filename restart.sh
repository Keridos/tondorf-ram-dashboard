#!/usr/bin/env bash
# (Re)starts the RAM dashboard in the background on port 63151.
# Builds first, so a failing build leaves the running instance untouched.
# PID: ~/.ramdashboard/server.pid, log: ~/.ramdashboard/server.log
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")"

data_dir="$HOME/.ramdashboard"
pid_file="$data_dir/server.pid"
log_file="$data_dir/server.log"
port=63151
mkdir -p "$data_dir"

if command -v cargo >/dev/null; then
    cargo build
else
    nix-shell -p cargo rustc gcc --run 'cargo build'
fi

# Stop the previous instance (its whole process group: nix-shell, cargo, server).
if [[ -f $pid_file ]]; then
    old=$(<"$pid_file")
    if kill -0 "$old" 2>/dev/null; then
        kill -- "-$old" 2>/dev/null || kill "$old"
        for _ in $(seq 50); do kill -0 "$old" 2>/dev/null || break; sleep 0.1; done
    fi
    rm -f "$pid_file"
fi

# New session, so the server outlives this shell and its group can be killed.
ROCKET_PORT=$port setsid nohup ./run.sh >"$log_file" 2>&1 </dev/null &
echo $! >"$pid_file"

# Wait for the health check.
for _ in $(seq 100); do
    if curl -fsS "http://127.0.0.1:$port/api/health" >/dev/null 2>&1; then
        echo "running on port $port (pid $(<"$pid_file")), log: $log_file"
        exit 0
    fi
    kill -0 "$(<"$pid_file")" 2>/dev/null || break
    sleep 0.2
done
echo "server did not come up; last log lines:" >&2
tail -n 20 "$log_file" >&2
exit 1
