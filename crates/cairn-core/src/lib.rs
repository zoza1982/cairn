//! The Cairn application core.
//!
//! Implements the Elm/TEA architecture: an [`AppState`] model, a set of [`Msg`]s, and a pure
//! [`update`] reducer that mutates the state and returns [`AppEffect`]s for the effect runner to
//! execute. The reducer performs **no I/O and never `.await`s** — that is what keeps the render path
//! non-blocking. Asynchronous results return as [`AppEvent`]s (wrapped in [`Msg::Event`]). See
//! `docs/LLD.md` §5.

pub mod forms;
mod msg;
mod state;
mod update;

pub use forms::{
    credential_method_fields, credential_methods, scheme_fields, scheme_needs_credentials,
    CredentialDraft, CredentialMethod, FieldSpec, OsSources, ProfileData, KNOWN_SCHEMES,
};
pub use msg::{Action, AppEffect, AppEvent, Msg, TextEdit, WriteBackMode};
pub use state::{
    detect_file_kind, next_theme, visible_connection_indices, ActiveTransfer, AppState,
    ArchiveFormat, ChoiceProvenance, ChoiceStatus, ConnectionChoice, ConnectionFormStage,
    ConnectionKind, ContentHash, DiscoverySource, FieldValue, FileKind, Listing, LogViewerId,
    LogViewerStatus, MaskedInput, MountFrame, OpKind, Overlay, PagerId, PagerMode, PagerStatus,
    PaneLocation, PaneState, PendingSave, PromptKind, QueuedTransfer, RemoteEditId, RemoteVersion,
    SessionEnd, SessionRecord, ShellActionMeta, Side, SortMode, TransferId, TransferPhase,
    WritebackChoice, WritebackConflictReason, LOG_VIEWER_MAX_BYTES, LOG_VIEWER_MAX_LINES,
    PAGER_HEX_ROW_BYTES, PAGER_MAX_BYTES, REMOTE_EDIT_MAX_BYTES, SESSION_OUTPUT_MAX_BYTES,
    SESSION_OUTPUT_MAX_LINES, THEME_PRESETS,
};
pub use update::{initial_effects, update, VAULT_PASSPHRASE_MIN_LEN};
