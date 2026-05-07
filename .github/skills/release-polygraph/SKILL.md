---
name: release-polygraph
description: "Release workflow for rs-polygraph. Use when: cutting a release, bumping the version, publishing to crates.io, preparing a release commit and tag."
argument-hint: "new version number, e.g. 0.7.0"
---

# rs-polygraph Release Workflow

## When to Use

- Cutting a new release of the `polygraph` crate
- Bumping the version (patch, minor, or major)
- Preparing a release commit and tag
- Publishing to crates.io

---

## Files That Must Be Updated

| File | Field | Notes |
|------|-------|-------|
| `Cargo.toml` | `version` under `[package]` | The single source of truth |
| `polygraph-difftest/Cargo.toml` | `polygraph` path-dep version | Update if it pins a version |
| `CHANGELOG.md` | New section at the top | Follow the existing heading format |

---

## Release Procedure

### 1. Determine the new version

Follow semantic versioning:
- **Patch** (`0.x.y`): bug fixes, no API changes
- **Minor** (`0.x.0`): new public API, backward-compatible
- **Major** (`x.0.0`): breaking API changes (rare before 1.0)

### 2. Update `Cargo.toml`

In the root `Cargo.toml`, change:
```toml
[package]
version = "<NEW>"
```

### 3. Update `CHANGELOG.md`

Add a new section at the top following this format:

```markdown
## [<NEW>] — <YYYY-MM-DD> — <Title>

<One-paragraph summary of what changed and why it matters.>

### Added
- ...

### Fixed
- ...

### Changed
- ...
```

### 4. Run the full test suite

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo test --test tck
cargo test -p polygraph-difftest
cargo doc --no-deps
```

All must pass with zero warnings and zero failures.

### 5. Dry-run publish

```bash
cargo publish --dry-run
```

Verify the crate package looks correct (no accidentally included large files).

### 6. Commit, tag, and push

```bash
git add Cargo.toml CHANGELOG.md
git commit -m "Release v<NEW>"
git tag v<NEW>
git push && git push --tags
```

The `release.yml` workflow will trigger on the tag, run CI again, and publish
to crates.io automatically. Monitor it at:
`https://github.com/trickle-labs/rs-polygraph/actions`

### 7. Verify publication

- crates.io: `https://crates.io/crates/polygraph`
- docs.rs: `https://docs.rs/polygraph/<NEW>`

docs.rs builds asynchronously — allow a few minutes.

---

## Common Mistakes

- Forgetting to update `CHANGELOG.md` before tagging.
- Tagging before all CI jobs pass (`cargo clippy` is the most common failure).
- Publishing when `cargo publish --dry-run` shows test/fixture files that bloat
  the download — add them to `exclude` in `Cargo.toml` first.
- Bumping only one of multiple `Cargo.toml` files in the workspace (check
  `polygraph-difftest/Cargo.toml` if it pins the parent version).
