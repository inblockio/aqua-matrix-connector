//! Typed agent-instance data model + loader/validator.
//!
//! This crate is the **aqua-pinnable** model that both the orchestrator
//! (the extended heartbeat) and the in-container agent consume. It is pure
//! data + validation: no async, no Matrix, no I/O beyond reading config files.
//!
//! Three-level model (see `docs/plans/typed-agent-instances.md`):
//!
//! - [`AgentType`] — the type definition, one per `types/<name>.json`. The
//!   immutable, aqua-pinnable scope contract / image / memory policy / mounts
//!   / resources / auth for a kind of agent.
//! - [`InstanceBinding`] — one per `instances/<id>.toml`. A *slot*: which type,
//!   which `AGENT_TARGET` it reports to, plus minimal overrides. It stores
//!   **no identity** — identity is per-incarnation, self-minted by the
//!   container.
//! - [`ResolvedInstance`] — an [`AgentType`] with overrides applied, tagged
//!   with the instance `id` and `target`. This is what the orchestrator
//!   serializes to `/agent/config.json` and mounts into the container for the
//!   in-container agent to read back.
//!
//! ## Validation
//!
//! Every struct uses `#[serde(deny_unknown_fields)]`, so a typo in a config
//! file fails loudly at load time, and required (non-`Option`) fields are
//! enforced by serde. Beyond that, the loaders enforce structural invariants
//! (filename-stem == `name`, no duplicate type names, type-ref resolves).
//!
//! [`agent_type_schema`] derives the JSON Schema for [`AgentType`] via
//! `schemars` — the aqua-pinnable schema artifact. We deliberately do **not**
//! validate-on-load against the schema beyond serde for now; that is a seam
//! left open for a later aqua-fication pass (hash-pin the schema + contract).

use anyhow::{anyhow, bail, Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// Type definition (`types/<name>.json`)
// ---------------------------------------------------------------------------

/// An agent **type**: the aqua-pinnable definition of a kind of agent.
///
/// Deserialized from `types/<name>.json`; the file stem must equal `name`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentType {
    /// Unique type id. Must match the filename stem of `types/<name>.json`.
    pub name: String,
    /// Semver of *this type definition* (not the image).
    pub version: String,
    /// Human-readable description of what the type is for.
    pub description: String,
    /// Container image ref, e.g. `"aqua-matrix-agent:poc"`.
    pub image: String,
    /// Registry role, e.g. `"claude-channel"`.
    pub role: String,
    /// Matrix profile alias the incarnation presents.
    pub display_name: String,
    /// One-time greeting. May contain `{did}` / `{user_id}` placeholders — we
    /// only carry the string here; templating happens at send time, not load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello: Option<String>,
    /// Immutable pre-session scope contract.
    pub contract: Contract,
    /// Memory / transcript policy.
    pub memory: MemoryPolicy,
    /// Read-only reference mounts (e.g. aqua-rs-sdk, aqua-spec).
    pub ref_mounts: Vec<RefMount>,
    /// Container resource caps.
    pub resources: Resources,
    /// How the container is authenticated to Claude.
    pub auth: Auth,
    /// Host/domain allowlist for egress. Carried but unused now — a seam for a
    /// later network policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_allowlist: Option<Vec<String>>,
}

/// The immutable pre-session scope contract: what the agent is *allowed* to do,
/// frozen for the life of the container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    /// Claude `--allowedTools` patterns.
    pub allowed_tools: Vec<String>,
    /// Claude `settings.json` permissions block.
    pub permissions: Permissions,
}

/// Claude `settings.json` permissions block (allow / deny patterns).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

/// Memory / transcript policy for the container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemoryPolicy {
    /// `CLAUDE_CONFIG_DIR` path inside the container, e.g. `"/agent/memory"`.
    pub config_dir: String,
    /// Logging-gated transcripts toggle.
    pub log_transcripts: bool,
}

/// A read-only reference mount: a host path made available inside the container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RefMount {
    pub host_path: String,
    pub container_path: String,
    pub read_only: bool,
}

/// Container resource caps. Also the override granularity for per-instance
/// resource tuning (see [`InstanceOverrides`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub memory_mb: u64,
    pub cpus: f64,
    pub pids_limit: u64,
}

/// How the container authenticates to Claude.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub method: AuthMethod,
    /// Env var that carries the secret into the container, e.g.
    /// `"CLAUDE_CODE_OAUTH_TOKEN"`. The secret value itself is never stored
    /// here — only the *name* of the variable the orchestrator injects.
    pub env_var: String,
}

/// Authentication method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// Subscription OAuth token (`subscription_oauth_token`).
    SubscriptionOauthToken,
    /// Plain API key (`api_key`).
    ApiKey,
}

// ---------------------------------------------------------------------------
// Instance binding (`instances/<id>.toml`)
// ---------------------------------------------------------------------------

/// An instance **slot**: which type to run, who it reports to, plus minimal
/// overrides. Deserialized from `instances/<id>.toml`.
///
/// **No identity field** — identity is per-incarnation, self-minted by the
/// container. Replacing the container yields a new identity for the same slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceBinding {
    /// Unique instance label.
    pub id: String,
    /// References an [`AgentType::name`]. Serialized as `type` in TOML.
    #[serde(rename = "type")]
    pub type_name: String,
    /// The `AGENT_TARGET` MXID this instance reports to / its allow-list
    /// receiver.
    pub target: String,
    /// Optional per-instance overrides applied on top of the type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<InstanceOverrides>,
}

/// Minimal per-instance overrides. Anything `Some` here wins over the type's
/// value when resolving.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
}

// ---------------------------------------------------------------------------
// Resolved instance (what the orchestrator mounts as /agent/config.json)
// ---------------------------------------------------------------------------

/// An [`AgentType`] with overrides applied, tagged with the binding's `id` and
/// `target`. This is the concrete, self-contained config the orchestrator
/// serializes to `/agent/config.json` and mounts into the container so the
/// in-container agent can read its role/contract/memory policy/hello/etc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolvedInstance {
    /// The instance slot label (from [`InstanceBinding::id`]).
    pub id: String,
    /// The `AGENT_TARGET` MXID (from [`InstanceBinding::target`]).
    pub target: String,
    /// The fully-resolved type with overrides applied.
    #[serde(flatten)]
    pub agent_type: AgentType,
}

// ---------------------------------------------------------------------------
// Loader / validator
// ---------------------------------------------------------------------------

/// Load every `*.json` file in `dir` as an [`AgentType`], keyed by `name`.
///
/// Errors if a file's `name` does not equal its filename stem, or if two files
/// declare the same `name`.
pub fn load_types(dir: &Path) -> Result<HashMap<String, AgentType>> {
    let mut types: HashMap<String, AgentType> = HashMap::new();

    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read types dir: {}", dir.display()))?;

    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("non-UTF-8 filename: {}", path.display()))?
            .to_string();

        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read type file: {}", path.display()))?;
        let agent_type: AgentType = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse type file: {}", path.display()))?;

        if agent_type.name != stem {
            bail!(
                "type name {:?} does not match filename stem {:?} in {}",
                agent_type.name,
                stem,
                path.display()
            );
        }

        if types.contains_key(&agent_type.name) {
            bail!(
                "duplicate type name {:?} (second occurrence in {})",
                agent_type.name,
                path.display()
            );
        }

        types.insert(agent_type.name.clone(), agent_type);
    }

    Ok(types)
}

/// Load every `*.toml` file in `dir` as an [`InstanceBinding`], skipping any
/// file whose name ends in `.example` (the committed templates).
pub fn load_instances(dir: &Path) -> Result<Vec<InstanceBinding>> {
    let mut instances = Vec::new();

    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read instances dir: {}", dir.display()))?;

    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("non-UTF-8 filename: {}", path.display()))?;

        // Skip committed templates (e.g. `claude-channel-1.toml.example`).
        if file_name.ends_with(".example") {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }

        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read instance file: {}", path.display()))?;
        let binding: InstanceBinding = toml::from_str(&contents)
            .with_context(|| format!("failed to parse instance file: {}", path.display()))?;

        instances.push(binding);
    }

    Ok(instances)
}

/// Resolve an [`InstanceBinding`] against the loaded types: look up its type,
/// apply overrides, and tag with `id` + `target`.
///
/// Errors if the binding's `type` does not name a known type.
pub fn resolve(
    instance: &InstanceBinding,
    types: &HashMap<String, AgentType>,
) -> Result<ResolvedInstance> {
    let mut agent_type = types
        .get(&instance.type_name)
        .ok_or_else(|| {
            anyhow!(
                "instance {:?} references unknown type {:?}",
                instance.id,
                instance.type_name
            )
        })?
        .clone();

    if let Some(overrides) = &instance.overrides {
        if let Some(display_name) = &overrides.display_name {
            agent_type.display_name = display_name.clone();
        }
        if let Some(hello) = &overrides.hello {
            agent_type.hello = Some(hello.clone());
        }
        if let Some(resources) = &overrides.resources {
            agent_type.resources = resources.clone();
        }
    }

    Ok(ResolvedInstance {
        id: instance.id.clone(),
        target: instance.target.clone(),
        agent_type,
    })
}

// ---------------------------------------------------------------------------
// Schema artifact
// ---------------------------------------------------------------------------

/// Derive the JSON Schema for [`AgentType`] as a `serde_json::Value`.
///
/// This is the aqua-pinnable schema artifact: a stable, machine-readable
/// description of the type definition that a later aqua-fication pass can
/// hash-pin. We currently only *produce* it (and assert its shape in tests);
/// validate-on-load against it is a deliberate seam left for later.
pub fn agent_type_schema() -> serde_json::Value {
    let schema = schemars::schema_for!(AgentType);
    serde_json::to_value(schema).expect("AgentType JSON Schema is serializable")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Path to the crate's `Cargo.toml` dir (workspace member root), so tests
    /// can reach the committed `types/` and `instances/` example artifacts at
    /// the workspace root regardless of the test CWD.
    fn workspace_root() -> PathBuf {
        // CARGO_MANIFEST_DIR == .../crates/aqua-matrix-template
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn sample_type() -> AgentType {
        AgentType {
            name: "claude-channel".into(),
            version: "0.1.0".into(),
            description: "test type".into(),
            image: "aqua-matrix-agent:poc".into(),
            role: "claude-channel".into(),
            display_name: "claude-channel".into(),
            hello: Some("hi {user_id}".into()),
            contract: Contract {
                allowed_tools: vec!["Read".into(), "mcp__ask__ask_human".into()],
                permissions: Permissions {
                    allow: vec!["Read".into()],
                    deny: vec!["Bash(rm:*)".into()],
                },
            },
            memory: MemoryPolicy {
                config_dir: "/agent/memory".into(),
                log_transcripts: true,
            },
            ref_mounts: vec![RefMount {
                host_path: "../aqua-rs-sdk".into(),
                container_path: "/refs/aqua-rs-sdk".into(),
                read_only: true,
            }],
            resources: Resources {
                memory_mb: 2048,
                cpus: 2.0,
                pids_limit: 512,
            },
            auth: Auth {
                method: AuthMethod::SubscriptionOauthToken,
                env_var: "CLAUDE_CODE_OAUTH_TOKEN".into(),
            },
            egress_allowlist: None,
        }
    }

    #[test]
    fn agent_type_json_roundtrip() {
        let original = sample_type();
        let json = serde_json::to_string_pretty(&original).unwrap();
        let parsed: AgentType = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);

        // AuthMethod renames to snake_case on the wire.
        assert!(json.contains("\"subscription_oauth_token\""));
    }

    #[test]
    fn load_committed_type_and_resolve_with_overrides() {
        let root = workspace_root();
        let types = load_types(&root.join("types")).unwrap();
        assert!(
            types.contains_key("claude-channel"),
            "committed claude-channel.json should load"
        );

        let committed = &types["claude-channel"];
        assert_eq!(committed.image, "aqua-matrix-agent:poc");
        assert_eq!(committed.memory.config_dir, "/agent/memory");
        assert!(committed.memory.log_transcripts);
        assert_eq!(committed.auth.method, AuthMethod::SubscriptionOauthToken);
        assert_eq!(committed.auth.env_var, "CLAUDE_CODE_OAUTH_TOKEN");
        // Two RO ref mounts: aqua-rs-sdk + aqua-spec.
        assert_eq!(committed.ref_mounts.len(), 2);
        assert!(committed.ref_mounts.iter().all(|m| m.read_only));

        // Copy the .example binding to a temp dir as a real `.toml`, then load.
        let tmp = std::env::temp_dir().join("aqua-matrix-template-test-instances");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let example = fs::read_to_string(
            root.join("instances/claude-channel-1.toml.example"),
        )
        .unwrap();
        fs::write(tmp.join("claude-channel-1.toml"), &example).unwrap();
        // Drop an `.example` next to it to prove it is skipped.
        fs::write(tmp.join("ignored.toml.example"), &example).unwrap();

        let mut instances = load_instances(&tmp).unwrap();
        assert_eq!(instances.len(), 1, "the .example file must be skipped");
        let binding = instances.pop().unwrap();
        assert_eq!(binding.id, "claude-channel-1");
        assert_eq!(binding.type_name, "claude-channel");

        let resolved = resolve(&binding, &types).unwrap();
        assert_eq!(resolved.id, "claude-channel-1");
        assert_eq!(resolved.target, binding.target);
        // No overrides in the example -> type values carried straight through.
        assert_eq!(resolved.agent_type.display_name, committed.display_name);
        assert_eq!(resolved.agent_type.image, committed.image);

        // Now prove overrides win.
        let overridden = InstanceBinding {
            overrides: Some(InstanceOverrides {
                display_name: Some("claude-channel-prod".into()),
                hello: Some("override hello".into()),
                resources: Some(Resources {
                    memory_mb: 4096,
                    cpus: 4.0,
                    pids_limit: 1024,
                }),
            }),
            ..binding
        };
        let resolved = resolve(&overridden, &types).unwrap();
        assert_eq!(resolved.agent_type.display_name, "claude-channel-prod");
        assert_eq!(resolved.agent_type.hello.as_deref(), Some("override hello"));
        assert_eq!(resolved.agent_type.resources.memory_mb, 4096);
        // Untouched fields still come from the type.
        assert_eq!(resolved.agent_type.image, committed.image);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unknown_field_is_rejected() {
        // A valid type JSON with one extra, unexpected field.
        let json = serde_json::to_value(sample_type()).unwrap();
        let mut obj = json.as_object().unwrap().clone();
        obj.insert("surprise".into(), serde_json::json!("boom"));
        let text = serde_json::to_string(&obj).unwrap();
        let err = serde_json::from_str::<AgentType>(&text).unwrap_err();
        assert!(
            err.to_string().contains("surprise")
                || err.to_string().contains("unknown field"),
            "deny_unknown_fields should reject extra fields, got: {err}"
        );
    }

    #[test]
    fn missing_type_ref_errors() {
        let types: HashMap<String, AgentType> = HashMap::new();
        let binding = InstanceBinding {
            id: "orphan".into(),
            type_name: "does-not-exist".into(),
            target: "@you:example.com".into(),
            overrides: None,
        };
        let err = resolve(&binding, &types).unwrap_err();
        assert!(
            err.to_string().contains("unknown type"),
            "resolve should reject an unknown type ref, got: {err}"
        );
    }

    #[test]
    fn type_name_must_match_filename_stem() {
        let tmp = std::env::temp_dir().join("aqua-matrix-template-test-stem");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        // File is `wrong-stem.json` but declares name "claude-channel".
        let json = serde_json::to_string(&sample_type()).unwrap();
        fs::write(tmp.join("wrong-stem.json"), json).unwrap();
        let err = load_types(&tmp).unwrap_err();
        assert!(
            err.to_string().contains("does not match filename stem"),
            "load_types should reject stem mismatch, got: {err}"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn schema_has_expected_required_keys() {
        let schema = agent_type_schema();
        let obj = schema.as_object().expect("schema is a JSON object");
        assert_eq!(
            obj.get("type").and_then(|t| t.as_str()),
            Some("object"),
            "AgentType schema describes an object"
        );

        let props = obj
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("schema has properties");
        for key in [
            "name",
            "version",
            "description",
            "image",
            "role",
            "display_name",
            "contract",
            "memory",
            "ref_mounts",
            "resources",
            "auth",
        ] {
            assert!(props.contains_key(key), "schema should describe `{key}`");
        }

        let required: Vec<String> = obj
            .get("required")
            .and_then(|r| r.as_array())
            .expect("schema has a required list")
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        for key in [
            "name", "version", "description", "image", "role", "display_name", "contract",
            "memory", "ref_mounts", "resources", "auth",
        ] {
            assert!(
                required.contains(&key.to_string()),
                "`{key}` should be required in the schema"
            );
        }
        // Optional fields must NOT be required.
        assert!(!required.contains(&"hello".to_string()));
        assert!(!required.contains(&"egress_allowlist".to_string()));
    }
}
