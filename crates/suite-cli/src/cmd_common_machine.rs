use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::ValueEnum;
use serde_json::{json, Value};
use suite_packet_core::{EnvelopeV1, JsonProfile, PacketWrapperV1};

use crate::cmd_common_machine_support::{
    attach_artifact_handle, compact_packet_payload, extract_cache_hit, insert_payload_debug,
    refresh_packet_budget,
};
pub use crate::cmd_common_machine_support::{budget_retry_hint, cache_summary_line};

#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum JsonProfileArg {
    #[default]
    Compact,
    Full,
    Handle,
}

impl From<JsonProfileArg> for JsonProfile {
    fn from(value: JsonProfileArg) -> Self {
        match value {
            JsonProfileArg::Compact => JsonProfile::Compact,
            JsonProfileArg::Full => JsonProfile::Full,
            JsonProfileArg::Handle => JsonProfile::Handle,
        }
    }
}

pub fn resolve_machine_profile(
    json_profile: Option<JsonProfileArg>,
    legacy_format: Option<&str>,
    legacy_flag_name: &str,
) -> Result<Option<JsonProfile>> {
    if let Some(profile) = json_profile {
        if let Some(fmt) = legacy_format {
            if !fmt.eq_ignore_ascii_case("json") {
                anyhow::bail!(
                    "Conflicting output flags: --json and {} {}",
                    legacy_flag_name,
                    fmt
                );
            }
        }
        return Ok(Some(profile.into()));
    }

    if legacy_format.is_some_and(|fmt| fmt.eq_ignore_ascii_case("json")) {
        return Ok(Some(JsonProfile::Compact));
    }

    Ok(None)
}

pub fn emit_machine_envelope<T: serde::Serialize + Clone>(
    packet_type: &str,
    envelope: &EnvelopeV1<T>,
    profile: JsonProfile,
    pretty: bool,
    artifact_root: &Path,
    debug: Option<Value>,
) -> Result<()> {
    let value = machine_envelope_value(packet_type, envelope, profile, artifact_root, debug)?;
    emit_json(&value, pretty)
}

pub fn emit_machine_wrapper<T: serde::Serialize + Clone>(
    wrapper: &PacketWrapperV1<EnvelopeV1<T>>,
    profile: JsonProfile,
    pretty: bool,
    artifact_root: &Path,
    debug: Option<Value>,
) -> Result<()> {
    let value = machine_wrapper_value(wrapper, profile, artifact_root, debug)?;
    emit_json(&value, pretty)
}

pub fn machine_envelope_value<T: serde::Serialize + Clone>(
    packet_type: &str,
    envelope: &EnvelopeV1<T>,
    profile: JsonProfile,
    artifact_root: &Path,
    debug: Option<Value>,
) -> Result<Value> {
    let wrapper = PacketWrapperV1::new(packet_type.to_string(), envelope.clone());
    machine_wrapper_value(&wrapper, profile, artifact_root, debug)
}

pub fn machine_wrapper_value<T: serde::Serialize + Clone>(
    wrapper: &PacketWrapperV1<EnvelopeV1<T>>,
    profile: JsonProfile,
    artifact_root: &Path,
    debug: Option<Value>,
) -> Result<Value> {
    let mut packet = serde_json::to_value(&wrapper.packet)?;

    match profile {
        JsonProfile::Full => {
            if let Some(ref debug) = debug {
                insert_payload_debug(&mut packet, debug.clone());
            }
        }
        JsonProfile::Compact => {
            compact_packet_payload(&wrapper.packet_type, &mut packet);
        }
        JsonProfile::Handle => {
            let handle = suite_packet_core::write_packet_artifact(
                artifact_root,
                &wrapper.packet_type,
                &wrapper.packet,
            )
            .map_err(|source| anyhow::anyhow!(source.to_string()))?;
            compact_packet_payload(&wrapper.packet_type, &mut packet);
            attach_artifact_handle(&mut packet, serde_json::to_value(handle)?);
        }
    }

    refresh_packet_budget(&mut packet);

    let mut output = PacketWrapperV1::new(wrapper.packet_type.clone(), packet);
    output.schema_version = wrapper.schema_version.clone();
    output.cache_hit =
        wrapper.cache_hit || debug.as_ref().and_then(extract_cache_hit).unwrap_or(false);
    serde_json::to_value(output).map_err(Into::into)
}

pub fn emit_json(value: &Value, pretty: bool) -> Result<()> {
    if pretty {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", serde_json::to_string(value)?);
    }
    Ok(())
}

pub fn emit_machine_error(
    command: &str,
    error: &anyhow::Error,
    pretty: bool,
    target: Option<&str>,
    retry_hint: Option<Value>,
) -> Result<()> {
    let causes = error
        .chain()
        .skip(1)
        .map(|cause| Value::String(cause.to_string()))
        .collect::<Vec<_>>();
    emit_json(
        &json!({
            "schema_version": "suite.error.v1",
            "command": command,
            "message": error.to_string(),
            "target": target,
            "retry_hint": retry_hint,
            "causes": causes,
        }),
        pretty,
    )
}

pub fn resolve_artifact_root(explicit_root: Option<&Path>) -> PathBuf {
    if let Some(root) = explicit_root {
        return root.to_path_buf();
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
