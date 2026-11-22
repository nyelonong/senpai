#!/bin/sh
# Installs a prebuilt senpai from the GitHub releases:
#   curl -fsSL https://raw.githubusercontent.com/nyelonong/senpai/main/install.sh | sh
# SENPAI_INSTALL_DIR picks the folder (default ~/.local/bin); SENPAI_VERSION a
# release tag such as v0.1.0 (default: the latest release).
#
# Everything runs from main on the last line, so a download cut short runs nothing.
set -eu

fail() {
  echo "senpai: $*" >&2
  exit 1
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sum=$(sha256sum "$1") || return 1
  elif command -v shasum >/dev/null 2>&1; then
    sum=$(shasum -a 256 "$1") || return 1
  else
    fail "found neither sha256sum nor shasum to check the download"
  fi
  echo "${sum%% *}"
}

main() {
  repo=nyelonong/senpai
  dir=${SENPAI_INSTALL_DIR:-$HOME/.local/bin}
  version=${SENPAI_VERSION:-latest}

  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    Linux-x86_64) target=x86_64-unknown-linux-musl ;;
    Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
    *) fail "no prebuilt binary for $(uname -s) $(uname -m); try: cargo install --git https://github.com/$repo" ;;
  esac

  if [ "$version" = latest ]; then
    base=https://github.com/$repo/releases/latest/download
  else
    base=https://github.com/$repo/releases/download/$version
  fi
  asset=senpai-$target.tar.gz

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  trap 'rm -rf "$tmp"; exit 130' INT TERM
  curl -fsSL "$base/$asset" -o "$tmp/$asset" || fail "could not download $base/$asset"
  curl -fsSL "$base/$asset.sha256" -o "$tmp/$asset.sha256" || fail "could not download the checksum for $asset"

  # The checksum catches a broken download; it comes from the same release, so it is not a signature.
  expected=$(cut -d ' ' -f 1 "$tmp/$asset.sha256")
  actual=$(sha256_of "$tmp/$asset")
  [ -n "$expected" ] || fail "the checksum file for $asset is empty; nothing was installed"
  [ "$expected" = "$actual" ] || fail "checksum mismatch for $asset; nothing was installed"

  tar -xzf "$tmp/$asset" -C "$tmp"
  mkdir -p "$dir"
  # Copy beside the target, then rename: the swap is atomic, so senpai is never half-written.
  next="$dir/.senpai.new.$$"
  cp "$tmp/senpai-$target/senpai" "$next" && chmod 0755 "$next" && mv -f "$next" "$dir/senpai" || {
    rm -f "$next"
    fail "could not write $dir/senpai"
  }
  echo "installed $("$dir/senpai" --version) to $dir/senpai"

  case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "add $dir to your PATH to run senpai by name" ;;
  esac
  echo "next: senpai skill install   (teaches your coding agents to use it)"
}

main "$@"
