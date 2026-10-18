#!/bin/bash
# Prepare a Whats Pro release: bump the version, write its notes, commit and
# tag. Pushing the tag starts .github/workflows/release.yml, which builds the
# Mac (Apple Silicon and Intel) and Windows installers, signs the update
# manifest and publishes the GitHub release the app's updater reads.
#
#   packaging/release-whatspro.sh 0.20.0 "Qué cambia, en una o varias líneas"
#
# It never pushes: it prints the command to run once you have looked.
set -euo pipefail

version="${1:-}"
notes="${2:-}"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || [ -z "$notes" ]; then
    echo "uso: $0 <versión, por ejemplo 0.20.0> \"<notas en castellano>\"" >&2
    exit 2
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "Hay cambios sin commitear. Commitealos o descartalos antes." >&2
    exit 1
fi
git fetch -q origin main
if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
    echo "HEAD no es origin/main: subí antes lo que falta (o actualizá esta copia)." >&2
    exit 1
fi
if git rev-parse -q --verify "refs/tags/v$version" >/dev/null; then
    echo "Ya existe el tag v$version." >&2
    exit 1
fi

current="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)"
sed -i '' "1,/^version = \"$current\"$/s//version = \"$version\"/" Cargo.toml
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cargo update -q -p zapfast --precise "$version" 2>/dev/null || cargo check -q --locked 2>/dev/null || cargo check -q

mkdir -p packaging/release-notes
{
    printf '%s\n' "$notes"
    printf '\nHecho con cariño por JulitoMaker\n'
} > "packaging/release-notes/v$version.md"

git add Cargo.toml Cargo.lock "packaging/release-notes/v$version.md"
git commit -q -m "Whats Pro $version"
git tag -a "v$version" -m "Whats Pro $version"

echo "Listo: Whats Pro $version (antes $current), commit y tag v$version."
echo "Para publicarla:"
echo "  git push origin main v$version"
