#!/usr/bin/env sh
# Checks what agent-pr-runner needs, then builds it and installs it outside any repository.
#
#   sh scripts/install.sh                        # check, build, install to ~/agent-pr-runner
#   sh scripts/install.sh --check-only           # only check
#   sh scripts/install.sh /opt/agent-pr-runner   # another install folder
#
# Creates:  <dir>/agent-pr-runner
#           <dir>/disabled-hooks/   (must stay empty)
#           <dir>/queues/           (one folder per repository)
#           <dir>/configs/          (one config per repository)
# Re-running it updates the binary and leaves configs and queues alone.
#
# Exit codes: 0 done, 2 something is missing (the report says what to install and how).
set -eu

check_only=0
install_dir="$HOME/agent-pr-runner"
for arg in "$@"; do
    case "$arg" in
        --check-only) check_only=1 ;;
        *) install_dir="$arg" ;;
    esac
done
source_dir="$(cd "$(dirname "$0")/.." && pwd)"

# rustup installs into ~/.cargo/bin, which a terminal opened earlier may not have on PATH yet.
PATH="$PATH:$HOME/.cargo/bin"
export PATH

os="$(uname -s)"
pkg=""
if [ "$os" = "Linux" ]; then
    for candidate in apt-get dnf pacman zypper; do
        if command -v "$candidate" >/dev/null 2>&1; then pkg="$candidate"; break; fi
    done
fi

install_hint() {
    case "$1:$os:$pkg" in
        rust:*) echo "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   (or https://rustup.rs)" ;;
        cc:Darwin:*) echo "xcode-select --install" ;;
        cc:Linux:apt-get) echo "sudo apt-get install build-essential" ;;
        cc:Linux:dnf) echo "sudo dnf install gcc" ;;
        cc:Linux:pacman) echo "sudo pacman -S base-devel" ;;
        cc:Linux:zypper) echo "sudo zypper install gcc" ;;
        git:Darwin:*) echo "xcode-select --install   (or brew install git)" ;;
        git:Linux:apt-get) echo "sudo apt-get install git" ;;
        git:Linux:dnf) echo "sudo dnf install git" ;;
        git:Linux:pacman) echo "sudo pacman -S git" ;;
        git:Linux:zypper) echo "sudo zypper install git" ;;
        gh:Darwin:*) echo "brew install gh   (or https://cli.github.com)" ;;
        gh:Linux:pacman) echo "sudo pacman -S github-cli" ;;
        gh:Linux:*) echo "follow https://github.com/cli/cli/blob/trunk/docs/install_linux.md" ;;
        *) echo "see https://rustup.rs, https://git-scm.com, https://cli.github.com" ;;
    esac
}

missing=""
report() {
    # report NAME COMMAND HINT_KEY
    if path="$(command -v "$2" 2>/dev/null)"; then
        printf '  ok       %-11s %s\n' "$1" "$path"
    else
        printf '  MISSING  %-11s install: %s\n' "$1" "$(install_hint "$3")"
        missing="$missing $1"
    fi
}

echo "Checking what agent-pr-runner needs:"
report "Rust" cargo rust
case "$os" in Darwin|Linux) report "C linker" cc cc ;; esac
report "Git" git git
report "GitHub CLI" gh gh
if command -v gh >/dev/null 2>&1; then
    if gh auth status >/dev/null 2>&1; then
        printf '  ok       %-11s logged in\n' "gh login"
    else
        printf '  MISSING  %-11s run: gh auth login   (the user does this; it opens a browser)\n' "gh login"
        missing="$missing gh-login"
    fi
fi

if [ -n "$missing" ]; then
    echo
    echo "Missing:$missing."
    echo "Install them, open a new terminal so PATH updates, and run this script again."
    exit 2
fi
if [ "$check_only" = 1 ]; then
    echo
    echo "Everything needed is installed."
    exit 0
fi

(cd "$source_dir" && cargo build --release)

mkdir -p "$install_dir/disabled-hooks" "$install_dir/queues" "$install_dir/configs"
if [ -f "$install_dir/agent-pr-runner" ]; then
    cp "$install_dir/agent-pr-runner" "$install_dir/agent-pr-runner.previous"
fi
cp "$source_dir/target/release/agent-pr-runner" "$install_dir/agent-pr-runner"
chmod 755 "$install_dir/agent-pr-runner"

echo
echo "Installed: $install_dir/agent-pr-runner"
echo "Next, write the config for a repository:"
echo "  \"$install_dir/agent-pr-runner\" init --repo <path to the repository>"
