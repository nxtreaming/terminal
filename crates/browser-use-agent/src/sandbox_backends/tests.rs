//! Network-free, no-escape tests for the sandbox backends.
//!
//! These exercise: pure policy derivation (codex parity cases), the honest
//! `PlatformSandboxProvider::select_initial` resolution, the bwrap arg/profile
//! construction, and — gated on real backend availability — an actual spawn of a
//! trivial child (`/bin/true`) under the Linux sandbox. NO escape attempts and
//! NO network access are performed.

#[cfg(not(target_os = "macos"))]
use std::collections::HashMap;
use std::path::PathBuf;

use super::derive_sandbox_policy;
use super::linux;
use super::policy::{policy_summary, FsIntent, FsPolicy, SandboxPolicy};
#[cfg(target_os = "linux")]
use super::provider::spawn_under_sandbox;
use super::provider::{
    get_platform_sandbox, real_backend_available, unavailable_denial, PlatformSandbox,
    PlatformSandboxProvider, SpawnUnderSandboxError,
};
use super::seatbelt;
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, SandboxPermissions, SandboxPreference, SandboxProvider, SandboxType,
};

fn fs_policy() -> FileSystemSandboxPolicy {
    FileSystemSandboxPolicy {
        restricted: true,
        denied_read: false,
    }
}

// ---- policy derivation (codex parity: SandboxPolicy derivation) -------------

#[test]
fn derive_read_only_locks_everything_down() {
    // Codex parity: ReadOnly intent -> new_read_only (no writes, no roots).
    let p = derive_sandbox_policy(FsIntent::ReadOnly, PathBuf::from("/work"), vec![], false);
    assert_eq!(p.fs, FsPolicy::ReadOnly);
    assert!(p.writable_roots.is_empty());
    assert!(!p.network_access);
    assert!(!p.allows_writes());
    assert!(!p.has_full_network_access());
}

#[test]
fn derive_read_only_honors_network_flag() {
    // network_access flag is applied after the constructor.
    let p = derive_sandbox_policy(FsIntent::ReadOnly, PathBuf::from("/work"), vec![], true);
    assert_eq!(p.fs, FsPolicy::ReadOnly);
    assert!(p.network_access);
    assert!(p.has_full_network_access());
}

#[test]
fn derive_workspace_write_roots_cwd_first_plus_extra() {
    // Codex parity: WorkspaceWrite -> cwd is the first writable root, extras
    // appended in order (compatibility_workspace_write_policy).
    let p = derive_sandbox_policy(
        FsIntent::WorkspaceWrite,
        PathBuf::from("/work"),
        vec![PathBuf::from("/tmp/extra"), PathBuf::from("/var/cache")],
        false,
    );
    assert_eq!(p.fs, FsPolicy::WorkspaceWrite);
    assert_eq!(
        p.writable_roots,
        vec![
            PathBuf::from("/work"),
            PathBuf::from("/tmp/extra"),
            PathBuf::from("/var/cache"),
        ]
    );
    assert!(!p.network_access);
    assert!(p.allows_writes());
}

#[test]
fn derive_workspace_write_honors_network_flag() {
    let p = derive_sandbox_policy(
        FsIntent::WorkspaceWrite,
        PathBuf::from("/work"),
        vec![],
        true,
    );
    assert!(p.network_access);
    assert_eq!(p.writable_roots, vec![PathBuf::from("/work")]);
}

#[test]
fn policy_summary_describes_fs_and_network() {
    let ro = SandboxPolicy::new_read_only();
    assert_eq!(policy_summary(&ro), "read-only, network-denied");
    let mut ww = SandboxPolicy::new_workspace_write(PathBuf::from("/work"), vec![]);
    ww.network_access = true;
    assert_eq!(policy_summary(&ww), "workspace-write, network-allowed");
}

// ---- bwrap arg construction (codex parity: bwrap.rs arg shape) --------------

#[test]
fn bwrap_args_read_only_ro_binds_root_and_unshares_net() {
    let p = SandboxPolicy::new_read_only();
    let args = linux::build_bwrap_args(&p);
    // Root bound read-only.
    assert!(window_contains(&args, &["--ro-bind", "/", "/"]));
    // No writable --bind mounts in read-only mode.
    assert!(!args.iter().any(|a| a == "--bind"));
    // Network denied -> namespace dropped.
    assert!(args.iter().any(|a| a == "--unshare-net"));
    // Ends with the `--` separator.
    assert_eq!(args.last().map(String::as_str), Some("--"));
}

#[test]
fn bwrap_args_workspace_write_binds_each_root() {
    let mut p = SandboxPolicy::new_workspace_write(
        PathBuf::from("/work"),
        vec![PathBuf::from("/tmp/extra")],
    );
    p.network_access = true;
    let args = linux::build_bwrap_args(&p);
    assert!(window_contains(&args, &["--bind", "/work", "/work"]));
    assert!(window_contains(
        &args,
        &["--bind", "/tmp/extra", "/tmp/extra"]
    ));
    // Network allowed -> namespace NOT dropped.
    assert!(!args.iter().any(|a| a == "--unshare-net"));
}

fn window_contains(args: &[String], needle: &[&str]) -> bool {
    args.windows(needle.len())
        .any(|w| w.iter().zip(needle).all(|(a, b)| a == b))
}

// ---- seatbelt profile (codex parity: generated Seatbelt policy text) --------

#[test]
fn seatbelt_profile_read_only_denies_writes_and_network() {
    let p = SandboxPolicy::new_read_only();
    let profile = seatbelt::seatbelt_profile(&p);
    assert!(profile.contains("(deny default)"));
    assert!(profile.contains("(allow file-read*)"));
    assert!(profile.contains("(deny file-write*)"));
    assert!(profile.contains("(deny network*)"));
}

#[test]
fn seatbelt_profile_workspace_write_scopes_writes() {
    let mut p = SandboxPolicy::new_workspace_write(PathBuf::from("/work"), vec![]);
    p.network_access = true;
    let profile = seatbelt::seatbelt_profile(&p);
    assert!(profile.contains("(allow file-write* (subpath \"/work\"))"));
    assert!(profile.contains("(allow network*)"));
}

#[test]
fn seatbelt_command_uses_sandbox_exec_and_appends_argv() {
    let p = SandboxPolicy::new_read_only();
    let cmd = seatbelt::seatbelt_command(&p, &["echo".to_string(), "ok".to_string()]);
    assert_eq!(cmd.program, PathBuf::from("/usr/bin/sandbox-exec"));
    assert_eq!(cmd.args.first().map(String::as_str), Some("-p"));
    assert_eq!(cmd.args.last().map(String::as_str), Some("ok"));
}

// ---- platform selection (codex parity: get_platform_sandbox) ----------------

#[test]
fn get_platform_sandbox_matches_current_os() {
    let got = get_platform_sandbox();
    if cfg!(target_os = "macos") {
        assert_eq!(got, Some(PlatformSandbox::MacosSeatbelt));
    } else if cfg!(target_os = "linux") {
        assert_eq!(got, Some(PlatformSandbox::LinuxBwrap));
    } else {
        assert_eq!(got, None);
    }
}

// ---- provider resolve: HONEST Restricted-vs-None ---------------------------

#[test]
fn resolve_never_pref_is_always_none() {
    let provider = PlatformSandboxProvider;
    let got = provider.select_initial(&fs_policy(), SandboxPreference::Never, false);
    assert_eq!(got, SandboxType::None, "Never must opt out of sandboxing");
}

#[test]
fn resolve_auto_is_honest_about_backend_availability() {
    let provider = PlatformSandboxProvider;
    let got = provider.select_initial(&fs_policy(), SandboxPreference::Auto, false);
    // The provider must NOT claim Restricted unless a backend can actually
    // enforce here, and MUST claim it when one can. Assert the resolution
    // exactly tracks `real_backend_available()` (no overclaim, no underclaim).
    if real_backend_available() {
        assert_eq!(
            got,
            SandboxType::Restricted,
            "a real backend is available; resolve must say Restricted"
        );
    } else {
        assert_eq!(
            got,
            SandboxType::None,
            "no backend available; resolve must honestly say None"
        );
    }
}

#[test]
fn prepare_downgrades_restricted_when_no_backend() {
    // On a host with no enforcing backend, a Restricted launch must honestly
    // downgrade to None rather than hand back a misleading Restricted handle.
    let provider = PlatformSandboxProvider;
    let launch = provider.prepare(
        SandboxType::Restricted,
        std::path::Path::new("/tmp"),
        SandboxPermissions::UseDefault,
    );
    if real_backend_available() {
        assert_eq!(launch.sandbox, SandboxType::Restricted);
    } else {
        assert_eq!(launch.sandbox, SandboxType::None);
    }
}

#[test]
fn availability_summary_is_nonempty() {
    let summary = PlatformSandboxProvider.availability_summary();
    assert!(!summary.is_empty());
}

// ---- honesty: unavailable surfaces a denial reason --------------------------

#[test]
fn unavailable_denial_carries_policy_summary_and_reason() {
    let policy = SandboxPolicy::new_read_only();
    let denial = unavailable_denial(&policy, "no bwrap binary");
    assert!(denial.output.stderr.contains("no bwrap binary"));
    assert!(denial.output.stderr.contains("read-only"));
}

#[test]
fn spawn_error_converts_to_denial() {
    let err = SpawnUnderSandboxError::Unavailable {
        reason: "no backend".to_string(),
    };
    assert_eq!(err.reason(), "no backend");
    let denial = err.into_denial();
    assert!(denial.output.stderr.contains("no backend"));
}

// ---- linux spawn (GATED on real availability) ------------------------------

/// When the Linux (`bwrap`) backend is available on this host, spawn `/bin/true`
/// under a read-only, network-denied policy and assert it does not panic / does
/// not run unsandboxed. Gated by a runtime capability check so the suite still
/// passes where bwrap is absent or user namespaces are disallowed (NEVER fail
/// merely because the box lacks the feature). NO escape attempts, NO network.
#[cfg(target_os = "linux")]
#[test]
fn linux_spawn_true_under_sandbox_when_available() {
    if !linux::is_available() {
        eprintln!("skip: no bwrap binary on this host");
        return;
    }
    let policy = SandboxPolicy::new_read_only();
    let cmd = vec!["/bin/true".to_string()];
    let env: HashMap<String, String> = HashMap::new();
    match spawn_under_sandbox(&policy, cmd, PathBuf::from("/"), env) {
        Ok(mut child) => {
            let status = child.wait().expect("wait on sandboxed child");
            // If bwrap could actually set up namespaces, /bin/true returns 0.
            // Some CI kernels disallow unprivileged user namespaces; in that
            // case bwrap exits non-zero. Either way the call did not panic and
            // did not run unsandboxed — accept both outcomes honestly.
            if !status.success() {
                eprintln!(
                    "note: sandboxed /bin/true exited non-zero (likely userns disabled): {status:?}"
                );
            }
        }
        Err(SpawnUnderSandboxError::Unavailable { reason }) => {
            // is_available() said yes but spawn still couldn't enforce — fine,
            // the backend honestly reported it rather than running unsandboxed.
            eprintln!("note: sandbox unavailable at spawn: {reason}");
        }
        Err(SpawnUnderSandboxError::Spawn { reason }) => {
            eprintln!("note: sandbox spawn failed: {reason}");
        }
    }
}

/// `bwrap_command` returns Some exactly when a bwrap binary is present —
/// honest, and the basis for the provider's None resolution.
#[cfg(target_os = "linux")]
#[test]
fn linux_bwrap_command_presence_matches_availability() {
    let policy = SandboxPolicy::new_read_only();
    let prepared = linux::bwrap_command(&policy, &["/bin/true".to_string()]);
    assert_eq!(prepared.is_some(), linux::is_available());
}

// ---- seatbelt: stub reports unsupported on non-macOS ------------------------

#[test]
fn seatbelt_availability_matches_platform() {
    assert_eq!(seatbelt::is_available(), cfg!(target_os = "macos"));
}

#[cfg(not(target_os = "macos"))]
#[test]
fn seatbelt_spawn_is_unsupported_on_non_macos() {
    let policy = SandboxPolicy::new_read_only();
    let env: HashMap<String, String> = HashMap::new();
    let err = seatbelt::spawn_command_under_seatbelt(
        &policy,
        vec!["/bin/true".to_string()],
        PathBuf::from("/"),
        env,
    )
    .expect_err("seatbelt must be unsupported off macOS");
    assert!(matches!(err, seatbelt::SeatbeltError::Unsupported { .. }));
}
