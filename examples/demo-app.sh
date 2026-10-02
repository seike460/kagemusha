#!/bin/sh
# Demo application supervised by kagemusha: a PID-1-friendly stub that
# traps signals like a real workload would.
trap 'echo "demo-app: TERM/INT received"; exit 0' TERM INT
echo "demo-app: pid=$$ starting"
while :; do sleep 86400; done
