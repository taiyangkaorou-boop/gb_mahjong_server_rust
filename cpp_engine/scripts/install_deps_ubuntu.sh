#!/usr/bin/env bash

set -euo pipefail

if [[ -r /etc/os-release ]]; then
  # shellcheck disable=SC1091
  source /etc/os-release
  if [[ "${ID:-}" != "ubuntu" ]]; then
    echo "warning: this script was prepared for Ubuntu; detected ID=${ID:-unknown}" >&2
  fi
fi

if [[ "${EUID}" -eq 0 ]]; then
  SUDO=""
else
  SUDO="sudo"
fi

PACKAGES=(
  build-essential
  cmake
  pkg-config
  libprotobuf-dev
  protobuf-compiler
  libgrpc++-dev
  protobuf-compiler-grpc
)

echo "Installing C++ rule engine build dependencies:"
printf '  - %s\n' "${PACKAGES[@]}"

${SUDO} apt-get update
${SUDO} apt-get install -y "${PACKAGES[@]}"

echo
echo "Dependency installation complete."
echo "Next steps:"
echo "  cmake -S cpp_engine -B cpp_engine/build"
echo "  cmake --build cpp_engine/build -j"
echo "  ./cpp_engine/build/gb_mahjong_rule_engine_server"
