#![allow(clippy::missing_safety_doc)]

use rodbus::client::channel::Channel;
use rodbus::client::session::{CallbackSession, SyncSession};
use rodbus::error::ErrorKind;
use rodbus::types::{AddressRange, UnitId, WriteMultiple};
use std::ffi::CStr;
use std::net::SocketAddr;
use std::os::raw::c_void;
use std::ptr::{null, null_mut};
use std::str::FromStr;
use tokio::runtime;

// asynchronous API
pub mod asynchronous;
// synchronous API
pub mod synchronous;

/// Status returned during synchronous and asynchronous API calls
#[repr(u8)]
pub enum Status {
    /// The operation was successful and any return value may be used
    Ok,
    /// The channel was shutdown before the operation could complete
    Shutdown,
    /// No connection could be made to the server
    NoConnection,
    /// No valid response was received before the timeout
    ResponseTimeout,
    /// The request was invalid
    BadRequest,
    /// The response was improperly formatted
    BadResponse,
    /// An I/O error occurred on the underlying stream while performing the request
    IOError,
    /// A framing error was detected while performing the request
    BadFraming,
    /// The server returned an exception code (see separate exception value)
    Exception,
    /// An unspecified internal error occurred while performing the request
    InternalError,
}

#[repr(C)]
pub struct Result {
    pub status: Status,
    pub exception: u8,
}

impl Result {
    fn exception(exception: u8) -> Self {
        Self {
            status: Status::Exception,
            exception,
        }
    }

    fn status(status: Status) -> Self {
        Self {
            status,
            exception: 0,
        }
    }

    fn ok() -> Self {
        Self {
            status: Status::Ok,
            exception: 0,
        }
    }
}

impl std::convert::From<&ErrorKind> for Result {
    fn from(err: &ErrorKind) -> Self {
        match err {
            ErrorKind::Bug(_) => Result::status(Status::InternalError),
            ErrorKind::NoConnection => Result::status(Status::NoConnection),
            ErrorKind::BadFrame(_) => Result::status(Status::BadFraming),
            ErrorKind::Shutdown => Result::status(Status::Shutdown),
            ErrorKind::ResponseTimeout => Result::status(Status::ResponseTimeout),
            ErrorKind::BadRequest(_) => Result::status(Status::BadRequest),
            ErrorKind::Exception(ex) => Result::exception(ex.to_u8()),
            ErrorKind::Io(_) => Result::status(Status::IOError),
            ErrorKind::BadResponse(_) => Result::status(Status::BadResponse),
            _ => Result::status(Status::InternalError),
        }
    }
}

impl<T> std::convert::From<std::result::Result<T, rodbus::error::Error>> for Result {
    fn from(result: std::result::Result<T, rodbus::error::Error>) -> Self {
        match result {
            Ok(_) => Result::ok(),
            Err(e) => e.kind().into(),
        }
    }
}

struct ContextStorage {
    context: *mut c_void,
}

#[repr(C)]
pub struct Session {
    runtime: *mut tokio::runtime::Runtime,
    channel: *mut rodbus::client::channel::Channel,
    unit_id: u8,
    timeout_ms: u32,
}

// we need these so we can send the callback context to the executor
// we rely on the C program to keep the context value alive
// for the duration of the operation, and for it to be thread-safe
unsafe impl Send for ContextStorage {}
unsafe impl Sync for ContextStorage {}

/// @brief create an instance of the multi-threaded work-stealing Tokio runtime
///
/// This instance is typically created at the beginning of your program and destroyed
/// using destroy_runtime() before your program exits.
///
/// @return An instance of the runtime or NULL if it cannot be created for some reason
#[no_mangle]
pub extern "C" fn create_multithreaded_runtime() -> *mut tokio::runtime::Runtime {
    match runtime::Builder::new()
        .enable_all()
        .threaded_scheduler()
        .build()
    {
        Ok(r) => Box::into_raw(Box::new(r)),
        Err(_) => null_mut(),
    }
}

/// @brief create an instance of the basic (single-threaded) Tokio runtime
///
/// This instance is typically created at the beginning of your program and destroyed
/// using destroy_runtime() before your program exits.
///
/// @return An instance of the runtime or NULL if it cannot be created for some reason
#[no_mangle]
pub extern "C" fn create_basic_runtime() -> *mut tokio::runtime::Runtime {
    match runtime::Builder::new()
        .enable_all()
        .basic_scheduler()
        .build()
    {
        Ok(r) => Box::into_raw(Box::new(r)),
        Err(_) => null_mut(),
    }
}

/// @brief Destroy a previously created runtime instance
///
/// This operation is typically performed just before program exit. It blocks until
/// the runtime stops and all operations are canceled. Any pending asynchronous callbacks
/// may not complete, and synchronous operations performed on other threads will fail
/// with a #Status value of #Status_Shutdown
///
#[no_mangle]
pub unsafe extern "C" fn destroy_runtime(runtime: *mut tokio::runtime::Runtime) {
    if !runtime.is_null() {
        Box::from_raw(runtime);
    };
}

/// @brief Convience function to build a session struct
///
/// This function does not allocate and is merely provided to convienently create the #Session struct.
///
/// @param runtime       pointer to the #Runtime that will be used to make requests on the channel
/// @param channel       pointer to the #Channel on which requests associated with the built #Session will be made
/// @param unit_id       Modbus unit identifier of the server
/// @param timeout_ms    timeout in milliseconds for any requests made via this session object
/// @return              built Session struct ready for use with the Modbus request functions
#[no_mangle]
pub extern "C" fn build_session(
    runtime: *mut tokio::runtime::Runtime,
    channel: *mut Channel,
    unit_id: u8,
    timeout_ms: u32,
) -> Session {
    Session {
        runtime,
        channel,
        unit_id,
        timeout_ms,
    }
}

#[no_mangle]
pub unsafe extern "C" fn create_tcp_client(
    runtime: *mut tokio::runtime::Runtime,
    address: *const std::os::raw::c_char,
    max_queued_requests: usize,
) -> *mut rodbus::client::channel::Channel {
    let rt = runtime.as_mut().unwrap();

    // if we can't turn the c-string into SocketAddr, return null
    let addr = {
        match CStr::from_ptr(address).to_str() {
            // TODO - consider logging?
            Err(_) => return null_mut(),
            Ok(s) => match SocketAddr::from_str(s) {
                // TODO - consider logging?
                Err(_) => return null_mut(),
                Ok(addr) => addr,
            },
        }
    };

    let (handle, task) = rodbus::client::channel::Channel::create_handle_and_task(
        addr,
        max_queued_requests,
        rodbus::client::channel::strategy::default(),
    );

    rt.spawn(task);

    Box::into_raw(Box::new(handle))
}

#[no_mangle]
pub unsafe extern "C" fn destroy_tcp_client(client: *mut rodbus::client::channel::Channel) {
    if !client.is_null() {
        Box::from_raw(client);
    };
}

pub(crate) unsafe fn to_write_multiple<T>(
    start: u16,
    values: *const T,
    count: u16,
) -> WriteMultiple<T>
where
    T: Copy,
{
    let mut vec = Vec::with_capacity(count as usize);
    for i in 0..count {
        vec.push(*values.add(i as usize));
    }
    WriteMultiple::new(start, vec)
}
