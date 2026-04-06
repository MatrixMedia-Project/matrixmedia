#!/usr/bin/env bash
# Bump MatrixMedia version across all workspaces
# Usage: bash scripts/bump-version.sh 0.2.0

set -euo pipefail

VERSION="${1:?Usage: $0 <new-version>}"

echo "Bumping to version $VERSION..."

# Rust workspace
sed -i.bak "s/^version = \"[^\"]*\"/version = \"$VERSION\"/" Cargo.toml

# Web packages
for pkg in mm-widget mm-dashboard mm-viewer; do
    jq ".version = \"$VERSION\"" "web/packages/$pkg/package.json" > tmp.json
    mv tmp.json "web/packages/$pkg/package.json"
done

# iOS SDK (Package.swift doesn't have version, uses git tags)
# Android SDK (update build.gradle.kts if needed)

# Clean up backups
find . -name "*.bak" -delete

echo "Version bumped to $VERSION"
echo "Next: git add -A && git commit -m 'chore: bump version to $VERSION' && git tag v$VERSION"
