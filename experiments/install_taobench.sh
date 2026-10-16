#!/usr/bin/env bash
# Build and install TaoBench, DCPerf's memcached-based server and its
# memtier-based client, without the rest of DCPerf.
# Usage: experiments/install_taobench.sh [PREFIX]
# Puts tao_bench_server and tao_bench_client in PREFIX/bin and the
# server's item-size tables and certificates in PREFIX/share/taobench.
# Distribution packages are installed with sudo on Fedora; everything
# else builds and installs as the user. Rerunning skips what is built.
set -euo pipefail
prefix=${1:-${XDG_DATA_HOME:-$HOME/.local/share}/taobench}
build=${TAOBENCH_BUILD:-$prefix/build}
jobs=${JOBS:-$(nproc)}
# DCPerf pins folly v2024.06.24.00, which no longer builds against the
# glog current distributions ship; the server only needs folly's JSON,
# file and random helpers, which any release has.
folly_tag=${FOLLY_TAG:-v2026.10.12.00}
memtier_rev=7bea7c63c5e95fea061366b95494bf730c5ca0d4       # as DCPerf pins it
memcached_ver=1.6.5                                        # as DCPerf pins it
dcperf_url=${DCPERF_URL:-https://github.com/facebookresearch/DCPerf}
log() { echo "== $*" >&2; }
mkdir -p "$build" "$prefix/bin" "$prefix/share/taobench"

if command -v dnf >/dev/null 2>&1; then
  log installing distribution packages
  sudo dnf install -y -q --skip-unavailable cmake autoconf automake libtool \
    patch git gcc-c++ make perl python3 pkgconf-pkg-config curl \
    libevent-devel openssl-devel pcre-devel zlib-ng-compat-devel bzip2-devel \
    xz-devel lz4-devel libzstd-devel snappy-devel libaio-devel libunwind-devel \
    double-conversion-devel libsodium-devel gflags-devel glog-devel fmt-devel \
    boost-devel libdwarf-devel
fi

cd "$build"
if [ ! -d DCPerf ]; then
  log fetching the TaoBench package from DCPerf
  git clone -q --filter=blob:none --sparse --depth 1 "$dcperf_url" DCPerf
fi
git -C DCPerf sparse-checkout set packages/tao_bench packages/common
tb=$build/DCPerf/packages/tao_bench

folly_scratch=$build/folly-scratch
folly_inst=$folly_scratch/installed/folly
if [ ! -f "$folly_inst/lib/pkgconfig/libfolly.pc" ]; then
  log building folly "$folly_tag"
  [ -d folly ] || git clone -q --branch "$folly_tag" --depth 1 https://github.com/facebook/folly
  (
    cd folly
    if git apply --check "$tb/folly_zlib_uri.patch" 2>/dev/null; then
      git apply "$tb/folly_zlib_uri.patch"
    fi
    # getdeps drives any sccache it finds on PATH, and one set up for
    # cargo does not serve C++ builds; hide it. Older folly releases
    # also predate CMake 4's removal of pre-3.5 compatibility.
    path=$PATH
    if sccache=$(command -v sccache); then
      path=$(printf '%s' "$PATH" | tr ':' '\n' | grep -vx "$(dirname "$sccache")" | paste -sd: -)
    fi
    PATH=$path CMAKE_POLICY_VERSION_MINIMUM=3.5 \
      ./build/fbcode_builder/getdeps.py --allow-system-packages build --no-tests \
      --scratch-path "$folly_scratch" --num-jobs "$jobs"
  )
fi

if [ ! -x "$prefix/bin/tao_bench_server" ]; then
  log building the server, memcached "$memcached_ver" with the TaoBench patches
  rm -rf "memcached-$memcached_ver"
  [ -f "memcached-$memcached_ver.tar.gz" ] ||
    curl -fsSL "https://www.memcached.org/files/memcached-$memcached_ver.tar.gz" \
      -o "memcached-$memcached_ver.tar.gz"
  tar xzf "memcached-$memcached_ver.tar.gz"
  (
    cd "memcached-$memcached_ver"
    for p in 0001-tao_bench_memcached 0002-tao_bench_memcached_oom_handling \
             0003-tao_bench_thread_pool_naming 0006-tao_bench_slow_thread_use_semaphore \
             0007-tao_bench_smart_nanosleep 0008-tao_bench_count_nanosleeps \
             0009-tao_bench_rand_bounds_check; do
      patch -s -p1 -i "$tb/$p.diff"
    done
    autoreconf -fi
    # The patched configure looks for fmt under <dir>/lib64/pkgconfig; a
    # getdeps-built fmt may sit under lib. Without one, the system fmt is
    # found through pkg-config.
    fmt_flag=()
    fmt_dir=$(find "$folly_scratch/installed" -maxdepth 1 -name 'fmt-*' | head -n1)
    if [ -n "$fmt_dir" ]; then
      [ -d "$fmt_dir/lib64" ] || ln -s lib "$fmt_dir/lib64"
      fmt_flag=(--with-fmt="$fmt_dir")
    fi
    PKG_CONFIG_PATH="$folly_inst/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" \
      ./configure -q --with-folly="$folly_inst" "${fmt_flag[@]}" --disable-coverage --enable-tls
    make -s -j"$jobs"
    install -m 755 memcached "$prefix/bin/tao_bench_server"
  )
fi

if [ ! -x "$prefix/bin/tao_bench_client" ]; then
  log building the client, memtier_benchmark with the TaoBench patch
  rm -rf memtier_client
  mkdir memtier_client
  (
    cd memtier_client
    git init -q
    git fetch -q --depth 1 https://github.com/RedisLabs/memtier_benchmark "$memtier_rev"
    git checkout -q FETCH_HEAD
    git apply "$tb/0005-tao_bench_client_memtier_20230615.diff"
    autoreconf -fi
    ./configure -q --enable-tls
    make -s -j"$jobs"
    install -m 755 memtier_benchmark "$prefix/bin/tao_bench_client"
  )
fi

install -m 644 "$tb/leader_sizes.json" "$tb/db_items.json" "$prefix/share/taobench/"
tar xzf "$build/DCPerf/packages/common/certs.tar.gz" -C "$prefix/share/taobench/"
log "installed under $prefix"
