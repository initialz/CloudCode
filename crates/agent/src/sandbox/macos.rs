//! macOS Seatbelt binding.
//!
//! `sandbox_init_with_parameters` is the parameterised variant of the
//! Seatbelt entry point. It ships in `libSystem.B.dylib` and has been
//! stable enough that Chromium, Firefox, and Electron all link against
//! it the same way. The C signature (from public uses in those projects)
//! is:
//!
//! ```c
//! int sandbox_init_with_parameters(
//!     const char *profile,
//!     uint64_t flags,
//!     const char *const parameters[],   // NULL-terminated K, V, K, V, ...
//!     char **errorbuf);
//! void sandbox_free_error(char *errorbuf);
//! ```

use crate::sandbox::{SandboxMode, SandboxParams};
use anyhow::{anyhow, Result};
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::ptr;

const PROFILE_STRICT: &str = include_str!("profile.sb");
const PROFILE_PERMISSIVE: &str = include_str!("profile_permissive.sb");

extern "C" {
    fn sandbox_init_with_parameters(
        profile: *const libc::c_char,
        flags: u64,
        parameters: *const *const libc::c_char,
        errorbuf: *mut *mut libc::c_char,
    ) -> libc::c_int;

    fn sandbox_free_error(errorbuf: *mut libc::c_char);
}

pub fn apply(params: &SandboxParams) -> Result<()> {
    let profile_src = match params.mode {
        SandboxMode::Strict => PROFILE_STRICT,
        SandboxMode::Permissive => PROFILE_PERMISSIVE,
        SandboxMode::Off => {
            // Off should be handled by the caller skipping the entire
            // sandbox-exec wrapper — we should never get here. If we do,
            // applying nothing is the safest fallback.
            return Ok(());
        }
    };
    let profile = CString::new(profile_src).map_err(|_| anyhow!("sandbox profile contains NUL"))?;

    // SBPL doesn't canonicalize subpath arguments — `/tmp/foo` won't match
    // accesses the kernel reports as `/private/tmp/foo`. Resolve symlinks
    // up front so the profile rules apply to the real path the kernel
    // actually sees on every syscall.
    let workspace_path =
        std::fs::canonicalize(&params.workspace).unwrap_or_else(|_| params.workspace.clone());
    let workspace_root_path = std::fs::canonicalize(&params.workspace_root)
        .unwrap_or_else(|_| params.workspace_root.clone());
    let home_path = std::fs::canonicalize(&params.home).unwrap_or_else(|_| params.home.clone());

    // claude maintains a per-project subdir under ~/.claude/projects/
    // keyed off the absolute cwd, with every '/' replaced by '-'. We
    // mirror that encoding so the strict profile can carve out *this*
    // project's subtree while denying every other one. Permissive
    // mode ignores this param; we still pass it so both profiles
    // accept the same parameter list.
    let project_dir_name = workspace_path.to_string_lossy().replace('/', "-");

    // OWN_ACCOUNT_ROOT = <WORKSPACE_ROOT>/<this account>. Used by the
    // permissive profile to keep cross-account isolation when the
    // strict toggle is off.
    let own_account_root = workspace_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| workspace_path.clone());

    // Parameter pairs: (key, value), NULL-terminated.
    let workspace = CString::new(workspace_path.as_os_str().as_bytes())
        .map_err(|_| anyhow!("workspace path contains NUL"))?;
    let workspace_root = CString::new(workspace_root_path.as_os_str().as_bytes())
        .map_err(|_| anyhow!("workspace_root path contains NUL"))?;
    let home = CString::new(home_path.as_os_str().as_bytes())
        .map_err(|_| anyhow!("home path contains NUL"))?;
    let claude_project_dir = CString::new(project_dir_name.as_bytes())
        .map_err(|_| anyhow!("claude project dir name contains NUL"))?;
    let own_account = CString::new(own_account_root.as_os_str().as_bytes())
        .map_err(|_| anyhow!("own account root path contains NUL"))?;
    let key_ws = CString::new("WORKSPACE").unwrap();
    let key_ws_root = CString::new("WORKSPACE_ROOT").unwrap();
    let key_home = CString::new("HOME_DIR").unwrap();
    let key_claude_project_dir = CString::new("CLAUDE_PROJECT_DIR").unwrap();
    let key_own_account = CString::new("OWN_ACCOUNT_ROOT").unwrap();

    let raw: Vec<*const libc::c_char> = vec![
        key_ws.as_ptr(),
        workspace.as_ptr(),
        key_ws_root.as_ptr(),
        workspace_root.as_ptr(),
        key_home.as_ptr(),
        home.as_ptr(),
        key_claude_project_dir.as_ptr(),
        claude_project_dir.as_ptr(),
        key_own_account.as_ptr(),
        own_account.as_ptr(),
        ptr::null(),
    ];

    let mut errbuf: *mut libc::c_char = ptr::null_mut();
    let rc = unsafe {
        sandbox_init_with_parameters(profile.as_ptr(), 0, raw.as_ptr(), &mut errbuf as *mut _)
    };

    if rc != 0 {
        let msg = if errbuf.is_null() {
            "sandbox_init_with_parameters returned a non-zero status with no error buffer".into()
        } else {
            let s = unsafe { CStr::from_ptr(errbuf) }
                .to_string_lossy()
                .into_owned();
            unsafe { sandbox_free_error(errbuf) };
            s
        };
        return Err(anyhow!("apply Seatbelt: {}", msg));
    }
    Ok(())
}
