// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! Temporary Rust consumer; run.py copies production codec/handshake modules into an OS temp crate.

mod agent_process;

use agent_process::codec::{decode_frame, CodecError, FrameKind, RpcFrame, DEFAULT_MAX_FRAME_BYTES};
use agent_process::error::AgentProcessError;
use agent_process::pending::{PendingRegistry, ResolveDisposition};
use agent_process::{LifecycleState, SidecarConfig, SidecarSupervisor, SessionEvent};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const EXPECTED_DIGEST_ENV: &str = "JA_CONTRACT_DIGEST";
const PROPERTY_PATH_ENV: &str = "JA_PROPERTY_PATH";
const PROPERTY_DIGEST_ENV: &str = "JA_PROPERTY_DIGEST";
const SIDECAR_EXECUTABLE_ENV: &str = "JA_RUST_SIDECAR_EXECUTABLE";
const SIDECAR_SCRIPT_ENV: &str = "JA_RUST_SIDECAR_SCRIPT";
const SIDECAR_SCENARIOS_ENV: &str = "JA_RUST_SIDECAR_SCENARIOS";
const GOLDEN_PATH: &str = "__GOLDEN_PATH__";
const PROPERTY_PATH: &str = "__PROPERTY_PATH__";

/// Runs every corpus stage behind a fixed marker so raw fixture values never become diagnostics.
fn main() {
    if run().is_err() {
        eprintln!("RUST_CONTRACT_FAIL classification=adapter_assertion");
        std::process::exit(1);
    }
}

/// Keeps all adapter assertions behind one result boundary so panics cannot expose fixture data.
fn run() -> Result<(), ()> {
    let golden = PathBuf::from(GOLDEN_PATH);
    if env::var_os("JA_RUST_SUPERVISOR_REPLAY_ONLY").is_some() {
        return run_supervisor_replay_only(&golden);
    }
    let property = PathBuf::from(env::var(PROPERTY_PATH_ENV).map_err(|_| ())?);
    let digest = corpus_digest(&golden).map_err(|_| ())?;
    if env::var(EXPECTED_DIGEST_ENV).map_err(|_| ())? != digest {
        eprintln!("RUST_CASE=digest_mismatch");
        return Err(());
    }
    let (valid_frames, method_results) = consume_valid(&golden)?;
    let parse_frames = consume_invalid(&golden)?;
    let (property_valid, property_invalid, property_digest) = consume_property(&property)?;
    if property_digest != env::var(PROPERTY_DIGEST_ENV).map_err(|_| ())? {
        eprintln!("RUST_CASE=property_digest");
        return Err(());
    }
    check_framing_boundaries()?;
    if valid_frames != 46
        || method_results != 12
        || parse_frames != 31
        || property_valid != 100
        || property_invalid != 100
    {
        eprintln!("RUST_CASE=count_assertion");
        return Err(());
    }
    println!(
        "RUST_CONTRACT_OK digest={digest} validFrames={valid_frames} methodResults={method_results} parseFrames={parse_frames} propertyValid={property_valid} propertyInvalid={property_invalid} propertyDigest={property_digest}"
    );
    Ok(())
}

/// Runs only the production Supervisor replay so a transport fixture can be diagnosed before the broad gate.
fn run_supervisor_replay_only(golden: &Path) -> Result<(), ()> {
    eprintln!("RUST_CASE=replay_start");
    let handshake = documents(&golden.join("valid").join("handshake.jsonl"))?;
    replay_valid_handshake_through_supervisor(&handshake)?;
    eprintln!("RUST_CASE=valid_replay_done");
    let invalid_cases = replay_invalid_handshake_cases(golden)?;
    eprintln!("RUST_CASE=invalid_replay_done");
    for document in documents(&golden.join("version").join("minor-compatible.json"))? {
        replay_minor_compatibility_through_supervisor(&document)?;
    }
    if handshake.len() != 6 || invalid_cases != 23 {
        return Err(());
    }
    println!(
        "RUST_SUPERVISOR_REPLAY_OK validFrames={} invalidCases={} minorCompatible=1",
        handshake.len(), invalid_cases
    );
    Ok(())
}

/// Hashes relative names and bytes exactly as the other consumers to prove one corpus was used.
fn corpus_digest(root: &Path) -> Result<String, std::io::Error> {
    let mut files = Vec::new();
    collect_json_files(root, &mut files)?;
    files.sort_by_key(|path| {
        path.strip_prefix(root)
            .expect("corpus path is contained")
            .to_string_lossy()
            .replace('\\', "/")
    });
    let mut digest = Sha256::new();
    for path in files {
        let relative = path
            .strip_prefix(root)
            .expect("corpus path is contained")
            .to_string_lossy()
            .replace('\\', "/");
        digest.update(relative.as_bytes());
        digest.update([0]);
        digest.update(fs::read(path)?);
        digest.update([0]);
    }
    hex_digest(digest.finalize())
}

/// Recursively selects only frozen JSON/JSONL inputs, excluding scripts and generated output.
fn collect_json_files(root: &Path, output: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_json_files(&path, output)?;
        } else if path.extension().is_some_and(|extension| extension == "json" || extension == "jsonl") {
            output.push(path);
        }
    }
    Ok(())
}

/// Reads JSONL one line at a time so a consumer cannot silently skip an individual frame.
fn documents(path: &Path) -> Result<Vec<Value>, ()> {
    let text = fs::read_to_string(path).map_err(|_| ())?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(|_| ()))
        .collect()
}

/// Sends valid frames through the production codec and preserves method/result identity.
fn consume_valid(golden: &Path) -> Result<(usize, usize), ()> {
    let mut files = Vec::new();
    collect_json_files(golden, &mut files).map_err(|_| ())?;
    files.sort();
    let mut count = 0;
    let mut method_results = 0;
    let mut pending_methods = HashMap::new();
    for file in files {
        if file.components().any(|component| component.as_os_str() == "invalid")
            || file.file_name().and_then(|name| name.to_str()) == Some("major-incompatible.json")
            || file.file_name().and_then(|name| name.to_str()) == Some("handshake.jsonl")
        {
            continue;
        }
        for document in documents(&file)? {
            let frame = decode_document(&document).map_err(|_| ())?;
            match frame.kind().map_err(|_| ())? {
                FrameKind::ClientRequest | FrameKind::ServerRequest => {
                    if let (Some(id), Some(method)) = (frame.id_opt(), frame.method()) {
                        pending_methods.insert(id.to_owned(), method.to_owned());
                    }
                }
                FrameKind::Response => {
                    if frame.result().is_present() {
                        let id = frame.id_opt().ok_or(())?;
                        if pending_methods.remove(id).is_none() {
                            return Err(());
                        }
                        preserve_result_payload(&frame, &document)?;
                        method_results += 1;
                    }
                }
                FrameKind::Notification => {}
            }
            count += 1;
        }
    }
    let handshake = documents(&golden.join("valid").join("handshake.jsonl"))?;
    replay_valid_handshake_through_supervisor(&handshake)?;
    count += handshake.len();
    Ok((count, method_results))
}

/// Replays all 23 invalid handshake cases through a real production supervisor generation.
fn consume_invalid(golden: &Path) -> Result<usize, ()> {
    let invalid = golden.join("invalid");
    let mut count = replay_invalid_handshake_cases(golden)?;
    for document in documents(&invalid.join("envelopes.jsonl"))? {
        if decode_document(&document).is_ok() {
            return Err(());
        }
        count += 1;
    }
    let mut pending = PendingRegistry::new(4, 8).map_err(|_| ())?;
    for (index, document) in documents(&invalid.join("duplicate-late-limit.jsonl"))?
        .into_iter()
        .enumerate()
    {
        let frame = decode_document(&document).map_err(|_| ())?;
        if frame.method() == Some("initialize") {
            let params = frame.params().ok_or(())?;
            if agent_process::validate_initialize_params(params).is_ok() {
                return Err(());
            }
        } else {
            let id = frame.id_opt().ok_or(())?.to_owned();
            if index == 0 {
                let deadline = Instant::now().checked_add(Duration::from_secs(1)).ok_or(())?;
                pending.register(id, deadline).map_err(|_| ())?;
                if pending.resolve(frame) != ResolveDisposition::Delivered {
                    return Err(());
                }
            } else if pending.resolve(frame) == ResolveDisposition::Delivered {
                return Err(());
            }
        }
        count += 1;
    }
    for document in documents(&golden.join("version").join("major-incompatible.json"))? {
        let frame = decode_document(&document).map_err(|_| ())?;
        let params = frame.params().ok_or(())?;
        if agent_process::validate_initialize_params(params).is_ok() {
            return Err(());
        }
        count += 1;
    }
    // The minor fixture must complete the production compatibility/promotion path;
    // a method-only parse would not prove the negotiated minor is usable.
    for document in documents(&golden.join("version").join("minor-compatible.json"))? {
        replay_minor_compatibility_through_supervisor(&document)?;
    }
    if count != 31 {
        return Err(());
    }
    Ok(count)
}

/// Replays all handshake challenge cases through production Supervisor generations and returns the case count.
fn replay_invalid_handshake_cases(golden: &Path) -> Result<usize, ()> {
    let invalid = golden.join("invalid");
    if env::var_os("JA_RUST_SUPERVISOR_REPLAY_ONLY").is_some() {
        eprintln!("RUST_CASE=invalid_docs_start");
    }
    let mut count = 0;
    let cases = documents(&invalid.join("handshake-challenge.jsonl"))?;
    if env::var_os("JA_RUST_SUPERVISOR_REPLAY_ONLY").is_some() {
        eprintln!("RUST_CASE=invalid_docs_loaded");
    }
    for (index, case_document) in cases
        .into_iter()
        .enumerate()
    {
        if env::var_os("JA_RUST_SUPERVISOR_REPLAY_ONLY").is_some() {
            eprintln!("RUST_CASE=enter_invalid_handshake_{index}");
        }
        let case_name = case_document
            .get("case")
            .and_then(Value::as_str)
            .ok_or(())?;
        let frames = case_document.get("frames").and_then(Value::as_array).ok_or(())?;
        if replay_invalid_handshake_through_supervisor(index, case_name, frames).is_err() {
            if !case_name.starts_with("error_") {
                eprintln!("RUST_CASE=invalid_handshake_{index}");
            }
            return Err(());
        }
        // One frozen handshake case is one supervisor rejection responsibility;
        // counting frames would let a partially replayed case appear complete.
        count += 1;
    }
    Ok(count)
}

/// Hashes an ordered 200-record classification stream so count-only adapters cannot pass.
fn consume_property(path: &Path) -> Result<(usize, usize, String), ()> {
    let mut valid = 0;
    let mut invalid = 0;
    let mut records = String::new();
    for (index, entry) in documents(path)?.into_iter().enumerate() {
        let expected = entry.get("kind").and_then(Value::as_str).ok_or(())?;
        let expected_valid = expected == "valid";
        let frame = entry.get("frame").ok_or(())?;
        let accepted = decode_document(frame).is_ok();
        if accepted != expected_valid {
            return Err(());
        }
        let classification = if accepted { "accepted" } else { "rejected" };
        records.push_str(&format!(
            "{{\"classification\":\"{classification}\",\"expected\":\"{expected}\",\"index\":{index}}}\n"
        ));
        if accepted { valid += 1; } else { invalid += 1; }
    }
    Ok((valid, invalid, sha256_text(&records)?))
}

/// Decodes one exact LF-delimited document through production Rust framing/envelope validation.
fn decode_document(document: &Value) -> Result<RpcFrame, ()> {
    let mut frame = serde_json::to_vec(document).map_err(|_| ())?;
    frame.push(b'\n');
    decode_frame(&frame, DEFAULT_MAX_FRAME_BYTES).map_err(|_| ())
}

/// Re-encodes a response and compares its parsed result, proving null/object payload preservation.
fn preserve_result_payload(frame: &RpcFrame, document: &Value) -> Result<(), ()> {
    let encoded = frame.encode(DEFAULT_MAX_FRAME_BYTES).map_err(|_| ())?;
    let round_trip = decode_frame(&encoded, DEFAULT_MAX_FRAME_BYTES).map_err(|_| ())?;
    let expected = document.get("result").ok_or(())?;
    if !round_trip.result().is_present() {
        return Err(());
    }
    if expected.is_null() {
        if round_trip.result().value().is_some() { return Err(()); }
    } else if round_trip.result().value() != Some(expected) {
        return Err(());
    }
    Ok(())
}

/// Builds the same production supervisor configuration used by the desktop host,
/// while pointing its executable at a temporary protocol-speaking child.
fn production_supervisor(
    scenario: &str,
    initialize_params: Option<Value>,
) -> Result<SidecarSupervisor, ()> {
    let executable = PathBuf::from(env::var(SIDECAR_EXECUTABLE_ENV).map_err(|_| ())?);
    let script = PathBuf::from(env::var(SIDECAR_SCRIPT_ENV).map_err(|_| ())?);
    let scenarios = PathBuf::from(env::var(SIDECAR_SCENARIOS_ENV).map_err(|_| ())?);
    let scenario_path = scenarios.join(scenario);
    if !scenario_path.is_file() {
        return Err(());
    }
    let run_dir = scenarios.parent().ok_or(())?;
    let mut config = SidecarConfig::new(executable, run_dir);
    config.args = vec![
        OsString::from(script),
        OsString::from(scenario_path),
    ];
    // The production config clears inherited variables; retain only the OS
    // runtime variables that Python needs to start on Windows.
    for name in ["SystemRoot", "SystemDrive", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = env::var_os(name) {
            config.env.insert(OsString::from(name), value);
        }
    }
    config.ready_timeout = Duration::from_secs(3);
    config.shutdown_timeout = Duration::from_secs(2);
    if let Some(params) = initialize_params {
        config.initialize_params = params;
    }
    SidecarSupervisor::new(config).map_err(|_| ())
}

/// Replays valid handshake evidence through Supervisor.start, Session event
/// dispatch, ready promotion, and Supervisor.shutdown instead of a copied FSM.
fn replay_valid_handshake_through_supervisor(frames: &[Value]) -> Result<(), ()> {
    if frames.len() != 6 {
        return Err(());
    }
    let mut supervisor = production_supervisor("valid.json", None)?;
    if env::var_os("JA_RUST_SUPERVISOR_REPLAY_ONLY").is_some() {
        eprintln!("RUST_CASE=valid_before_start");
    }
    if let Err(error) = supervisor.start() {
        eprintln!("RUST_CASE=valid_start_{}", classify_agent_error(&error));
        return Err(());
    }
    if env::var_os("JA_RUST_SUPERVISOR_REPLAY_ONLY").is_some() {
        eprintln!("RUST_CASE=valid_started");
    }
    if supervisor.state() != LifecycleState::Ready {
        return Err(());
    }
    if env::var_os("JA_RUST_SUPERVISOR_REPLAY_ONLY").is_some() {
        eprintln!("RUST_CASE=valid_ready");
    }
    let stopped = supervisor.next_event(Duration::from_secs(1)).ok_or(())?;
    let SessionEvent::Notification(frame) = stopped else {
        return Err(());
    };
    if frame.method() != Some("runtime/statusChanged")
        || frame.params().and_then(|params| params.get("status")).and_then(Value::as_str)
            != Some("stopped")
    {
        return Err(());
    }
    if env::var_os("JA_RUST_SUPERVISOR_REPLAY_ONLY").is_some() {
        eprintln!("RUST_CASE=valid_stopped_event");
    }
    supervisor.shutdown(Duration::from_secs(2)).map_err(|_| ())?;
    if env::var_os("JA_RUST_SUPERVISOR_REPLAY_ONLY").is_some() {
        eprintln!("RUST_CASE=valid_shutdown");
    }
    Ok(())
}

/// Replays one invalid case through the production supervisor and lets the
/// production Session/codec decide whether the case is rejected.
fn replay_invalid_handshake_through_supervisor(
    index: usize,
    case_name: &str,
    frames: &[Value],
) -> Result<(), ()> {
    if frames.is_empty() {
        return Err(());
    }
    if case_name.starts_with("error_") {
        return replay_error_case_through_supervisor(index, frames);
    }
    let scenario = format!("case-{index:02}.json");
    let mut supervisor = production_supervisor(&scenario, None)?;
    let result = supervisor.start();
    if result.is_ok() {
        return Err(());
    }
    Ok(())
}

/// Sends token-bearing error data as a real pending response so the production
/// parser—not a raw fixture walk—must reject the complete error payload.
fn replay_error_case_through_supervisor(index: usize, frames: &[Value]) -> Result<(), ()> {
    let scenario = format!("case-{index:02}.json");
    let mut supervisor = production_supervisor(&scenario, None)?;
    if let Err(error) = supervisor.start() {
        eprintln!(
            "RUST_CASE=invalid_error_start_{index}_{}",
            classify_agent_error(&error)
        );
        return Err(());
    }
    if supervisor.state() != LifecycleState::Ready {
        return Err(());
    }
    let response = supervisor.request("version", serde_json::json!({}), Duration::from_secs(2));
    if response.is_ok() {
        // A raw error-data token reaching a delivered response proves that the
        // production codec projected it away instead of refusing the frame.
        eprintln!("RUST_CASE=invalid_error_accepted_{index}");
        return Err(());
    }
    if frames.last().and_then(Value::as_object).is_none() {
        return Err(());
    }
    let _ = supervisor.shutdown(Duration::from_secs(2));
    Ok(())
}

/// Converts production start failures to bounded case diagnostics without exposing paths or payloads.
fn classify_agent_error(error: &AgentProcessError) -> &'static str {
    match error {
        AgentProcessError::HandshakeFailed => "handshake",
        AgentProcessError::ProtocolFault => "protocol",
        AgentProcessError::SessionClosed => "closed",
        AgentProcessError::ProcessExited => "exited",
        AgentProcessError::DeadlineExceeded => "deadline",
        AgentProcessError::Incompatible => "incompatible",
        _ => "other",
    }
}

/// Runs the minor-compatible initialize through version negotiation and ready
/// promotion while preserving the fixture's unknown optional field on wire.
fn replay_minor_compatibility_through_supervisor(document: &Value) -> Result<(), ()> {
    let frame = decode_document(document).map_err(|_| ())?;
    if frame.method() != Some("initialize") {
        return Err(());
    }
    let params = frame.params().ok_or(())?;
    if params.get("protocolMinor").and_then(Value::as_i64) != Some(1)
        || params.get("futureOptionalField").and_then(Value::as_str)
            != Some("old-client-ignores-this")
    {
        return Err(());
    }
    let mut initialize = params.clone();
    initialize
        .as_object_mut()
        .ok_or(())?
        .insert(
            "workspacePolicy".to_owned(),
            serde_json::json!({
                "mode": "plan",
                "network": "disabled",
                "enforcement": "unavailable",
                "protectedRoots": []
            }),
        );
    let mut supervisor = production_supervisor("minor.json", Some(initialize))?;
    supervisor.start().map_err(|_| ())?;
    if supervisor.state() != LifecycleState::Ready {
        return Err(());
    }
    supervisor.shutdown(Duration::from_secs(2)).map_err(|_| ())?;
    Ok(())
}

/// Hashes canonical classification records with shared UTF-8/LF semantics.
fn sha256_text(text: &str) -> Result<String, ()> {
    let mut digest = Sha256::new();
    digest.update(text.as_bytes());
    hex_digest(digest.finalize()).map_err(|_| ())
}

/// Converts a SHA-256 digest to lowercase hexadecimal without exposing fixture contents.
fn hex_digest(bytes: impl AsRef<[u8]>) -> Result<String, std::io::Error> {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes { write!(&mut output, "{byte:02x}").map_err(|_| std::io::Error::other("hex"))?; }
    Ok(output)
}

/// Exercises byte/framing edge cases required by the gate in addition to JSON fixture coverage.
fn check_framing_boundaries() -> Result<(), ()> {
    let valid = br#"{"jsonrpc":"2.0","method":"runtime/notice","params":{}}"#;
    let mut no_lf = valid.to_vec();
    if !matches!(decode_frame(&no_lf, DEFAULT_MAX_FRAME_BYTES), Err(CodecError::PartialFrame)) { return Err(()); }
    no_lf.push(b'\n');
    let mut crlf = no_lf.clone();
    crlf.insert(crlf.len() - 1, b'\r');
    if decode_frame(&crlf, DEFAULT_MAX_FRAME_BYTES).is_ok() { return Err(()); }
    let invalid_utf8 = [b'{', 0xff, b'}', b'\n'];
    if !matches!(decode_frame(&invalid_utf8, DEFAULT_MAX_FRAME_BYTES), Err(CodecError::InvalidUtf8)) { return Err(()); }
    let mut oversized = vec![b'x'; DEFAULT_MAX_FRAME_BYTES + 1];
    oversized.push(b'\n');
    if !matches!(decode_frame(&oversized, DEFAULT_MAX_FRAME_BYTES), Err(CodecError::FrameTooLarge { .. })) { return Err(()); }
    Ok(())
}
