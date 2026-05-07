---
name: release-polygraph
description: "Release workflow for rs-polygraph. Use when: cutting a release, bumping the version, preparing a release commit and tag."
argument-hint: "new version number, e.g. 0.9.0"
---

# rs-polygraph Release Workflow

Three crates live in this workspace, each with its own independent version:

| Crate | Cargo.toml | Notes |
|---|---|---|
| `polygraph` | `crates/polygraph/Cargo.toml` | Main transpiler — the primary release target |
| `opencypher-parser` | `crates/opencypher-parser/Cargo.toml` | Parser sub-crate — bump when its public API changes |
| `polygraph-difftest` | `polygraph-difftest/Cargo.toml` | `publish = false` — never released, version is irrelevant |

A normal release only requires bumping `crates/polygraph/Cargo.toml`. Bump `opencypher-parser` separately only when its own API or grammar changes.

---

## Steps

### 1. Check ROADMAP.md

Confirm the new version aligns with the next planned milestone before deciding on a patch / minor / major bump.

### 2. Run the full test suite

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test --lib
cargo test --test tck        # slow, mandatory
cargo test -p polygraph-difftest
cargo doc --no-deps
```

All must pass with zero warnings. Do not proceed if anything fails.

### 3. Update `Cargo.toml` and `CHANGELOG.md`

In `crates/polygraph/Cargo.toml` (and `crates/opencypher-parser/Cargo.toml` if its API changed):
```toml
[package]
version = "<NEW>"
```

Add a section at the top of `CHANGELOG.md`:
```markdown
## [<NEW>] — <YYYY-MM-DD> — <Title>

<One-paragraph summary.>

### Added / Fixed / Changed / Removed
- ...
```

Verify the version is correct:
```bash
grep "^version" crates/polygraph/Cargo.toml crates/opencypher-parser/Cargo.toml
```

### 4. Commit, tag, and push

```bash
git add crates/polygraph/Cargo.toml crates/opencypher-parser/Cargo.toml CHANGELOG.md
git commit -m "Release v<NEW>"
git tag v<NEW>
git push && git push --tags
```

### 5. Create the GitHub release

Go to [github.com/BaardBouvet/rs-polygraph/releases/new](https://github.com/BaardBouvet/rs-polygraph/releases/new):
- **Tag**: `v<NEW>`
- **Title**: `v<NEW> — <Title from CHANGELOG>`
- **Description**: paste the new CHANGELOG section (body only, no heading)

### 6. Publishing (automatic)

The `.github/workflows/release.yml` workflow triggers automatically when the tag is pushed. It will:

1. Validate the tag matches `crates/polygraph/Cargo.toml` version
2. Run the full CI suite
3. Publish `opencypher-parser` and `polygraph` to crates.io

Monitor progress at: `https://github.com/BaardBouvet/rs-polygraph/actions/workflows/release.yml`

Once the workflow completes (usually 10–15 minutes), both crates will be live on crates.io:
- `https://crates.io/crates/opencypher-parser`
- `https://crates.io/crates/polygraph`

---

## Rollback

If something goes wrong after pushing the tag:

```bash
git tag -d v<NEW>
git push --delete origin v<NEW>
```

Then delete the GitHub release if it was created. Note: **If the CI workflow completed and published to crates.io, you cannot unpublish.** You'll need to yank the version on crates.io manually and prepare a patch release instead.
