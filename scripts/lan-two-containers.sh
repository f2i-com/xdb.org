#!/usr/bin/env bash
# Two XDB nodes in two Linux containers on one Docker bridge network, each in
# its own network namespace: mDNS discovery, GossipSub delivery and
# reconciliation between separate network stacks. The closest thing to the
# two-host LAN check that a single machine can run.
#
#   scripts/lan-two-containers.sh            # build, run, print both results
#   scripts/lan-two-containers.sh --no-build # reuse the xdb-lan-probe image
#
# Each container runs `lan-probe`, writes one record and waits until it holds
# both (its own and the peer's). Exit 0 only when both containers converged.
# Needs a Docker daemon with Linux containers; the image compiles the crate
# with the nightly toolchain from rust-toolchain.toml and no default features
# (the Tauri layer and its GTK/D-Bus system libraries are not needed here).
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=xdb-lan-probe
NET=xdb-lan
TIMEOUT="${XDB_LAN_TIMEOUT:-90}"

if [[ "${1:-}" != "--no-build" ]]; then
  echo "== building $IMAGE (first build compiles the crate; later builds reuse the layer cache)"
  docker build -t "$IMAGE" -f scripts/lan-probe.Dockerfile .
fi

docker network inspect "$NET" >/dev/null 2>&1 || docker network create "$NET" >/dev/null
docker rm -f xdb-lan-a xdb-lan-b >/dev/null 2>&1 || true
cleanup() { docker rm -f xdb-lan-a xdb-lan-b >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "== starting node a"
docker run -d --name xdb-lan-a --network "$NET" "$IMAGE" \
  --data /data --label a --write hello-from-a --expect 2 --timeout "$TIMEOUT" >/dev/null
# A moment later so a is listening when b's first mDNS query goes out; the
# 30 s re-query would recover from a simultaneous start anyway.
sleep 2
echo "== starting node b"
docker run -d --name xdb-lan-b --network "$NET" "$IMAGE" \
  --data /data --label b --write hello-from-b --expect 2 --timeout "$TIMEOUT" >/dev/null

status=0
for name in xdb-lan-a xdb-lan-b; do
  code="$(docker wait "$name")"
  echo "== $name exit $code"
  docker logs "$name" 2>&1 | sed "s/^/   /"
  [[ "$code" == "0" ]] || status=1
done

if [[ "$status" == "0" ]]; then
  echo "== PASS: both containers hold both records (discovery, delivery and reconciliation across network namespaces)"
else
  echo "== FAIL: at least one container did not converge within ${TIMEOUT}s"
fi
exit "$status"
