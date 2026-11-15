# Releasing (trunk + tags)

`main` is the only long-lived branch. Everything reaches it through a pull request, and a
release is just a pull request that raises the version.

## What runs when

| Event | What happens |
|---|---|
| Commit on a branch | Nothing. |
| Pull request to `main` | The checks only (`ci.yml`: clippy, fmt, tests, advisories, Docker and browser integration tests). No image is built. |
| Push (merge) to `main` | `release.yml`, **without tests** (they ran on the PR): the image is published as `edge` and `sha-<commit>`. `main` is protected, so only a PR with green, up-to-date checks gets here. |
| Push to `main` with a **new version** in `Cargo.toml` | The same, plus a **release**: the image is also published as `X.Y.Z`, `X.Y`, `X` and `latest`, then the tag `vX.Y.Z` and the GitHub Release are created on that commit. |

Tags are never pushed by hand and no workflow runs on a tag: a tag only comes out of `main`,
after the images exist. A failing CI publishes nothing and creates no tag.

## How to release

1. In a pull request, raise `version` in the workspace `Cargo.toml` (and the three
   `jellyfin-api` / `jellyswarrm-macros` / `jellyswarrm-proxy` entries in `Cargo.lock`:
   `cargo update --workspace` does it), together with the change itself or on its own.
2. Merge it. That is the release.

A merge that does not change the version only publishes `edge`.

## Safeguards (job `version` in `.github/workflows/release.yml`)

- The version must be `X.Y.Z` (no suffix).
- A version that is **lower** than the latest existing tag fails the run instead of being skipped.
- If the tag `vX.Y.Z` already exists the run only publishes the snapshot (re-running is safe).
- One run at a time per branch, so two quick merges cannot race for the same tag.

## Using the images

Deploy only SemVer tags (`X.Y.Z`); `edge` and `sha-...` are snapshots of `main`. Renovate in the
manifests repository follows the SemVer tags.
