# RFC-0004: Docker/OCI container backend

- **Status:** Accepted
- **Author(s):** container-backend-engineer, rust-staff-engineer (synthesized)
- **Date:** 2026-06-28
- **Tracking item:** M6-1 (and M6-2)

## Summary

`cairn-backend-docker` presents a Docker engine as a navigable filesystem: containers and images at
the top, each container's filesystem browsable beneath it.

## Design

- **Tree model.** `/` → `containers/` and `images/`. `/containers/<name>/…` browses that container's
  filesystem; `/images/<tag>` represents an image. Entries carry `EntryExt::Container`/`Image`.
- **`ContainerOps` seam** — `list_containers`/`list_images`/`list_dir`/`stat`/`read`. The path-routing
  and entry-mapping logic (`DockerVfs`) depends only on this seam and is **fully unit-tested against
  an in-memory mock** (the routing — root vs `/containers` vs `/containers/<c>/<path>` — is where the
  bugs are).
- **`BollardDocker` adapter** — implements `ContainerOps` over the Docker Engine API via `bollard`.
  Container/image **listing is implemented for real**; browsing a container's filesystem
  (`list_dir`/`stat`/`read`) is done over the archive (tar) / `exec` APIs as the integration step
  (validated against a live daemon in a dedicated CI job), returning `Unsupported` until then.
- **Read-only** for this milestone (writes/deletes into a container are a later refinement).
  Capabilities: `LIST|READ|RANDOM_READ`.

## Drawbacks / deferred

- In-container filesystem ops in the real adapter (tar/exec), `exec`/`logs` actions, image-layer
  browsing, container writes, and Podman/containerd runtimes are deferred.
- Container `state` enum mapping in the adapter is a refinement (currently `Unknown`).

## Rationale & alternatives

- *Implement directly on bollard* — the routing/mapping would be untestable without a live daemon;
  the `ContainerOps` seam gives hermetic coverage of the bespoke routing.
- *exec `ls`/`cat` parsing vs tar archive* — the tar archive (`download_from_container`) is more
  robust than shell parsing; chosen for the integration step.

## Security & privacy

A reachable Docker socket is powerful; registry/daemon credentials are brokered (never stored by the
backend). TLS for remote daemons in the transport step.

## Unresolved questions

- tar vs exec for in-container fs and the exact streaming model.
- Multi-runtime support (Podman API compatibility, containerd).
