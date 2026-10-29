# Releasing

`mesh-llm plugins install Mesh-LLM/powermetrics-plugin` installs, from the
**latest GitHub release** (not `main`), the archive for the host's target
triple: `powermetrics-plugin-<target>.tar.gz`, or `.zip` on Windows, with its
`.sha256`. Merging does not ship anything; a tag does.

Each release carries, for `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`, the versioned name
`powermetrics-plugin-vX.Y.Z-<target>` and the stable alias
`powermetrics-plugin-<target>`, each rooted under `powermetrics-plugin/` with
`plugin.toml` and the executable, as the installer expects.

1. PR bumping `version` in `Cargo.toml` and `plugin.toml` (and `Cargo.lock`,
   via `cargo check`). Merge it.
2. Tag the merge commit and push the tag:
   ```sh
   git tag v0.2.0 <merge-sha> && git push origin v0.2.0
   ```
   `.github/workflows/release.yml` (also run as a no-publish dry run on PRs
   that touch packaging) checks the tag matches the version, builds and smokes
   each target on its own runner, and publishes the release with every archive.
3. Check: `mesh-llm plugins install Mesh-LLM/powermetrics-plugin` in a clean
   `HOME` reports the new version.

Users update with `mesh-llm plugins update powermetrics-plugin`, or by
re-running the install command.

Manual fallback, on a machine of the target triple:
`just package <target>`, then
`gh release create vX.Y.Z --target <sha> dist/*`.
