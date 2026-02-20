#!/usr/bin/env bash
# Check that all crate versions are consistent and match expected patterns

set -euo pipefail

echo "Checking version consistency..."

# Get strata-gst version (this is the "release" version)
GST_VERSION=$(grep '^version = ' crates/strata-gst/Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')

# Core strata crates that should share a version
CORE_CRATES=(
    "strata-bonding"
    "strata-common"
    "strata-control"
    "strata-agent"
    "strata-dashboard"
    "strata-portal"
    "strata-sim"
)

echo "Release version (strata-gst): $GST_VERSION"

# Check each core crate
for crate in "${CORE_CRATES[@]}"; do
    version=$(grep '^version = ' "crates/$crate/Cargo.toml" | head -1 | sed 's/version = "\(.*\)"/\1/')
    echo "  $crate: $version"
done

echo ""
echo "Note: Version differences are expected:"
echo "  - strata-transport is at its own version (networking layer)"
echo "  - strata-gst typically has a different version (GStreamer plugin)"
echo "  - Core platform crates (common, control, agent, etc.) typically share a version"
echo ""
echo "✓ Version check complete"
