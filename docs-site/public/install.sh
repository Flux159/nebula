#!/bin/bash
#
# Nebula Installer
# https://flux159.github.io/nebula/
#
# Usage: curl -fsSL https://flux159.github.io/nebula/install.sh | bash
#
# Options (environment variables):
#   NEBULA_INSTALL_DIR  Directory for the CLI shims (default: ~/.nebula/bin)
#   NEBULA_VERSION      Release tag to install (default: the latest release,
#                       e.g. v0.1.0)
#
# macOS (Apple Silicon): installs Nebula.app to /Applications and puts the
#   `nebula` CLI on your PATH. Linux (x64/arm64): installs the engine
#   (nebula, nebulad, libkrun) to ~/.nebula/engine. Windows: use install.ps1.
#

set -e

REPO="Flux159/nebula"
BIN_DIR="${NEBULA_INSTALL_DIR:-$HOME/.nebula/bin}"
ENGINE_DIR="$HOME/.nebula/engine"
APP_DIR="/Applications"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

print_banner() {
    echo -e "${BLUE}"
    echo "  _   _      _           _       "
    echo " | \\ | | ___| |__  _   _| | __ _ "
    echo " |  \\| |/ _ \\ '_ \\| | | | |/ _\` |"
    echo " | |\\  |  __/ |_) | |_| | | (_| |"
    echo " |_| \\_|\\___|_.__/ \\__,_|_|\\__,_|"
    echo -e "${NC}"
}

info() {
    echo -e "${BLUE}==>${NC} $1"
}

success() {
    echo -e "${GREEN}==>${NC} $1"
}

warn() {
    echo -e "${YELLOW}Warning:${NC} $1"
}

error() {
    echo -e "${RED}Error:${NC} $1"
    exit 1
}

detect_platform() {
    local os=$(uname -s)
    local arch=$(uname -m)

    case "$os" in
        Darwin)
            OS="macos"
            if [ "$arch" != "arm64" ]; then
                error "Nebula on macOS requires Apple Silicon (arm64). Intel Macs are not supported."
            fi
            ARCH="aarch64"
            ;;
        Linux)
            OS="linux"
            case "$arch" in
                x86_64|amd64) ARCH="x86_64" ;;
                arm64|aarch64) ARCH="aarch64" ;;
                *) error "Unsupported architecture: $arch" ;;
            esac
            ;;
        MINGW*|MSYS*|CYGWIN*)
            error "Please use PowerShell on Windows: irm https://flux159.github.io/nebula/install.ps1 | iex"
            ;;
        *)
            error "Unsupported operating system: $os"
            ;;
    esac

    info "Detected platform: ${OS}-${ARCH}"
}

resolve_version() {
    TAG="${NEBULA_VERSION:-}"
    if [ -n "$TAG" ]; then
        case "$TAG" in v*) ;; *) TAG="v$TAG" ;; esac
        info "Using pinned version: $TAG"
    else
        info "Fetching latest release..."
        if command -v curl &> /dev/null; then
            TAG=$(curl -sL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
        elif command -v wget &> /dev/null; then
            TAG=$(wget -qO- "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
        else
            error "Neither curl nor wget found. Please install one of them."
        fi
        # Fallback: the gh CLI (works while the repository is private).
        if [ -z "$TAG" ] && command -v gh &> /dev/null; then
            TAG=$(gh release view --repo "$REPO" --json tagName -q .tagName 2>/dev/null || true)
        fi
        if [ -z "$TAG" ]; then
            error "Failed to fetch the latest version. Check your internet connection."
        fi
        info "Latest version: $TAG"
    fi
    VER="${TAG#v}"
}

# fetch <asset-name> <dest-path> — direct download with gh CLI fallback
fetch() {
    local asset="$1"
    local dest="$2"
    local url="https://github.com/$REPO/releases/download/$TAG/$asset"

    if command -v curl &> /dev/null; then
        curl -fsSL "$url" -o "$dest" && return 0
    elif command -v wget &> /dev/null; then
        wget -q "$url" -O "$dest" && return 0
    fi

    if command -v gh &> /dev/null; then
        warn "Direct download failed; retrying with the GitHub CLI..."
        gh release download "$TAG" --repo "$REPO" --pattern "$asset" --output "$dest" && return 0
    fi
    return 1
}

install_macos() {
    local tmpdir=$(mktemp -d)
    local dmg="$tmpdir/nebula.dmg"
    local asset="Nebula_${VER}_aarch64.dmg"

    info "Downloading $asset..."
    fetch "$asset" "$dmg" || error "Download failed. Check that the release exists: https://github.com/$REPO/releases/tag/$TAG"

    info "Installing Nebula.app to $APP_DIR..."
    local mount
    mount=$(hdiutil attach -nobrowse -readonly "$dmg" | grep -o '/Volumes/.*' | head -1)
    [ -d "$mount/Nebula.app" ] || { hdiutil detach "$mount" -quiet || true; error "Nebula.app not found in the disk image."; }

    if [ -d "$APP_DIR/Nebula.app" ]; then
        warn "Replacing existing $APP_DIR/Nebula.app"
        rm -rf "$APP_DIR/Nebula.app"
    fi
    cp -R "$mount/Nebula.app" "$APP_DIR/"
    hdiutil detach "$mount" -quiet
    rm -rf "$tmpdir"

    # A wrapper (not a symlink) so the CLI's own bundle-relative lookups
    # (nebulad sidecar, Frameworks/, Resources/) resolve inside the app.
    info "Linking the nebula CLI into $BIN_DIR..."
    mkdir -p "$BIN_DIR"
    cat > "$BIN_DIR/nebula" <<'EOF'
#!/bin/sh
exec "/Applications/Nebula.app/Contents/MacOS/nebula" "$@"
EOF
    chmod +x "$BIN_DIR/nebula"

    INSTALLED_CLI="$BIN_DIR/nebula"
}

install_linux() {
    local tmpdir=$(mktemp -d)
    local stage="nebula-${VER}-linux-${ARCH}"

    info "Downloading $stage.tar.gz..."
    fetch "$stage.tar.gz" "$tmpdir/$stage.tar.gz" || error "Download failed. Check that the release exists: https://github.com/$REPO/releases/tag/$TAG"

    # Verify the checksum when the sidecar file is available
    if fetch "$stage.tar.gz.sha256" "$tmpdir/$stage.tar.gz.sha256" 2>/dev/null; then
        info "Verifying SHA-256 checksum..."
        (cd "$tmpdir" && sha256sum -c "$stage.tar.gz.sha256" >/dev/null) || error "Checksum verification failed."
    else
        warn "Checksum file not available; skipping verification."
    fi

    info "Installing engine to $ENGINE_DIR..."
    tar -C "$tmpdir" -xzf "$tmpdir/$stage.tar.gz"
    rm -rf "$ENGINE_DIR"
    mkdir -p "$(dirname "$ENGINE_DIR")"
    mv "$tmpdir/$stage" "$ENGINE_DIR"
    rm -rf "$tmpdir"

    # Symlinks are fine on Linux: /proc/self/exe resolves to the real path,
    # so nebulad and lib/libkrun.so.1 are found next to the actual binaries.
    mkdir -p "$BIN_DIR"
    ln -sf "$ENGINE_DIR/nebula" "$BIN_DIR/nebula"
    ln -sf "$ENGINE_DIR/nebulad" "$BIN_DIR/nebulad"

    INSTALLED_CLI="$BIN_DIR/nebula"

    if [ ! -e /dev/kvm ]; then
        warn "/dev/kvm not found — Nebula on Linux needs KVM. Enable virtualization in your BIOS/hypervisor."
    fi
}

CONFIGURED_SHELL_PROFILE=""

setup_path() {
    local shell_profile=""
    local shell_name=$(basename "$SHELL")

    case "$shell_name" in
        bash)
            if [ -f "$HOME/.bashrc" ]; then
                shell_profile="$HOME/.bashrc"
            elif [ -f "$HOME/.bash_profile" ]; then
                shell_profile="$HOME/.bash_profile"
            fi
            ;;
        zsh)
            shell_profile="$HOME/.zshrc"
            ;;
        fish)
            shell_profile="$HOME/.config/fish/config.fish"
            ;;
    esac

    if [ -n "$shell_profile" ]; then
        local path_line="export PATH=\"\$PATH:$BIN_DIR\""

        if [ "$shell_name" = "fish" ]; then
            path_line="set -gx PATH \$PATH $BIN_DIR"
        fi

        if ! grep -q "$BIN_DIR" "$shell_profile" 2>/dev/null; then
            echo "" >> "$shell_profile"
            echo "# Nebula" >> "$shell_profile"
            echo "$path_line" >> "$shell_profile"
            CONFIGURED_SHELL_PROFILE="$shell_profile"
            info "Added $BIN_DIR to PATH in $shell_profile"
        else
            CONFIGURED_SHELL_PROFILE="$shell_profile"
            info "PATH already configured in $shell_profile"
        fi
    else
        warn "Could not detect shell profile. Add this to your shell config:"
        echo "  export PATH=\"\$PATH:$BIN_DIR\""
    fi
}

verify_installation() {
    if [ -x "$INSTALLED_CLI" ]; then
        success "Installation complete!"
        echo ""
        echo "To use nebula in this terminal session, run:"
        echo -e "  ${BLUE}export PATH=\"\$PATH:$BIN_DIR\"${NC}"
        echo ""
        if [ -n "$CONFIGURED_SHELL_PROFILE" ]; then
            echo -e "PATH has been added to ${BLUE}$CONFIGURED_SHELL_PROFILE${NC} for future terminal sessions."
        fi
        echo ""
        echo "Boot the Vessel and point docker at it:"
        echo -e "  ${BLUE}nebula up${NC}"
        echo -e "  ${BLUE}nebula setup docker${NC}"
        echo -e "  ${BLUE}docker run -d -p 8080:80 nginx${NC}"
        echo ""
        echo "On first 'nebula up', the guest kernel + rootfs are downloaded and"
        echo "verified automatically."
        echo ""
        echo -e "Documentation: ${BLUE}https://flux159.github.io/nebula/${NC}"
    else
        error "Installation failed. nebula CLI not found."
    fi
}

main() {
    print_banner
    detect_platform
    resolve_version
    if [ "$OS" = "macos" ]; then
        install_macos
    else
        install_linux
    fi
    setup_path
    verify_installation
}

main "$@"
