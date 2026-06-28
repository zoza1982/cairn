# Cairn — Product Requirements Document (PRD)

> **Status:** Draft v0.1
> **Owner:** Zoran Vukmirica
> **Last updated:** 2026-06-27
> **Scope of this doc:** Product vision, users, goals, and *what* we are building and *why*.
> Technical architecture lives in the **Low-Level Design (LLD)**; sequencing and tasks live in the **Implementation Plan**.

---

## 1. Summary

**Cairn** is a modern, cross-platform terminal file manager in the spirit of Midnight Commander — a fast dual-pane TUI — reimagined so that *every pane can be any filesystem*: local disk, SSH/SFTP, S3, GCS, Azure Blob, Docker, and Kubernetes. It bundles secure credential management for all of those backends and an agentic AI assistant that can plan and execute file operations on your behalf, always with your confirmation.

The name evokes a *cairn* — the stacked stones that mark a trail through unfamiliar terrain. That is the product's promise: confident navigation across any storage, anywhere.

---

## 2. Problem & Motivation

Terminal users juggle a fragmented toolbox: `mc` for local files, `aws s3` / `gsutil` / `az` for object stores, `kubectl` for clusters, `docker` for containers, `scp`/`sftp` for remotes — each with its own syntax, auth model, and mental model. Moving a file from a Kubernetes pod to an S3 bucket means stitching several tools together by hand.

Meanwhile, Midnight Commander — still beloved — feels dated: clunky theming, awkward cloud support, no fuzzy search, no command palette, no AI, and a credential story that ranges from "plaintext config" to "doesn't exist."

**Cairn unifies these workflows behind one consistent, modern, keyboard-driven interface**, with credentials handled safely and an AI layer that turns intent into reviewed, executed actions.

---

## 3. Goals & Non-Goals

### 3.1 Goals
- **One tool, every filesystem.** Browse, transfer, and operate across local, remote, object, container, and cluster storage with a single consistent UX.
- **MC muscle memory preserved.** Veterans feel at home on day one; newcomers are not punished.
- **Secure by default.** Credentials for every backend are encrypted, never stored in plaintext, and easy to manage.
- **Cross-backend operations are first-class.** Copy/move/diff *between* any two backends as easily as within one.
- **Agentic AI that's safe.** Natural-language intent → a reviewable plan → confirmed execution.
- **Fast and responsive.** Async, non-blocking; the UI never freezes on a slow network or a huge listing.
- **Truly cross-platform.** Linux, macOS, Windows, and headless/remote terminals are all first-class.
- **Extensible.** Third parties can add backends, viewers, and actions without forking.

### 3.2 Non-Goals (for now)
- Not a GUI application — Cairn is TUI-only (though it must look great in a modern terminal).
- Not a general cloud-management console (no provisioning VMs, editing IAM, managing billing). Cairn manages *files and file-like resources*, not infrastructure lifecycle.
- Not a full IDE — the built-in editor targets quick edits, not large-scale development.
- Not a backup/sync product (though it can be scripted toward those ends).

---

## 4. Target Users & Personas

Cairn is positioned as **all-in-one, no compromise**: equally strong as a local power-user file manager *and* as a cloud/infra operator's daily driver.

| Persona | Who | Primary needs |
|---------|-----|---------------|
| **The SRE / DevOps engineer** | Lives in clusters and clouds | Jump between k8s pods, S3 buckets, and bastions; pull logs; move artifacts; never fumble credentials |
| **The terminal power user** | Long-time `mc`/vim/tmux user | Fast local file management, keyboard everything, scriptability, no mouse required |
| **The backend/platform developer** | Ships services, touches many environments | Inspect container filesystems, edit configs over SSH, browse object storage, quick diffs |
| **The data / ML practitioner** | Moves datasets around | Browse and transfer large objects across S3/GCS/Azure, verify integrity, summarize contents |

**Anti-persona:** users who want a mouse-first GUI app, or who only ever touch one local machine and are happy with `ls`/`cp`.

---

## 5. Product Principles

1. **Consistency across backends.** A copy is a copy whether it's local→local or pod→bucket. The same keys, the same flow.
2. **Keyboard-first, mouse-optional.** Everything is reachable without a mouse; the mouse is a bonus, never a requirement.
3. **The UI never blocks.** Slow operations run in the background with visible progress and a queue.
4. **Safe by default, powerful on demand.** Destructive and irreversible actions always confirm; power users can streamline once they opt in.
5. **Discoverable.** A command palette and contextual hints mean you can find a feature without reading the manual — but the manual is good too.
6. **Honest about cost and risk.** Cloud egress, deletes, and AI actions are surfaced clearly before they happen.
7. **Local-trust for secrets.** Credentials never leave the machine unencrypted; the AI never sees raw secrets.

---

## 6. Key Decisions (resolved)

These foundational product decisions are settled and frame the rest of the document:

| Area | Decision |
|------|----------|
| **Name** | Cairn |
| **Positioning** | All-in-one: local power tool *and* cloud/infra operator |
| **AI** | Deep agentic (natural language drives the tool), with a **plan → confirm → execute** safety model |
| **AI provider** | Pluggable / provider-agnostic; cloud model default, local (Ollama) as drop-in |
| **Secrets** | Hybrid — built-in encrypted vault, OS keychain holds the master key, optional sync to external managers |
| **License** | Pure open source (permissive: Apache-2.0 / MIT) |
| **Keybindings** | MC-faithful default, with switchable vim and custom presets |
| **v1 backends** | Local, SSH/SFTP, S3, GCS, Azure Blob, Docker, Kubernetes — *all* at launch |
| **Extensibility** | Sandboxed WASM plugins + declarative config |
| **Platforms** | Linux, macOS, Windows, and WSL/SSH/container terminals — all first-class |
| **Viewer/Editor** | Built-in viewer & editor, with `$EDITOR`/`$PAGER` fallback on demand |

---

## 7. Feature Requirements

Features are grouped by theme. Priority: **P0** = required for v1, **P1** = fast-follow, **P2** = later.

### 7.1 Core file manager (MC parity)
- **P0** Dual-pane layout with independent navigation, sort, and filter per pane.
- **P0** Copy, move, rename, delete, mkdir, symlink; bulk operations on selections.
- **P0** Multi-select (mark/unmark, invert, by pattern).
- **P0** Sort by name/size/date/type; show/hide hidden files; quick filter-as-you-type.
- **P0** File details and permissions; chmod/chown where the backend supports it.
- **P0** Bookmarks, navigation history, and "hotlist" of saved locations.
- **P0** Find files (by name, glob, content/grep) across the active backend.
- **P1** Directory comparison and sync (highlight differences, mirror selections).
- **P1** Archive browsing as a virtual filesystem (zip, tar, tar.gz, 7z) — enter, extract, add.
- **P1** Tabs (multiple locations per pane) and split/merge panes.
- **P2** Hex viewer and binary inspection.

### 7.2 Backends / Virtual File System
All backends present the same browse/transfer/operate interface where it makes sense; backend-specific actions appear contextually.

- **P0 Local** — full native filesystem.
- **P0 SSH/SFTP** — remote browsing, transfer, edit-in-place; key and agent auth.
- **P0 S3** — buckets/objects, prefixes-as-folders, multipart upload, storage-class awareness.
- **P0 GCS** — buckets/objects, parity with S3 feature set.
- **P0 Azure Blob** — containers/blobs, parity with S3 feature set.
- **P0 Docker** — list containers/images; browse a container's filesystem; browse image layers; `exec` into a container; copy files in/out.
- **P0 Kubernetes** — contexts/namespaces/pods as a navigable tree; view/stream logs; `exec` into pods; `cp` files; port-forward; multi-cluster.
- **P1** FTP/FTPS and WebDAV.
- **P2** Git objects, MTP/Android, additional object stores (MinIO/R2/B2 via S3-compat is covered by S3).

**Backend-aware semantics:** the product must gracefully express that not all backends support all operations (e.g., no chmod on S3, "directories" are prefixes, k8s "files" can be live logs). The UI communicates capability rather than failing opaquely.

### 7.3 Transfers & operations engine
- **P0** Background, non-blocking transfers with a visible queue and per-item progress, speed, and ETA.
- **P0** **Cross-backend transfers** (e.g., k8s pod → S3, local → Azure) as a native, first-class flow.
- **P0** Pause/resume/cancel; queue reordering.
- **P1** Resumable transfers across restarts where the backend allows.
- **P1** Checksum/integrity verification after transfer.
- **P1** Conflict resolution policies (skip/overwrite/rename/newer-wins) with per-operation defaults.
- **P2** Bandwidth throttling and concurrency limits per backend.

### 7.4 Secrets & credentials (hybrid model)
- **P0** Built-in **encrypted vault** storing credentials for every backend.
- **P0** Master key protected by the **OS keychain** (macOS Keychain, Secret Service, Windows Credential Manager); password fallback where no keychain exists.
- **P0** Per-backend credential profiles (e.g., multiple AWS profiles, several SSH identities), selectable when connecting.
- **P0** No plaintext secrets on disk, ever; secrets are redacted in logs and never exposed to the AI layer.
- **P1** Import from existing sources (`~/.aws/credentials`, `~/.ssh/config`, kubeconfig, gcloud, az).
- **P1** Optional sync/integration with external managers (HashiCorp Vault, cloud secret managers).
- **P2** Shared/team vaults (would intersect any future hosted offering).

### 7.5 Viewer & editor
- **P0** Built-in viewer (F3): syntax highlighting, large-file streaming, search, line numbers.
- **P0** Built-in editor (F4): quick edits with save-back to any backend (including remote/object/pod).
- **P0** On-demand fallback to `$EDITOR` / `$PAGER` for users who prefer vim/`bat`/`less`.
- **P1** Structured/preview rendering for common formats (Markdown, JSON, YAML, images via terminal graphics where supported).
- **P1** Live log view for streaming sources (k8s/docker), with follow and filter.

### 7.6 AI / agentic assistant
- **P0** Assistant panel (toggleable) for natural-language requests scoped to the current panes/selection.
- **P0** **Plan → confirm → execute** model: the AI proposes a concrete step list / dry-run; the user approves before anything mutates; destructive or irreversible steps confirm individually.
- **P0** Provider-agnostic: ships with a cloud model default, supports local models (Ollama) as a drop-in; user can bring their own key/endpoint.
- **P0** AI never receives raw secrets; it operates through the same permissioned action layer the user does.
- **P1** Practical tasks: summarize a file/log, explain a config, suggest commands, bulk-rename by description, spot anomalies in logs, find files by intent.
- **P1** Multi-step operations across backends (e.g., "move all logs older than 30 days from this pod to the archive bucket").
- **P2** Saved/repeatable AI "recipes" and an action history/undo journal.

### 7.7 Modern UX
- **P0** Command palette (fuzzy, Ctrl-K style) for actions *and* connection switching.
- **P0** True-color themes; light/dark; Nerd Font icon support with ASCII fallback.
- **P0** Mouse support (optional): click, scroll, drag-select, resize panes.
- **P0** Configurable keybindings with MC/vim/custom presets; first-run preset chooser.
- **P0** Breadcrumb path display and quick "go to path".
- **P1** Session restore (reopen panes, tabs, and connections).
- **P1** Fuzzy file finder within a location.
- **P2** Pluggable layouts/widgets.

### 7.8 Extensibility
- **P1** Declarative configuration (custom keybinds, actions, themes, backend profiles).
- **P1** Sandboxed **WASM plugin** API for third-party backends, viewers, and actions.
- **P2** A plugin registry/discovery experience.

### 7.9 Platform & distribution
- **P0** First-class on Linux, macOS, Windows, and WSL/SSH/container terminals.
- **P0** Single self-contained binary; sensible install paths and a config directory per platform convention.
- **P1** Distribution via common channels (Homebrew, package managers, `cargo install`, static musl build, prebuilt releases).
- **P1** Graceful degradation in limited terminals (no truecolor, no Nerd Fonts, narrow widths).

---

## 8. UX & Layout Concepts

Concrete TUI direction (illustrative — final visual design is its own track). The through-line: a familiar MC dual-pane skeleton where **each pane is a VFS source**, augmented by a palette, a preview/AI side panel, and a function-key bar.

**A — Classic dual-pane (each pane is a backend)**
```
┌─ cairn ─────────────────────────────────────── ⛁ local · 🔑 3 vaults ─┐
│ ◉ local:~/projects          │ ◉ s3://prod-backups/2026/              │
├─────────────────────────────┼────────────────────────────────────────┤
│  Name          Size  Modif  │  Name            Size   Modified        │
│ ▸ ..                        │ ▸ ..                                     │
│   src/          —    14:02  │   db-snap.tar.gz 4.2G  2026-06-20       │
│   Cargo.toml   1.2K  13:55  │   logs/           —    2026-06-19       │
│ ▸ README.md    8.0K  12:30  │   config.yaml    2.1K  2026-06-18       │
├─────────────────────────────┴────────────────────────────────────────┤
│ ~/projects $ _                                                        │
├──────────────────────────────────────────────────────────────────────┤
│ 1Help 2Menu 3View 4Edit 5Copy 6Move 7Mkdir 8Del 9Conn 10Quit         │
└──────────────────────────────────────────────────────────────────────┘
```

**B — Dual-pane + context preview** (a k8s "directory" is pods; preview is live logs)
```
┌─ cairn ────────────────────────────────────────────────────────────────┐
│ ◉ k8s://prod/ns:payments/pods     │ Preview · api-7f9c (logs, live)    │
├───────────────────────────────────┤ 14:22:01 INFO  starting worker     │
│ ▸ ..                              │ 14:22:03 WARN  retry queue=12      │
│   api-7f9c        Running   3/3   │ 14:22:05 ERROR timeout calling db  │
│   cache-0         Running   1/1   │ 14:22:06 INFO  reconnected         │
│   migrate-job     Completed       │ …                                  │
├───────────────────────────────────┴─────────────────────────────────────┤
│ 1Help 2Menu 3View 4Edit 5Exec 6Logs 7Port-fwd 8Del 9Conn 10Quit         │
└──────────────────────────────────────────────────────────────────────────┘
```

**C — Command / connection palette (Ctrl-K)**
```
┌─ cairn ────────────────────────────────────────────────────────────────┐
│ ◉ local:~/proj    ╔═ Connect / Run ═══════════════════════════╗        │
│ ▸ ..              ║ > s3                                       ║        │
│   src/            ║ ───────────────────────────────────────── ║        │
│   Cargo.toml      ║  ⛁ s3://prod-backups        (saved)        ║        │
│   README.md       ║  ☁ gcs://analytics-export   (saved)        ║        │
│                   ║  🐳 docker · 14 containers                 ║        │
│                   ║  ⎈ k8s · prod, staging, minikube           ║        │
│                   ║  🔒 ssh · bastion.prod                     ║        │
│                   ║  + New connection…                         ║        │
│                   ╚════════════════════════════════════════════╝        │
```

**D — Agentic AI side panel (plan → confirm)**
```
┌─ cairn ────────────────────────────────────────────────────────────────┐
│ ◉ local:~/logs             │ 🤖 Assistant                               │
│   app-2026-06-27.log       │ You: archive logs older than 30d to s3      │
│   app-2026-06-26.log       │ AI: Plan (review before run):               │
│   nginx/                   │  1. select 14 files older than 2026-05-28   │
│                            │  2. copy → s3://prod-backups/logs/          │
│                            │  3. verify checksums                        │
│                            │  4. delete local originals  ⚠ destructive   │
│                            │ [Approve all] [Step through] [Edit] [Cancel]│
├────────────────────────────┴─────────────────────────────────────────────┤
│ ⌘K palette · ⌥A ask AI · ⌥/ search · F9 menu                            │
└────────────────────────────────────────────────────────────────────────────┘
```

---

## 9. Success Metrics

- **Adoption:** GitHub stars, downloads/installs, and number of distinct backends used per active user (a proxy for the "all-in-one" value landing).
- **Activation:** % of new users who connect at least one *non-local* backend within their first session.
- **Engagement:** weekly active users; cross-backend transfers performed (the differentiator).
- **AI value:** % of AI plans approved & executed; user-reported time saved; AI-assisted op success rate.
- **Trust:** zero plaintext-secret incidents; credential-related support issues trending down.
- **Performance:** UI stays responsive (no perceptible blocking) on large listings and slow networks; transfer throughput competitive with native CLIs.
- **Quality:** crash-free sessions; cross-platform parity (same feature set works on Linux/macOS/Windows).

---

## 10. Assumptions & Constraints

- Users have, or can provide, valid credentials for the cloud/infra backends they connect to; Cairn manages and secures them but does not provision access.
- Terminal capabilities vary widely; the product must degrade gracefully (color, fonts, width, mouse).
- AI quality and latency depend on the chosen provider/model; the product must remain fully usable with AI disabled.
- Cloud operations may incur cost (egress, API calls); the product surfaces risk but is not a billing tool.
- "All backends in v1" is an ambitious scope; depth-vs-breadth tradeoffs per backend are expected and will be detailed in the Implementation Plan.

---

## 11. Risks & Open Questions

| Risk / Question | Notes |
|-----------------|-------|
| **Scope of "all backends at launch"** | Seven backends in v1 is large; we may ship a thinner feature set per backend initially. To be sequenced in the Implementation Plan. |
| **Agentic AI safety on cloud ops** | Irreversible cloud deletes are higher-stakes than local. Plan→confirm mitigates; need clear "irreversible" signaling and possibly per-backend guardrails. |
| **Secrets portability vs. security** | Hybrid model must balance OS-keychain convenience with cross-machine portability. Details in LLD. |
| **Unified UX over divergent backends** | Expressing differing capabilities (no chmod on S3, live logs as "files") without confusing users. |
| **AI cost/privacy** | Cloud default is convenient but a secrets-handling tool attracts privacy-sensitive users; local-model path must be genuinely good. |
| **Cross-platform terminal quirks** | Windows path/console behavior and limited terminals are a recurring source of edge cases. |
| **Naming/trademark** | Confirm "Cairn" is clear of conflicts in the dev-tooling space before committing to branding. |

---

## 12. Out of Scope for this Document

The following are intentionally deferred to companion documents:

- **Low-Level Design (LLD):** VFS trait/abstraction model, async runtime and concurrency model, plugin/WASM sandbox interface, vault crypto design, AI tool/permission layer, data/state model, error handling, theming engine.
- **Implementation Plan:** milestone sequencing, per-backend rollout order, dependency graph, testing strategy, release/packaging pipeline, staffing/effort.

---

## 13. Glossary

- **Backend / VFS source:** any browsable, file-like system Cairn can connect to (local, SSH, S3, GCS, Azure, Docker, Kubernetes, …).
- **Pane:** one half of the dual-pane view, each bound to a backend location.
- **Cross-backend operation:** a transfer or comparison whose source and destination are different backends.
- **Vault:** Cairn's encrypted credential store.
- **Agentic AI:** the assistant that turns natural-language intent into a reviewable, executable plan.
- **Plan → confirm → execute:** the safety model where the AI never mutates state without explicit user approval.
