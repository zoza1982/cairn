//! Connection-form data types: field descriptors, per-scheme field lists, and the profile data
//! mirror that flows between the form overlay and the effect runner.
//!
//! **Design note:** this module deliberately mirrors `cairn_config::ConnectionProfile` without
//! importing it, keeping `cairn-core` free of the `cairn-config` dependency. The effect runner in
//! the binary crate translates between the two representations when saving.

use std::collections::BTreeMap;
use uuid::Uuid;

// ─────────────────────────────────────── FieldSpec ───────────────────────────────────────────

/// Static descriptor for one editable field in the connection form.
///
/// All pointers are `'static` (string literals), so the descriptors live as constants and are
/// never heap-allocated. The renderer reads these to build labelled input boxes; the reducer
/// uses them to validate that required fields are non-empty on submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec {
    /// The `endpoint` map key that will hold this field's value (e.g. `"host"`, `"bucket"`).
    pub key: &'static str,
    /// Short human-readable label shown left of the input box (e.g. `"Host"`).
    pub label: &'static str,
    /// Greyed-out example shown when the field is empty (e.g. `"192.168.1.1"`).
    pub placeholder: &'static str,
    /// Whether the form refuses to submit when this field is empty.
    pub required: bool,
    /// Whether the field holds a secret value (rendered masked). Not used in P4 — credential
    /// capture is deferred to P5 — but present here so P5 can enable it without an API break.
    pub secret: bool,
}

// ─────────────────────────────────────── ProfileData ─────────────────────────────────────────

/// A connection profile as the pure core sees it: endpoint data only, no I/O.
///
/// This mirrors `cairn_config::ConnectionProfile` field-for-field so the binary crate can
/// convert losslessly when saving. `secret_ref` is `None` for a newly-created connection
/// (credentials are wired up in P5); on edit it is **preserved** from the existing profile so a
/// saved vault secret is never silently dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileData {
    /// Stable UUID that identifies this profile across config reloads.
    pub id: Uuid,
    /// Scheme identifier matching one of the [`KNOWN_SCHEMES`] entries (e.g. `"ssh"`, `"s3"`).
    pub scheme: String,
    /// Human-readable name shown in the connection switcher.
    pub display_name: String,
    /// Per-field endpoint values keyed by [`FieldSpec::key`] (e.g. `{"host": "prod.example.com"}`).
    pub endpoint: BTreeMap<String, String>,
    /// Optional reference to a vault secret for this connection (P5+). `None` until credentials
    /// are configured; preserved from the existing profile when editing so it is never lost.
    pub secret_ref: Option<Uuid>,
}

// ──────────────────────────────────── Per-scheme field lists ─────────────────────────────────

static SSH_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "display_name",
        label: "Name",
        placeholder: "My SSH server",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "host",
        label: "Host",
        placeholder: "192.168.1.1",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "user",
        label: "User",
        placeholder: "admin",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "port",
        label: "Port",
        placeholder: "22",
        required: false,
        secret: false,
    },
];

static S3_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "display_name",
        label: "Name",
        placeholder: "My S3 bucket",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "bucket",
        label: "Bucket",
        placeholder: "my-bucket",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "region",
        label: "Region",
        placeholder: "us-east-1",
        required: false,
        secret: false,
    },
    FieldSpec {
        key: "endpoint_url",
        label: "Endpoint URL",
        placeholder: "https://s3.amazonaws.com",
        required: false,
        secret: false,
    },
];

static GCS_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "display_name",
        label: "Name",
        placeholder: "My GCS bucket",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "bucket",
        label: "Bucket",
        placeholder: "my-gcs-bucket",
        required: true,
        secret: false,
    },
];

static AZURE_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "display_name",
        label: "Name",
        placeholder: "My Azure container",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "account",
        label: "Account",
        placeholder: "mystorageaccount",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "container",
        label: "Container",
        placeholder: "mycontainer",
        required: true,
        secret: false,
    },
];

static LOCAL_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "display_name",
        label: "Name",
        placeholder: "My local directory",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "path",
        label: "Path",
        placeholder: "/home/user/data",
        required: true,
        secret: false,
    },
];

static GENERIC_FIELDS: &[FieldSpec] = &[FieldSpec {
    key: "display_name",
    label: "Name",
    placeholder: "My connection",
    required: true,
    secret: false,
}];

/// Return the ordered list of [`FieldSpec`]s for a scheme. Always includes `display_name` first.
///
/// Returns a generic single-field list for unknown schemes so the form still functions if new
/// backends are added before their field lists land.
#[must_use]
pub fn scheme_fields(scheme: &str) -> &'static [FieldSpec] {
    match scheme {
        "ssh" => SSH_FIELDS,
        "s3" => S3_FIELDS,
        "gcs" => GCS_FIELDS,
        "azure" => AZURE_FIELDS,
        "local" => LOCAL_FIELDS,
        _ => GENERIC_FIELDS,
    }
}

// ──────────────────────────────────── KNOWN_SCHEMES ──────────────────────────────────────────

/// The ordered list of schemes the form's scheme-picker presents.
///
/// Each entry is `(scheme_id, display_label)` — the id is the value stored in
/// `ProfileData::scheme`; the label is shown in the picker list.
pub const KNOWN_SCHEMES: &[(&str, &str)] = &[
    ("ssh", "SSH / SFTP"),
    ("s3", "Amazon S3 / S3-compatible"),
    ("gcs", "Google Cloud Storage"),
    ("azure", "Azure Blob Storage"),
    ("local", "Local directory"),
];

// ─────────────────────────────────────────── Tests ───────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_fields_ssh_has_host_and_user() {
        let fields = scheme_fields("ssh");
        assert!(fields.iter().any(|f| f.key == "host"));
        assert!(fields.iter().any(|f| f.key == "user"));
    }

    #[test]
    fn scheme_fields_s3_has_bucket() {
        let fields = scheme_fields("s3");
        assert!(fields.iter().any(|f| f.key == "bucket"));
    }

    #[test]
    fn scheme_fields_gcs_has_bucket() {
        let fields = scheme_fields("gcs");
        assert!(fields.iter().any(|f| f.key == "bucket"));
    }

    #[test]
    fn scheme_fields_azure_has_account_and_container() {
        let fields = scheme_fields("azure");
        assert!(fields.iter().any(|f| f.key == "account"));
        assert!(fields.iter().any(|f| f.key == "container"));
    }

    #[test]
    fn scheme_fields_local_has_path() {
        let fields = scheme_fields("local");
        assert!(fields.iter().any(|f| f.key == "path"));
    }

    #[test]
    fn scheme_fields_unknown_returns_generic() {
        let fields = scheme_fields("ftp");
        assert_eq!(fields, GENERIC_FIELDS);
    }

    #[test]
    fn all_schemes_have_display_name_first() {
        for (scheme, _) in KNOWN_SCHEMES {
            let fields = scheme_fields(scheme);
            assert!(!fields.is_empty(), "scheme {scheme} has no fields");
            assert_eq!(
                fields[0].key, "display_name",
                "first field of {scheme} must be display_name"
            );
        }
    }

    #[test]
    fn known_schemes_are_non_empty() {
        assert!(!KNOWN_SCHEMES.is_empty());
        for (id, label) in KNOWN_SCHEMES {
            assert!(!id.is_empty());
            assert!(!label.is_empty());
        }
    }

    #[test]
    fn profile_data_roundtrip() {
        let id = Uuid::new_v4();
        let mut ep = BTreeMap::new();
        ep.insert("host".to_owned(), "example.com".to_owned());
        ep.insert("user".to_owned(), "alice".to_owned());
        let profile = ProfileData {
            id,
            scheme: "ssh".to_owned(),
            display_name: "Test SSH".to_owned(),
            endpoint: ep.clone(),
            secret_ref: None,
        };
        assert_eq!(profile.id, id);
        assert_eq!(profile.scheme, "ssh");
        assert_eq!(profile.display_name, "Test SSH");
        assert_eq!(profile.endpoint, ep);
        assert!(profile.secret_ref.is_none());
    }
}
