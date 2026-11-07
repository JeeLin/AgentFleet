#!/bin/sh
# 安装 afcli：从 GitHub Release 取「干净名」的静态二进制，校验 SHA256SUMS，
# chmod +x 装进 PATH，最后真跑一次 `afcli --version` 证明它可直接执行。
#
#   curl -fsSL https://github.com/JeeLin/AgentFleet/releases/latest/download/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/JeeLin/AgentFleet/main/install.sh | sh
#
# 可覆盖：AFCLI_VERSION=v0.7.3、AFCLI_REPO、AFCLI_INSTALL_DIR、AFCLI_BASE_URL（内网/离线镜像）。
# Windows 没有 sh：直接下载 Release 里的 afcli-windows-amd64.exe。
set -eu

repo="${AFCLI_REPO:-JeeLin/AgentFleet}"
if [ "${AFCLI_VERSION:-latest}" = "latest" ]; then
    default_base="https://github.com/$repo/releases/latest/download"
else
    default_base="https://github.com/$repo/releases/download/${AFCLI_VERSION}"
fi
base="${AFCLI_BASE_URL:-$default_base}"

os=$(uname -s)
arch=$(uname -m)
case "$os:$arch" in
    Linux:x86_64 | Linux:amd64) asset=afcli-linux-amd64 ;;
    Linux:aarch64 | Linux:arm64) asset=afcli-linux-arm64 ;;
    Darwin:arm64) asset=afcli-macos-arm64 ;;
    *)
        echo "afcli: unsupported platform $os/$arch; download afcli-windows-amd64.exe on Windows" >&2
        exit 1
        ;;
esac

for tool in curl install awk; do
    command -v "$tool" >/dev/null 2>&1 || { echo "afcli: $tool is required" >&2; exit 1; }
done

tmp=${TMPDIR:-/tmp}/afcli-install.$$
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' 0 HUP INT TERM
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"
curl -fsSL "$base/$asset" -o "$tmp/$asset"
cd "$tmp"

# 只校验选中那一行：SUMS 里还有 afhub 与桌面产物，别让本机没有的文件把 -c 弄红。
awk -v a="$asset" '$2 == a' SHA256SUMS > ONE.sha256
[ "$(wc -l < ONE.sha256 | tr -d ' ')" = 1 ] || {
    echo "afcli: SHA256SUMS 里 $asset 的条目数不为 1" >&2
    exit 1
}
expected=$(awk '{print $1}' ONE.sha256)
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$asset" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$asset" | awk '{print $1}')
else
    echo "afcli: need sha256sum or shasum to verify the download" >&2
    exit 1
fi
[ "$actual" = "$expected" ] || {
    echo "afcli: checksum mismatch for $asset" >&2
    exit 1
}

install_dir="${AFCLI_INSTALL_DIR:-/usr/local/bin}"
mkdir -p "$install_dir" 2>/dev/null || true
if [ -w "$install_dir" ]; then
    install -m 0755 "$asset" "$install_dir/afcli"
elif command -v sudo >/dev/null 2>&1; then
    sudo mkdir -p "$install_dir"
    sudo install -m 0755 "$asset" "$install_dir/afcli"
else
    echo "afcli: $install_dir is not writable and sudo is missing; set AFCLI_INSTALL_DIR" >&2
    exit 1
fi

# 不是只 chmod：真的执行一次，动态链/glibc/PIE 问题会在这里当场暴露。
"$install_dir/afcli" --version
echo "afcli installed to $install_dir/afcli"
