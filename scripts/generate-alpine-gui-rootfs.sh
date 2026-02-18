#!/usr/bin/env bash
set -euo pipefail

# Generate an Alpine minirootfs with XFCE + Xvfb + x11vnc + noVNC + qgui.
#
# Output is a rootfs tar.gz (not an OCI image). This is useful for runtimes that ingest rootfs
# tarballs directly.

OUT_PATH="${1:-./qgui-alpine-gui.tar.gz}"

ALPINE_VERSION="${ALPINE_VERSION:-3.21}"
ALPINE_MIRROR="${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}"
ALPINE_BRANCH="${ALPINE_BRANCH:-v${ALPINE_VERSION}}"

ROOTFS_ARCH="${ROOTFS_ARCH:-}"
if [[ -z "${ROOTFS_ARCH}" ]]; then
  case "$(uname -m)" in
    x86_64|amd64) ROOTFS_ARCH="x86_64" ;;
    aarch64|arm64) ROOTFS_ARCH="aarch64" ;;
    *)
      echo "Unsupported host arch: $(uname -m). Set ROOTFS_ARCH manually." >&2
      exit 1
      ;;
  esac
fi

rust_musl_target() {
  case "$(uname -m)" in
    x86_64|amd64) echo "x86_64-unknown-linux-musl" ;;
    aarch64|arm64) echo "aarch64-unknown-linux-musl" ;;
    *) echo "" ;;
  esac
}

main() {
  local tmp
  tmp="$(mktemp -d)"

  local tar_name="alpine-minirootfs-${ALPINE_VERSION}.0-${ROOTFS_ARCH}.tar.gz"
  local url="${ALPINE_MIRROR}/${ALPINE_BRANCH}/releases/${ROOTFS_ARCH}/${tar_name}"
  local cache="/tmp/${tar_name}"

  echo "[qgui] building static qgui..."
  local musl
  musl="$(rust_musl_target)"
  if [[ -z "${musl}" ]]; then
    echo "Could not determine musl target for $(uname -m)" >&2
    exit 1
  fi
  cargo build --release --bin qgui --target "${musl}" >/dev/null

  echo "[qgui] downloading alpine minirootfs: ${url}"
  if [[ ! -f "${cache}" ]]; then
    curl -fsSL -o "${cache}" "${url}"
  fi

  echo "[qgui] extracting rootfs..."
  tar -xzf "${cache}" -C "${tmp}"

  # Repos and DNS for apk inside chroot.
  sudo mkdir -p "${tmp}/etc/apk"
  cat > "${tmp}/etc/apk/repositories" <<EOF
${ALPINE_MIRROR}/${ALPINE_BRANCH}/main
${ALPINE_MIRROR}/${ALPINE_BRANCH}/community
EOF
  if [[ -f /etc/resolv.conf ]]; then
    sudo cp /etc/resolv.conf "${tmp}/etc/resolv.conf" 2>/dev/null || true
  fi

  echo "[qgui] installing GUI packages (chroot apk)..."
  sudo mount -t proc none "${tmp}/proc" 2>/dev/null || true
  sudo mount -t sysfs none "${tmp}/sys" 2>/dev/null || true
  sudo mount --bind /dev "${tmp}/dev" 2>/dev/null || true

  # Keep the set minimal but functional.
  sudo chroot "${tmp}" /bin/sh -c "
    set -e
    apk update
    apk add --no-cache \
      xfce4 xfce4-terminal \
      xvfb \
      x11vnc \
      novnc websockify \
      dbus dbus-x11 \
      fontconfig ttf-dejavu \
      mesa-dri-gallium mesa-vulkan-swrast
    command -v Xvfb >/dev/null
    command -v startxfce4 >/dev/null
    command -v x11vnc >/dev/null
    test -d /usr/share/novnc
    command -v websockify >/dev/null
  "

  sudo umount "${tmp}/proc" 2>/dev/null || true
  sudo umount "${tmp}/sys" 2>/dev/null || true
  sudo umount "${tmp}/dev" 2>/dev/null || true

  echo "[qgui] installing qgui binary..."
  sudo mkdir -p "${tmp}/usr/local/bin"
  sudo cp "./target/${musl}/release/qgui" "${tmp}/usr/local/bin/qgui"
  sudo chmod +x "${tmp}/usr/local/bin/qgui"

  # Convenience alias.
  if [[ -f "${tmp}/etc/profile" ]]; then
    cat >> "${tmp}/etc/profile" <<'EOF'

# qgui helper
alias gui='qgui up'
EOF
  fi

  echo "[qgui] creating tarball: ${OUT_PATH}"
  (cd "${tmp}" && sudo tar czf - .) > "${OUT_PATH}"
  sudo chown "$(id -u)":"$(id -g)" "${OUT_PATH}" 2>/dev/null || true

  sudo rm -rf "${tmp}"

  echo "[qgui] done: ${OUT_PATH}"
}

main "$@"

