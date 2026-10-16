#!/usr/bin/env bash
# TaoBench as one benchmark command for run.sh. The server runs here,
# in the recorded scope; the clients run in a transient scope of their
# own, so the recorder sees the server alone, as it would a container.
# Usage: experiments/taobench.sh SECS
# Needs tao_bench_server and tao_bench_client from install_taobench.sh.
# Sizes follow DCPerf's run.py scaled to a small server: DCPerf gives
# the server three quarters of the logical cores as fast threads, a
# third as many slow threads and a quarter as many dispatchers, and
# sizes the key space for a 0.9 hit ratio on 434-byte items.
set -euo pipefail
secs=$1
prefix=${TAOBENCH_PREFIX:-${XDG_DATA_HOME:-$HOME/.local/share}/taobench}
share=$prefix/share/taobench
port=${TAOBENCH_PORT:-11211}
mem_mb=${TAOBENCH_MEM_MB:-2048}
fast=${TAOBENCH_FAST_THREADS:-8}
slow=$(( fast / 3 > 0 ? fast / 3 : 1 ))
dispatchers=$(( fast / 4 > 0 ? fast / 4 : 1 ))
client_threads=${TAOBENCH_CLIENT_THREADS:-8}
clients=${TAOBENCH_CLIENTS_PER_THREAD:-380}
keys=$(( mem_mb * 1024 * 1024 / 434 * 807 / 900 ))   # items / 0.9, times DCPerf's 0.807

opts="lru_crawler,tao_it_gen_file=$share/leader_sizes.json,tao_max_item_size=65536"
opts+=",tao_gen_payload=0,tao_slow_dispatchers=$dispatchers,tao_num_slow_threads=$slow"
opts+=",tao_max_slow_reqs=1024,tao_worker_sleep_ns=100,tao_dispatcher_sleep_ns=100"
opts+=",tao_slow_sleep_ns=100,tao_slow_path_sleep_us=0,tao_compress_items=1"
opts+=",tao_stats_sleep_ms=5000,tao_slow_use_semaphore=0,tao_pin_threads=0,tao_smart_nanosleep=0"
opts+=",ssl_chain_cert=$share/certs/example.crt,ssl_key=$share/certs/example.key"
"$prefix/bin/tao_bench_server" -c 180000 -m "$mem_mb" -t "$fast" -B binary \
  -p "$port" -I 16m -l 127.0.0.1 -Z -o "$opts" &
server=$!
trap 'kill $server 2>/dev/null; wait $server 2>/dev/null || true' EXIT
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && break
  sleep 0.1
done

systemd-run --user --scope --quiet --unit="taobench-clients-$$" --collect \
  "$prefix/bin/tao_bench_client" -s 127.0.0.1 -p "$port" -P memcache_binary \
  --key-pattern=R:R --distinct-client-seed --randomize -R --hide-histogram \
  --expiry-range=1800-1802 --data-size-range=8191-8193 --ratio=0:1 \
  --key-minimum=1 --key-maximum="$keys" -t "$client_threads" --clients="$clients" \
  --threads-coherence=0 --clients-coherence=3 --key-bytes=220 --test-time="$secs" \
  -4 --cert="$share/certs/example.crt" --key="$share/certs/example.key" \
  --tls --tls-skip-verify
