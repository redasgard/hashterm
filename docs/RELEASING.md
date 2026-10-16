# Releasing

Releases are built by CI and published by a person. The repository uses GitHub's
**immutable releases**: once a release is published, its files can never be
added, replaced or deleted, and its tag name can never be reused. So CI stages
everything in a **draft**, and publishing is the last, deliberate step.

0. CI (`.github/workflows/ci.yml`: rustfmt, clippy `-D warnings`, tests with tmux installed) must be green on the commit you are about to tag.
1. Bump `version` under `[workspace.package]` in `Cargo.toml` (Cargo.lock
   follows), commit and push to `main`.
2. Tag and push:

   ```sh
   git tag -a vX.Y.Z -m "hashterm X.Y.Z"
   git push origin vX.Y.Z
   ```

   The workflow refuses a tag that does not match the Cargo.toml version.
3. Wait for the `release` workflow (about 6 minutes). It builds the `.deb` on
   Debian 13, generates signed SLSA provenance, and creates a **draft** release
   with both attached. A tag containing `-` (`v0.3.0-rc1`) becomes a
   pre-release. Its Debian version is written `0.3.0~rc1` (`build-deb.sh`
   maps the first `-` to `~`), so the rc sorts before the final release and
   upgrades to it.
4. Open the draft on GitHub, edit the notes, and press **Publish release**.

If the draft is wrong, delete it and the tag, fix, and tag again. That is safe
only **before** publishing; after publishing a tag name is gone for good, so use
the next version number.

## Verifying a download

```sh
slsa-verifier verify-artifact hashterm_X.Y.Z-1_amd64.deb \
  --provenance-path hashterm_X.Y.Z-1_amd64.deb.intoto.jsonl \
  --source-uri github.com/redasgard/hashterm --source-tag vX.Y.Z
```

([slsa-verifier](https://github.com/slsa-framework/slsa-verifier).) The `.deb`
declares `libvte-2.91-gtk4-0 (>= 0.80)` and `libgtk-4-1 (>= 4.18)`, so it installs
on stock Debian 13.
