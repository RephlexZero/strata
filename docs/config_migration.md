# Config Migration

## Versioning
- Current schema version: `1`
- `version=0` is accepted and mapped to the current defaults.

## Deprecation Policy
- Deprecated fields remain supported for at least one minor release.
- Deprecated fields emit warnings during parsing.

## Migration Steps
1. Set `version = 1` in TOML.
2. Move receiver settings under `[receiver]`.
3. Add `[lifecycle]` only if non-default thresholds are needed.

## Compatibility Tests
- Empty config resolves to defaults.
- `version=0` resolves to current schema version.
