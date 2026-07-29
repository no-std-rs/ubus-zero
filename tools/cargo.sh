#!/usr/bin/env bash
# Linux: native cargo. macOS: cross with a Linux target.
set -euo pipefail

case "$(uname -s)" in
  Linux)
    exec cargo "$@"
    ;;
  Darwin)
    case "$(uname -m)" in
      arm64|aarch64) default_target=aarch64-unknown-linux-gnu ;;
      *) default_target=x86_64-unknown-linux-gnu ;;
    esac
    target="${CARGO_HOST_TARGET:-$default_target}"
    [[ $# -gt 0 ]] || { echo "usage: $0 <subcommand> [args]" >&2; exit 2; }
    sub="$1"; shift
    case "$sub" in
      fmt|clean) exec cargo "$sub" "$@" ;;
    esac
    command -v cross >/dev/null 2>&1 || { echo "$0: install cross (cargo install cross) and run docker" >&2; exit 1; }
    exec cross "$sub" --target "$target" "$@"
    ;;
  *)
    echo "$0: unsupported host: $(uname -s)" >&2
    exit 1
    ;;
esac
