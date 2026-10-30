# Deployment

Runs on the k3s/Flux2 cluster (Raspberry Pi 5, aarch64). REA is on the v_flakes
**container standard**: `nix build .#container` produces the OCI image, the
release workflow pushes it to GHCR on a `vX.Y.Z` tag, and Flux rolls it out. The
cluster manifests are generated from this repo's container contract — see the
`gitops` repo.

## Release

Tag the repo; CI does the rest (no manual build/ship):

```sh
git tag v0.1.0 && git push --tags    # → release-container.yml builds on
                                      #   ubuntu-24.04-arm + pushes
                                      #   ghcr.io/ev-estate/real_estate_allocation:v0.1.0
```

Manual build/push (when needed):

```sh
nix build .#container    # result → a docker-archive .tar.gz
skopeo copy docker-archive:result \
  docker://ghcr.io/ev-estate/real_estate_allocation:v0.1.0
```

## Contract

Port `59079`, health `/health`, criticality `normal`. State lives on a PVC
mounted at `/data` (the image's `WorkingDir`; sqlite + properties + layout).

- **Secret env** (`REA_ADMIN_TOKEN`, `GOOGLE_MAPS_KEY`): the `real_estate_allocation-env`
  k8s Secret, applied out-of-band — replaces the old `reasonable_envsubst` ship step.
- **`/data/config.toml`**: the `real_estate_allocation-config` ConfigMap, mounted
  (subPath) over the PVC. `socket_addr` MUST bind `0.0.0.0`.

Both are defined in `gitops/clusters/rpi5/apps/real-estate/`.
