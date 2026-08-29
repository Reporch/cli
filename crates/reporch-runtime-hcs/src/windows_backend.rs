#![allow(unsafe_code)]

use std::io::{self, Read, Write};
use std::mem::size_of;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Networking::WinSock::{
    ADDRESS_FAMILY, AF_HYPERV, FIONBIO, POLLWRNORM, SEND_RECV_FLAGS, SO_ERROR, SO_RCVTIMEO,
    SO_SNDTIMEO, SOCK_STREAM, SOCKADDR, SOCKET, SOCKET_ERROR, SOL_SOCKET, WSADATA, WSAEWOULDBLOCK,
    WSAGetLastError, WSAPOLLFD, WSAPoll, WSASocketW, WSAStartup, closesocket, connect, getsockopt,
    ioctlsocket, recv, send, setsockopt,
};
use windows::Win32::System::HostComputeSystem::{
    HCS_OPERATION, HCS_SYSTEM, HcsCancelOperation, HcsCloseComputeSystem, HcsCloseOperation,
    HcsCreateComputeSystem, HcsCreateOperation, HcsGetComputeSystemProperties,
    HcsStartComputeSystem, HcsTerminateComputeSystem, HcsWaitForOperationResult,
};
use windows::Win32::System::Hypervisor::{HV_PROTOCOL_RAW, SOCKADDR_HV};
use windows::core::{GUID, HSTRING, PSTR, PWSTR};

use super::{HcsVmConfigV1, hyperv_vsock_service_id};

const HCS_OPERATION_TIMEOUT_MS: u32 = 15_000;
const HCS_TERMINATE_TIMEOUT_MS: u32 = 5_000;
const HVSOCK_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct HcsVirtualMachine {
    system: HCS_SYSTEM,
    id: GUID,
    service_id: GUID,
    terminated: bool,
}

impl HcsVirtualMachine {
    pub fn create(config: &HcsVmConfigV1) -> Result<Self> {
        config.validate()?;
        let configuration = HSTRING::from(config.configuration_json()?);
        let id_text = HSTRING::from(config.id.hyphenated().to_string());
        let operation = Operation::new()?;
        let system = unsafe {
            HcsCreateComputeSystem(&id_text, &configuration, operation.0, None)
                .context("HcsCreateComputeSystem")?
        };
        if let Err(error) = operation.wait(HCS_OPERATION_TIMEOUT_MS) {
            unsafe { HcsCloseComputeSystem(system) };
            return Err(error).context("create HCS virtual machine");
        }
        let operation = Operation::new()?;
        unsafe { HcsStartComputeSystem(system, operation.0, &HSTRING::new()) }
            .context("HcsStartComputeSystem")?;
        if let Err(error) = operation.wait(HCS_OPERATION_TIMEOUT_MS) {
            let mut vm = Self {
                system,
                id: GUID::from_u128(config.id.as_u128()),
                service_id: service_guid(config.vsock_port),
                terminated: false,
            };
            let _ = vm.terminate();
            return Err(error).context("start HCS virtual machine");
        }
        let operation = Operation::new()?;
        unsafe {
            HcsGetComputeSystemProperties(
                system,
                operation.0,
                &HSTRING::from(r#"{"PropertyTypes":["RuntimeId"]}"#),
            )
        }
        .context("HcsGetComputeSystemProperties")?;
        let properties = operation
            .wait_document(HCS_OPERATION_TIMEOUT_MS)?
            .context("HCS omitted the virtual machine RuntimeId")?;
        let runtime_id = parse_runtime_id(&properties)?;
        Ok(Self {
            system,
            id: runtime_id,
            service_id: service_guid(config.vsock_port),
            terminated: false,
        })
    }

    pub fn connect(&self, io_timeout: Duration) -> Result<HvSocketStream> {
        ensure!(!self.terminated, "HCS virtual machine is terminated");
        HvSocketStream::connect(self.id, self.service_id, HVSOCK_CONNECT_TIMEOUT, io_timeout)
    }

    pub fn terminate(&mut self) -> Result<()> {
        if self.terminated {
            return Ok(());
        }
        self.terminated = true;
        let operation = Operation::new()?;
        unsafe { HcsTerminateComputeSystem(self.system, operation.0, &HSTRING::new()) }
            .context("HcsTerminateComputeSystem")?;
        operation
            .wait(HCS_TERMINATE_TIMEOUT_MS)
            .context("terminate HCS virtual machine")
    }
}

impl Drop for HcsVirtualMachine {
    fn drop(&mut self) {
        if !self.terminated {
            let _ = self.terminate();
        }
        unsafe { HcsCloseComputeSystem(self.system) };
    }
}

struct Operation(HCS_OPERATION);

impl Operation {
    fn new() -> Result<Self> {
        let handle = unsafe { HcsCreateOperation(None, None) };
        ensure!(
            !handle.is_invalid(),
            "HcsCreateOperation returned an invalid handle"
        );
        Ok(Self(handle))
    }

    fn wait(&self, timeout_ms: u32) -> Result<()> {
        match unsafe { HcsWaitForOperationResult(self.0, timeout_ms, None) } {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = unsafe { HcsCancelOperation(self.0) };
                Err(error.into())
            }
        }
    }

    fn wait_document(&self, timeout_ms: u32) -> Result<Option<String>> {
        let mut document = PWSTR::null();
        match unsafe { HcsWaitForOperationResult(self.0, timeout_ms, Some(&mut document)) } {
            Ok(()) if document.is_null() => Ok(None),
            Ok(()) => {
                let guard = LocalString(document);
                Ok(Some(unsafe { guard.0.to_string() }?))
            }
            Err(error) => {
                let _ = unsafe { HcsCancelOperation(self.0) };
                Err(error.into())
            }
        }
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        unsafe { HcsCloseOperation(self.0) };
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

pub struct HvSocketStream {
    socket: SOCKET,
}

impl HvSocketStream {
    fn connect(
        vm_id: GUID,
        service_id: GUID,
        timeout: Duration,
        io_timeout: Duration,
    ) -> Result<Self> {
        initialize_winsock()?;
        let io_timeout_ms = u32::try_from(io_timeout.as_millis())?;
        ensure!(io_timeout_ms > 0, "Hyper-V socket I/O timeout is zero");
        let deadline = Instant::now() + timeout;
        let mut last_error = None;
        while Instant::now() < deadline {
            let socket = unsafe {
                WSASocketW(
                    i32::from(AF_HYPERV),
                    SOCK_STREAM.0,
                    HV_PROTOCOL_RAW as i32,
                    None,
                    0,
                    0,
                )
            }
            .context("create Hyper-V socket")?;
            set_timeout(socket, SO_RCVTIMEO, io_timeout_ms)?;
            set_timeout(socket, SO_SNDTIMEO, io_timeout_ms)?;
            let mut nonblocking = 1_u32;
            ensure!(
                unsafe { ioctlsocket(socket, FIONBIO, &mut nonblocking) } != SOCKET_ERROR,
                "enable nonblocking Hyper-V connect failed with Winsock error {}",
                unsafe { WSAGetLastError() }.0
            );
            let address = SOCKADDR_HV {
                Family: ADDRESS_FAMILY(AF_HYPERV),
                Reserved: 0,
                VmId: vm_id,
                ServiceId: service_id,
            };
            let result = unsafe {
                connect(
                    socket,
                    (&raw const address).cast::<SOCKADDR>(),
                    i32::try_from(size_of::<SOCKADDR_HV>())?,
                )
            };
            if result != SOCKET_ERROR {
                set_blocking(socket)?;
                return Ok(Self { socket });
            }
            let connect_error = unsafe { WSAGetLastError() };
            if connect_error == WSAEWOULDBLOCK {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let wait_ms = i32::try_from(remaining.as_millis().min(250))?;
                let mut poll = WSAPOLLFD {
                    fd: socket,
                    events: POLLWRNORM,
                    revents: Default::default(),
                };
                if wait_ms > 0 && unsafe { WSAPoll(&mut poll, 1, wait_ms) } > 0 {
                    let mut socket_error = 0_i32;
                    let mut socket_error_size = i32::try_from(size_of::<i32>())?;
                    let queried = unsafe {
                        getsockopt(
                            socket,
                            SOL_SOCKET,
                            SO_ERROR,
                            PSTR((&raw mut socket_error).cast()),
                            &mut socket_error_size,
                        )
                    };
                    if queried != SOCKET_ERROR && socket_error == 0 {
                        set_blocking(socket)?;
                        return Ok(Self { socket });
                    }
                    last_error = Some(windows::Win32::Networking::WinSock::WSA_ERROR(socket_error));
                } else {
                    last_error = Some(connect_error);
                }
            } else {
                last_error = Some(connect_error);
            }
            unsafe { closesocket(socket) };
            std::thread::sleep(Duration::from_millis(50));
        }
        anyhow::bail!(
            "connect Hyper-V socket timed out (Winsock error {})",
            last_error.map_or(0, |error| error.0)
        )
    }
}

fn set_blocking(socket: SOCKET) -> Result<()> {
    let mut nonblocking = 0_u32;
    ensure!(
        unsafe { ioctlsocket(socket, FIONBIO, &mut nonblocking) } != SOCKET_ERROR,
        "restore blocking Hyper-V socket failed with Winsock error {}",
        unsafe { WSAGetLastError() }.0
    );
    Ok(())
}

impl Read for HvSocketStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let result = unsafe { recv(self.socket, buffer, SEND_RECV_FLAGS(0)) };
        if result == SOCKET_ERROR {
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }.0));
        }
        Ok(result as usize)
    }
}

impl Write for HvSocketStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let result = unsafe { send(self.socket, buffer, SEND_RECV_FLAGS(0)) };
        if result == SOCKET_ERROR {
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }.0));
        }
        Ok(result as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for HvSocketStream {
    fn drop(&mut self) {
        unsafe { closesocket(self.socket) };
    }
}

fn initialize_winsock() -> Result<()> {
    static RESULT: OnceLock<i32> = OnceLock::new();
    let result = *RESULT.get_or_init(|| {
        let mut data = WSADATA::default();
        unsafe { WSAStartup(0x0202, &mut data) }
    });
    ensure!(result == 0, "WSAStartup failed with error {result}");
    Ok(())
}

fn set_timeout(socket: SOCKET, option: i32, milliseconds: u32) -> Result<()> {
    let bytes = milliseconds.to_ne_bytes();
    let result = unsafe { setsockopt(socket, SOL_SOCKET, option, Some(&bytes)) };
    ensure!(
        result != SOCKET_ERROR,
        "setsockopt failed with Winsock error {}",
        unsafe { WSAGetLastError() }.0
    );
    Ok(())
}

fn service_guid(port: u32) -> GUID {
    let text = hyperv_vsock_service_id(port);
    GUID::try_from(text.as_str()).expect("validated Hyper-V service GUID")
}

fn parse_runtime_id(document: &str) -> Result<GUID> {
    let value: serde_json::Value =
        serde_json::from_str(document).context("parse HCS runtime properties document")?;
    let runtime_id = value
        .get("RuntimeId")
        .and_then(serde_json::Value::as_str)
        .context("HCS runtime properties omitted RuntimeId")?;
    GUID::try_from(runtime_id).context("parse HCS RuntimeId")
}
