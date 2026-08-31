#![allow(unsafe_code)]

use std::fs;
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use tokio::net::windows::named_pipe::NamedPipeClient;
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_HANDLE,
    SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS,
};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{
    IsProcessorFeaturePresent, OpenProcess, OpenProcessToken, PF_VIRT_FIRMWARE_ENABLED,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{PWSTR, w};

const LOCAL_SYSTEM_SID: &str = "S-1-5-18";

pub(crate) fn current_os_version() -> Result<String> {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: u32::try_from(std::mem::size_of::<OSVERSIONINFOW>())?,
        ..Default::default()
    };
    let status = unsafe { RtlGetVersion(&mut version) };
    ensure!(status.0 >= 0, "Windows version is unavailable");
    Ok(format!(
        "{}.{}.{}.0",
        version.dwMajorVersion, version.dwMinorVersion, version.dwBuildNumber
    ))
}

pub(crate) fn hyper_v_available() -> bool {
    hyper_v_available_inner().unwrap_or(false)
}

fn hyper_v_available_inner() -> Result<bool> {
    if !unsafe { IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED) }.as_bool() {
        return Ok(false);
    }
    let manager = OwnedServiceHandle(unsafe {
        OpenSCManagerW(None, None, SC_MANAGER_CONNECT).context("open Windows service manager")?
    });
    let service = OwnedServiceHandle(unsafe {
        OpenServiceW(manager.0, w!("vmcompute"), SERVICE_QUERY_STATUS)
            .context("open Host Compute Service")?
    });
    let mut status = SERVICE_STATUS::default();
    unsafe { QueryServiceStatus(service.0, &mut status) }.context("query Host Compute Service")?;
    Ok(status.dwCurrentState == SERVICE_RUNNING)
}

pub(crate) fn authenticate_runtime_pipe_server(stream: &NamedPipeClient) -> Result<()> {
    let pipe = HANDLE(stream.as_raw_handle());
    let mut process_id = 0_u32;
    unsafe { GetNamedPipeServerProcessId(pipe, &mut process_id) }
        .context("identify runtime pipe server")?;
    ensure!(process_id != 0, "runtime pipe server process ID is invalid");

    let process = OwnedHandle(unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id)
            .context("open runtime pipe server process")?
    });
    ensure!(
        process_sid(process.0)? == LOCAL_SYSTEM_SID,
        "runtime pipe server is not LocalSystem"
    );

    let actual_path = process_image_path(process.0)?;
    let actual = fs::canonicalize(&actual_path).with_context(|| {
        format!(
            "resolve runtime pipe server executable {}",
            actual_path.display()
        )
    })?;
    let expected = trusted_runtime_service_path()?;
    ensure!(
        same_windows_path(&actual, &expected),
        "runtime pipe server executable is not the installed Reporch service"
    );
    Ok(())
}

fn trusted_runtime_service_path() -> Result<PathBuf> {
    let program_files = std::env::var_os("ProgramFiles").context("ProgramFiles is required")?;
    let path = PathBuf::from(program_files)
        .join("Reporch")
        .join("bin")
        .join("reporch-runtime-service.exe");
    ensure!(path.is_absolute(), "runtime service path is not absolute");
    fs::canonicalize(&path)
        .with_context(|| format!("resolve installed runtime service {}", path.display()))
}

fn process_image_path(process: HANDLE) -> Result<PathBuf> {
    let mut buffer = vec![0_u16; 32_768];
    let mut length = u32::try_from(buffer.len())?;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .context("read runtime pipe server executable")?;
    ensure!(
        length > 0 && usize::try_from(length)? < buffer.len(),
        "runtime pipe server executable path is invalid"
    );
    buffer.truncate(usize::try_from(length)?);
    Ok(PathBuf::from(String::from_utf16(&buffer)?))
}

fn process_sid(process: HANDLE) -> Result<String> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }
        .context("open runtime pipe server token")?;
    let token = OwnedHandle(token);
    let mut required = 0_u32;
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut required) };
    ensure!(
        (std::mem::size_of::<TOKEN_USER>() as u32..=64 * 1024).contains(&required),
        "runtime pipe server token has an invalid size"
    );
    let words = required.div_ceil(std::mem::size_of::<usize>() as u32) as usize;
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
    .context("read runtime pipe server token")?;
    let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = PWSTR::null();
    unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid) }
        .context("format runtime pipe server SID")?;
    let sid = LocalString(sid);
    Ok(unsafe { sid.0.to_string() }?)
}

fn same_windows_path(left: &Path, right: &Path) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

struct OwnedServiceHandle(SC_HANDLE);

impl Drop for OwnedServiceHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseServiceHandle(self.0) };
        }
    }
}

struct LocalString(PWSTR);

impl Drop for LocalString {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0.cast())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_path_comparison_is_case_insensitive_but_exact() {
        assert!(same_windows_path(
            Path::new(r"C:\Program Files\Reporch\bin\reporch-runtime-service.exe"),
            Path::new(r"c:\program files\reporch\BIN\REPORCH-RUNTIME-SERVICE.EXE"),
        ));
        assert!(!same_windows_path(
            Path::new(r"C:\Temp\reporch-runtime-service.exe"),
            Path::new(r"C:\Program Files\Reporch\bin\reporch-runtime-service.exe"),
        ));
    }
}
