//! A per-command AppContainer identity.
//!
//! Decisions:
//! - Every contained command gets its own AppContainer name. MXC defaults to
//!   one shared name, and on its DACL tier one run's permission restore then
//!   strips the access of another run still in flight.
//! - PowerShell resolves its location by reading the metadata of every parent
//!   of the working directory, which an AppContainer is denied under the user
//!   profile. On hosts where MXC grants access through host DACLs, the
//!   identity gets read-attributes, not inherited, on each parent of the
//!   workspace for the command's lifetime. `SetFileSecurityW` writes only that
//!   folder's descriptor, so nothing propagates to the folder's contents.
//! - The profile MXC creates for the name is deleted when the command ends.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ACCESS_MODE, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
    SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, FreeSid, GetFileSecurityW, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, InitializeSecurityDescriptor, NO_INHERITANCE, PSECURITY_DESCRIPTOR,
    PSID, SE_DACL_AUTO_INHERITED, SE_DACL_PROTECTED, SECURITY_DESCRIPTOR, SetFileSecurityW,
    SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
};

const FILE_READ_ATTRIBUTES: u32 = 0x80;
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

/// An AppContainer name, its SID, and the parent folders it was granted.
pub(crate) struct ContainerIdentity {
    name: String,
    sid: PSID,
    granted: Vec<PathBuf>,
}

// SAFETY: `sid` is owned by this value, freed once in `Drop`, and only read.
unsafe impl Send for ContainerIdentity {}
unsafe impl Sync for ContainerIdentity {}

impl ContainerIdentity {
    pub(crate) fn new(name: String) -> std::io::Result<Self> {
        let wide_name = wide(OsStr::new(&name));
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: `wide_name` is NUL-terminated and outlives the call.
        let result =
            unsafe { DeriveAppContainerSidFromAppContainerName(wide_name.as_ptr(), &mut sid) };
        if result < 0 || sid.is_null() {
            return Err(std::io::Error::other(format!(
                "derive the AppContainer SID for {name}: HRESULT {result:#x}"
            )));
        }
        Ok(Self {
            name,
            sid,
            granted: Vec::new(),
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Grant read-attributes on each parent of `dir` below the drive root.
    /// Best effort: a folder the user cannot re-permission is skipped.
    pub(crate) fn grant_parents(&mut self, dir: &Path) {
        for parent in dir.ancestors().skip(1) {
            if parent.parent().is_none() {
                break;
            }
            if set_read_attributes(parent, self.sid, GRANT_ACCESS).is_ok() {
                self.granted.push(parent.to_path_buf());
            }
        }
    }
}

impl Drop for ContainerIdentity {
    fn drop(&mut self) {
        for parent in self.granted.drain(..) {
            let _ = set_read_attributes(&parent, self.sid, REVOKE_ACCESS);
        }
        let wide_name = wide(OsStr::new(&self.name));
        // SAFETY: the SID came from DeriveAppContainerSidFromAppContainerName,
        // which documents FreeSid; the name is NUL-terminated.
        unsafe {
            FreeSid(self.sid);
            DeleteAppContainerProfile(wide_name.as_ptr());
        }
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Add or remove one explicit, non-inherited read-attributes ACE for `sid`.
fn set_read_attributes(path: &Path, sid: PSID, mode: ACCESS_MODE) -> std::io::Result<()> {
    let name = wide(path.as_os_str());
    let mut needed = 0u32;
    // SAFETY: a size probe with a null buffer, as documented.
    unsafe {
        GetFileSecurityW(
            name.as_ptr(),
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // u64 storage keeps the descriptor suitably aligned.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    let descriptor = buffer.as_mut_ptr() as PSECURITY_DESCRIPTOR;
    // SAFETY: `buffer` holds `needed` bytes.
    if unsafe {
        GetFileSecurityW(
            name.as_ptr(),
            DACL_SECURITY_INFORMATION,
            descriptor,
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: `descriptor` is the self-relative descriptor read above.
    unsafe {
        if GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) == 0
            || GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_READ_ATTRIBUTES,
        grfAccessMode: mode,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid as *mut u16,
        },
    };
    let mut updated: *mut ACL = std::ptr::null_mut();
    let old = if present != 0 {
        dacl
    } else {
        std::ptr::null_mut()
    };
    // SAFETY: `access` and `old` are valid for the call; `updated` is freed
    // with LocalFree below.
    let status = unsafe { SetEntriesInAclW(1, &access, old, &mut updated) };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }
    let mut absolute = SECURITY_DESCRIPTOR::default();
    let target = &mut absolute as *mut SECURITY_DESCRIPTOR as PSECURITY_DESCRIPTOR;
    let inherit_bits = SE_DACL_AUTO_INHERITED | SE_DACL_PROTECTED;
    // SAFETY: `absolute` lives across the calls; `updated` is a valid ACL.
    let written = unsafe {
        InitializeSecurityDescriptor(target, SECURITY_DESCRIPTOR_REVISION) != 0
            && SetSecurityDescriptorDacl(target, 1, updated, 0) != 0
            && SetSecurityDescriptorControl(target, inherit_bits, control & inherit_bits) != 0
            && SetFileSecurityW(name.as_ptr(), DACL_SECURITY_INFORMATION, target) != 0
    };
    let error = std::io::Error::last_os_error();
    // SAFETY: allocated by SetEntriesInAclW.
    unsafe { LocalFree(updated as _) };
    if written { Ok(()) } else { Err(error) }
}
