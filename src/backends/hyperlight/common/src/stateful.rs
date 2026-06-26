// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `StatefulSandboxBackend` impl for the Hyperlight + Unikraft micro-VM.
//!
//! Provides a 5-phase lifecycle (provision → start → exec → stop →
//! deprovision) where Python state persists across `exec` calls within a
//! single started session. This is the stateful counterpart to
//! `HyperlightScriptRunner`, which is hermetic (restores between calls).
//!
//! ## Lifecycle mapping
//!
//! | Phase       | What happens                                                   |
//! |-------------|----------------------------------------------------------------|
//! | provision   | Resolve image home, auto-install snapshot if needed, mint ID.  |
//! | start       | Create `pyhl::Runtime`, initial restore to warm state.         |
//! | exec        | `run_code_stateful` — NO restore, Python state persists.       |
//! | stop        | Drop the runtime (guest VM torn down).                         |
//! | deprovision | No-op — no OS-level resources outlive the runtime.             |
//!
//! ## State persistence
//!
//! The key difference from `HyperlightScriptRunner`: the one-shot runner
//! calls `Runtime::run_code` which restores the snapshot before every
//! call (hermetic). The stateful backend calls `Runtime::run_code_stateful`
//! which skips the restore, so globals, imports, and variables survive
//! across exec calls within the same start → stop window.

use std::collections::HashMap;
use std::path::PathBuf;

use hyperlight_unikraft::pyhl;
use hyperlight_unikraft::Preopen;

use wxc_common::models::ExecutionRequest;
use wxc_common::mxc_error::MxcError;
use wxc_common::state_aware_backend::{
    DeprovisionResult, ExecHandle, PipeHandle, ProvisionResult, StartResult,
    StatefulSandboxBackend, StopResult,
};

use super::{has_install_source, is_installed, HyperlightScriptRunner, INITRD_FILE, KERNEL_FILE};

/// Sentinel pipe handle value (invalid fd on Linux, null HANDLE on
/// Windows). The dispatcher's relay function only calls the waiter
/// closure — it does not read from these handles.
fn sentinel_pipe() -> PipeHandle {
    #[cfg(target_os = "windows")]
    // SAFETY: HANDLE is a pointer-sized wrapper; zeroed = null handle.
    unsafe { std::mem::zeroed() }
    #[cfg(not(target_os = "windows"))]
    -1
}

/// State for a single provisioned + started session.
struct ActiveSession {
    runtime: pyhl::Runtime,
    #[allow(dead_code)] // retained for future snapshot-on-stop support
    home: PathBuf,
}

/// Stateful sandbox backend for Hyperlight + Unikraft micro-VMs.
///
/// Each `start` creates a `pyhl::Runtime` loaded from the persisted
/// snapshot. Successive `exec` calls run Python code statelessly —
/// imports, variables, and side-effects persist across calls. `stop`
/// tears down the runtime.
pub struct HyperlightStatefulBackend {
    /// Active sessions keyed by sandbox_id.
    sessions: HashMap<String, ActiveSession>,
}

impl Default for HyperlightStatefulBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl HyperlightStatefulBackend {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// Extract the token portion from `"hl:<token>"`.
    fn extract_token(sandbox_id: &str) -> Result<&str, MxcError> {
        match sandbox_id.split_once(':') {
            Some((prefix, rest))
                if prefix
                    == <Self as StatefulSandboxBackend>::ID_PREFIX
                    && !rest.is_empty() =>
            {
                Ok(rest)
            }
            _ => Err(MxcError::malformed_id(format!(
                "expected {}:<token>, got {:?}",
                <Self as StatefulSandboxBackend>::ID_PREFIX,
                sandbox_id
            ))),
        }
    }
}

impl StatefulSandboxBackend for HyperlightStatefulBackend {
    const ID_PREFIX: &'static str = "hl";
    const BACKEND_KEY: &'static str = "hyperlight";

    // No per-phase config or metadata for the prototype.
    type ProvisionConfig = ();
    type StartConfig = ();
    type ExecConfig = ();
    type StopConfig = ();
    type DeprovisionConfig = ();
    type ProvisionMetadata = ();
    type StartMetadata = ();
    type StopMetadata = ();
    type DeprovisionMetadata = ();

    /// Resolve image home, verify the snapshot is installed (auto-install
    /// if kernel + initrd are present), and mint a sandbox ID.
    fn provision(
        &mut self,
        _request: &ExecutionRequest,
        _config: Option<()>,
    ) -> Result<ProvisionResult<()>, MxcError> {
        let home = HyperlightScriptRunner::resolve_home().map_err(|e| {
            MxcError::backend_error(format!("hyperlight provision: image home resolution: {e}"))
        })?;

        // Auto-install if snapshot is missing but raw artifacts exist.
        if !is_installed(&home) {
            if !has_install_source(&home) {
                return Err(MxcError::backend_error(
                    "no warmed snapshot and no kernel/initrd to install from. \
                     run `--setup-hyperlight` first."
                        .to_string(),
                ));
            }
            let kernel = home.join(KERNEL_FILE);
            let initrd = home.join(INITRD_FILE);
            let opts = pyhl::InstallOptions {
                home: &home,
                source: pyhl::InstallSource::Explicit {
                    kernel: &kernel,
                    initrd: &initrd,
                },
                mounts: &[],
                network: None,
                listen_ports: None,
                max_surrogates: Some(0),
                force: false,
            };
            pyhl::install(&opts).map_err(|e| {
                MxcError::backend_error(format!("hyperlight auto-install: {e:#}"))
            })?;
        }

        let token = wxc_common::id::mint_random_token();
        Ok(ProvisionResult {
            sandbox_id: format!("{}:{}", Self::ID_PREFIX, token),
            metadata: None,
        })
    }

    /// Create the `pyhl::Runtime` — loads the snapshot and performs the
    /// initial restore to reach the warm (post-warmup) interpreter state.
    fn start(
        &mut self,
        sandbox_id: &str,
        _request: &ExecutionRequest,
        _config: Option<()>,
    ) -> Result<StartResult<()>, MxcError> {
        let _ = Self::extract_token(sandbox_id)?;

        if self.sessions.contains_key(sandbox_id) {
            return Err(MxcError::already_started(format!(
                "session {sandbox_id} is already started"
            )));
        }

        let home = HyperlightScriptRunner::resolve_home().map_err(|e| {
            MxcError::backend_error(format!("hyperlight start: image home resolution: {e}"))
        })?;

        // No mounts, no network for the prototype. These can be wired
        // through per-phase config later.
        let preopens: Vec<Preopen> = Vec::new();
        let runtime =
            pyhl::Runtime::new(&home, &preopens, None, None, Some(0)).map_err(|e| {
                MxcError::backend_error(format!("hyperlight start: runtime init: {e:#}"))
            })?;

        self.sessions.insert(
            sandbox_id.to_string(),
            ActiveSession {
                runtime,
                home,
            },
        );

        Ok(StartResult { metadata: None })
    }

    /// Execute Python code in the running session. State persists across
    /// calls — no snapshot restore between execs.
    fn exec(
        &mut self,
        sandbox_id: &str,
        request: &ExecutionRequest,
        _config: Option<()>,
    ) -> Result<ExecHandle, MxcError> {
        let _ = Self::extract_token(sandbox_id)?;

        let session = self.sessions.get_mut(sandbox_id).ok_or_else(|| {
            MxcError::not_started(format!(
                "no running session for {sandbox_id} — call start first"
            ))
        })?;

        // run_code_stateful: skips restore, Python state persists.
        let timing = session
            .runtime
            .run_code_stateful(&request.script_code)
            .map_err(|e| {
                MxcError::backend_error(format!("hyperlight exec: {e:#}"))
            })?;

        let exit_code = timing.exit_code;
        let null = sentinel_pipe();

        Ok(ExecHandle {
            stdout: null,
            stderr: null,
            stdin: null,
            waiter: Box::new(move || Ok(exit_code)),
            terminator: Box::new(|| {}),
        })
    }

    /// Tear down the runtime. The guest VM is dropped and all Python
    /// state is lost.
    fn stop(
        &mut self,
        sandbox_id: &str,
        _request: &ExecutionRequest,
        _config: Option<()>,
    ) -> Result<StopResult<()>, MxcError> {
        let _ = Self::extract_token(sandbox_id)?;

        if self.sessions.remove(sandbox_id).is_none() {
            return Err(MxcError::already_stopped(format!(
                "no running session for {sandbox_id}"
            )));
        }

        Ok(StopResult { metadata: None })
    }

    /// No-op — there are no OS-level resources that outlive the runtime.
    fn deprovision(
        &mut self,
        sandbox_id: &str,
        _request: &ExecutionRequest,
        _config: Option<()>,
    ) -> Result<DeprovisionResult<()>, MxcError> {
        let _ = Self::extract_token(sandbox_id)?;

        // If the session is still running, stop it first.
        self.sessions.remove(sandbox_id);

        Ok(DeprovisionResult { metadata: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wxc_common::mxc_error::MxcErrorCode;

    // ====== Wire-format constants ======

    #[test]
    fn id_prefix_matches_wire_format() {
        assert_eq!(
            <HyperlightStatefulBackend as StatefulSandboxBackend>::ID_PREFIX,
            "hl"
        );
    }

    #[test]
    fn backend_key_matches_wire_format() {
        assert_eq!(
            <HyperlightStatefulBackend as StatefulSandboxBackend>::BACKEND_KEY,
            "hyperlight"
        );
    }

    // ====== sandbox_id parsing ======

    #[test]
    fn extract_token_unwraps_hl_prefix() {
        assert_eq!(
            HyperlightStatefulBackend::extract_token("hl:abcd1234").unwrap(),
            "abcd1234"
        );
    }

    #[test]
    fn extract_token_rejects_other_prefix() {
        let err = HyperlightStatefulBackend::extract_token("iso:abc").unwrap_err();
        assert_eq!(err.code, MxcErrorCode::MalformedId);
    }

    #[test]
    fn extract_token_rejects_missing_colon() {
        let err = HyperlightStatefulBackend::extract_token("no-colon").unwrap_err();
        assert_eq!(err.code, MxcErrorCode::MalformedId);
    }

    #[test]
    fn extract_token_rejects_empty_payload() {
        let err = HyperlightStatefulBackend::extract_token("hl:").unwrap_err();
        assert_eq!(err.code, MxcErrorCode::MalformedId);
    }

    // ====== Lifecycle state machine ======

    #[test]
    fn stop_without_start_returns_already_stopped() {
        let mut backend = HyperlightStatefulBackend::new();
        let err = backend
            .stop("hl:fake123", &ExecutionRequest::default(), None)
            .unwrap_err();
        assert_eq!(err.code, MxcErrorCode::AlreadyStopped);
    }

    #[test]
    fn exec_without_start_returns_not_started() {
        let mut backend = HyperlightStatefulBackend::new();
        let err = backend
            .exec(
                "hl:fake123",
                &ExecutionRequest {
                    script_code: "print('hi')".to_string(),
                    ..Default::default()
                },
                None,
            )
            .unwrap_err();
        assert_eq!(err.code, MxcErrorCode::NotStarted);
    }

    #[test]
    fn deprovision_without_session_is_ok() {
        let mut backend = HyperlightStatefulBackend::new();
        // deprovision is a no-op cleanup — succeeds even without a session
        backend
            .deprovision("hl:fake123", &ExecutionRequest::default(), None)
            .unwrap();
    }

    // ====== Default validation hooks ======

    #[test]
    fn default_validate_hooks_all_pass() {
        let b = HyperlightStatefulBackend::new();
        let req = ExecutionRequest::default();
        b.validate_provision(&req, None).unwrap();
        b.validate_start("hl:abcd1234", &req, None).unwrap();
        b.validate_exec("hl:abcd1234", &req, None).unwrap();
        b.validate_stop("hl:abcd1234", &req, None).unwrap();
        b.validate_deprovision("hl:abcd1234", &req, None).unwrap();
    }
}
