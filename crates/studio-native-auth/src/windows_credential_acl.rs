use std::ffi::c_void;
use std::fs;
use std::mem::{size_of, size_of_val};
use std::os::windows::fs::MetadataExt as _;
use std::path::Path;
use std::ptr;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
    EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl, GetTokenInformation,
    INHERITED_ACE, IsValidSid, OBJECT_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING, REPLACEFILE_WRITE_THROUGH, ReplaceFileW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{HSTRING, PCWSTR, PWSTR};

const ACCESS_ALLOWED_ACE_TYPE_VALUE: u8 = 0;

pub(super) fn secure_path(path: &Path) -> Result<(), String> {
    let metadata = secure_metadata(path)?;
    let current_sid = CurrentUserSid::query()?;
    let sid = current_sid.to_string()?;
    let inheritance = if metadata.is_dir() { "OICI" } else { "" };
    let descriptor =
        SecurityDescriptor::from_sddl(&format!("O:{sid}D:P(A;{inheritance};FA;;;{sid})"))?;
    let information = OWNER_SECURITY_INFORMATION
        | DACL_SECURITY_INFORMATION
        | PROTECTED_DACL_SECURITY_INFORMATION;
    unsafe {
        SetFileSecurity::apply(path, information, descriptor.0)?;
    }
    verify_path(path)
}

pub(super) fn verify_path(path: &Path) -> Result<(), String> {
    let metadata = secure_metadata(path)?;
    let current_sid = CurrentUserSid::query()?;
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let code = unsafe {
        GetNamedSecurityInfoW(
            &HSTRING::from(path.as_os_str()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        )
    };
    if code.0 != 0 || descriptor.is_invalid() || owner.0.is_null() || dacl.is_null() {
        return Err(format!(
            "read credential ACL failed with Win32 status {}",
            code.0
        ));
    }
    let descriptor = SecurityDescriptor(descriptor);
    unsafe { EqualSid(owner, current_sid.as_psid()) }
        .map_err(|error| format!("credential owner mismatch: {error}"))?;

    let mut control = 0_u16;
    let mut revision = 0_u32;
    unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) }
        .map_err(|error| format!("read credential ACL control failed: {error}"))?;
    if control & SE_DACL_PROTECTED.0 == 0 {
        return Err("credential DACL inherits permissions".into());
    }

    let mut information = ACL_SIZE_INFORMATION::default();
    unsafe {
        GetAclInformation(
            dacl,
            ptr::addr_of_mut!(information).cast(),
            size_of_val(&information) as u32,
            AclSizeInformation,
        )
    }
    .map_err(|error| format!("inspect credential ACL failed: {error}"))?;
    if information.AceCount != 1 {
        return Err("credential DACL must contain exactly one access rule".into());
    }

    let mut raw_ace: *mut c_void = ptr::null_mut();
    unsafe { GetAce(dacl, 0, &mut raw_ace) }
        .map_err(|error| format!("read credential access rule failed: {error}"))?;
    if raw_ace.is_null() {
        return Err("credential access rule is missing".into());
    }
    let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
    if ace.Header.AceType != ACCESS_ALLOWED_ACE_TYPE_VALUE
        || usize::from(ace.Header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
        || u32::from(ace.Header.AceFlags) & INHERITED_ACE.0 != 0
        || ace.Mask != FILE_ALL_ACCESS.0
    {
        return Err("credential access rule is not an explicit full-control allow".into());
    }
    let ace_sid = PSID(ptr::addr_of!(ace.SidStart).cast_mut().cast());
    unsafe { EqualSid(ace_sid, current_sid.as_psid()) }
        .map_err(|error| format!("credential access rule SID mismatch: {error}"))?;

    let expected_inheritance = if metadata.is_dir() { 0x03 } else { 0x00 };
    if ace.Header.AceFlags & 0x03 != expected_inheritance {
        return Err("credential access rule inheritance is invalid".into());
    }
    Ok(())
}

pub(super) fn atomic_replace(source: &Path, destination: &Path) -> Result<(), String> {
    verify_path(source)?;
    if destination.exists() {
        verify_path(destination)?;
        unsafe {
            ReplaceFileW(
                &HSTRING::from(destination.as_os_str()),
                &HSTRING::from(source.as_os_str()),
                PCWSTR::null(),
                REPLACEFILE_WRITE_THROUGH,
                None,
                None,
            )
        }
        .map_err(|error| format!("atomically replace credential file failed: {error}"))?;
    } else {
        unsafe {
            MoveFileExW(
                &HSTRING::from(source.as_os_str()),
                &HSTRING::from(destination.as_os_str()),
                MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|error| format!("atomically install credential file failed: {error}"))?;
    }
    verify_path(destination)
}

fn secure_metadata(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    let handle = unsafe {
        CreateFileW(
            &HSTRING::from(path.as_os_str()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(|error| format!("open credential path metadata failed: {error}"))?;
    let handle = OwnedHandle(handle);
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle.0, &mut information) }
        .map_err(|error| format!("read credential path metadata failed: {error}"))?;
    if (!metadata.is_file() && !metadata.is_dir())
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || (metadata.is_file() && information.nNumberOfLinks != 1)
    {
        return Err(
            "credential path is not a single-link regular non-reparse file or directory".into(),
        );
    }
    Ok(metadata)
}

struct CurrentUserSid {
    buffer: Vec<usize>,
}

impl CurrentUserSid {
    fn query() -> Result<Self, String> {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .map_err(|error| format!("open current process token failed: {error}"))?;
        let token = OwnedHandle(token);
        let mut required = 0_u32;
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut required) };
        if !(size_of::<TOKEN_USER>() as u32..=64 * 1024).contains(&required) {
            return Err("current process token returned an invalid SID size".into());
        }
        let words = required.div_ceil(size_of::<usize>() as u32) as usize;
        let mut buffer = vec![0_usize; words];
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                required,
                &mut required,
            )
        }
        .map_err(|error| format!("read current process SID failed: {error}"))?;
        let value = Self { buffer };
        if !unsafe { IsValidSid(value.as_psid()) }.as_bool() {
            return Err("current process SID is invalid".into());
        }
        Ok(value)
    }

    fn as_psid(&self) -> PSID {
        let token_user = unsafe { &*self.buffer.as_ptr().cast::<TOKEN_USER>() };
        token_user.User.Sid
    }

    fn to_string(&self) -> Result<String, String> {
        let mut text = PWSTR::null();
        unsafe { ConvertSidToStringSidW(self.as_psid(), &mut text) }
            .map_err(|error| format!("format current process SID failed: {error}"))?;
        let text = LocalString(text);
        unsafe { text.0.to_string() }.map_err(|error| error.to_string())
    }
}

struct SetFileSecurity;

impl SetFileSecurity {
    unsafe fn apply(
        path: &Path,
        information: OBJECT_SECURITY_INFORMATION,
        descriptor: PSECURITY_DESCRIPTOR,
    ) -> Result<(), String> {
        unsafe {
            windows::Win32::Security::SetFileSecurityW(
                &HSTRING::from(path.as_os_str()),
                information,
                descriptor,
            )
        }
        .ok()
        .map_err(|error| format!("restrict credential ACL failed: {error}"))
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn from_sddl(value: &str) -> Result<Self, String> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(value),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|error| format!("parse credential security descriptor failed: {error}"))?;
        if descriptor.is_invalid() {
            return Err("credential security descriptor is invalid".into());
        }
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

struct LocalString(PWSTR);

impl Drop for LocalString {
    fn drop(&mut self) {
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0.cast()))) };
    }
}

#[cfg(test)]
mod tests {
    use super::{atomic_replace, secure_path, verify_path};
    use std::fs;

    #[test]
    fn native_acl_round_trip_rejects_hardlinks_and_preserves_atomic_replacement() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("private");
        fs::create_dir(&directory).unwrap();
        secure_path(&directory).unwrap();

        let credential = directory.join("credentials-v2.json");
        fs::write(&credential, b"old").unwrap();
        secure_path(&credential).unwrap();
        verify_path(&credential).unwrap();

        let hardlink = directory.join("credential-copy");
        fs::hard_link(&credential, &hardlink).unwrap();
        assert!(verify_path(&credential).is_err());
        fs::remove_file(&hardlink).unwrap();
        verify_path(&credential).unwrap();

        let replacement = directory.join("replacement.tmp");
        fs::write(&replacement, b"new").unwrap();
        secure_path(&replacement).unwrap();
        atomic_replace(&replacement, &credential).unwrap();
        assert_eq!(fs::read(&credential).unwrap(), b"new");
        verify_path(&credential).unwrap();
    }
}
