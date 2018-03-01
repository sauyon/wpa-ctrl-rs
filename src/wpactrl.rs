#![deny(missing_docs)]
use failure::Error;
use std::cell::RefCell;
use std::ffi::CString;
use std::ptr;
use std::path::{Path, PathBuf};
use std::os::unix::ffi::OsStrExt;
use std::sync::Mutex;
use std;

use libc::{c_char, c_int, c_void, size_t};

#[derive(Debug, Fail, PartialEq)]
enum WpaError {
    #[fail(display = "An error occurred")]
    Failure,
    #[fail(display = "Failed to create interface")]
    Interface,
    #[fail(display = "Timed out")]
    Timeout,
    #[fail(display = "Unknown error {}", _0)]
    Unknown(c_int),
}

type Result<T> = ::std::result::Result<T, Error>;

#[link(name = "wpactrl", kind = "static")]
extern "C" {
    fn wpa_ctrl_open2(ctrl_path: *const c_char, cli_pth: *const c_char) -> *mut c_void;
    fn wpa_ctrl_request(
        ctrl: *mut c_void,
        cmd: *const c_char,
        cmd_len: size_t,
        reply: *mut c_char,
        reply_len: *mut size_t,
        msg_cb: Option<unsafe extern "C" fn(msg: *mut c_char, len: size_t)>,
    ) -> c_int;
    fn wpa_ctrl_close(ctrl: *mut c_void);
    fn wpa_ctrl_pending(ctrl: *mut c_void) -> c_int;
    fn wpa_ctrl_recv(ctrl: *mut c_void, reply: *mut c_char, len: *mut size_t) -> c_int;
}

lazy_static! {
    static ref CALLBACK: Mutex<RefCell<Box<FnMut(Result<&str>) + Send>>> = Mutex::new(RefCell::new(Box::new(|_|())));
}

fn request_cb<F: Fn(Result<&str>)>(f: Option<F>) -> Option<unsafe extern "C" fn(*mut c_char, size_t)> {
    match f {
        Some(_) => {
            unsafe extern "C" fn wrapped(msg: *mut c_char, len: size_t) {
                use std::ops::DerefMut;
                let x = CALLBACK.lock().unwrap();
                (x.borrow_mut().deref_mut())(std::str::from_utf8(std::slice::from_raw_parts(msg as *const u8, len))
                    .map_err(Error::from));
            }
            Some(wrapped)
        }
        None => None,
    }
}

/// Send a command to wpa_supplicant/hostapd. 
fn request_helper(handle: *mut c_void, cmd: &str, cb: Option<fn(Result<&str>)>) -> Result<String> {
    let mut res_len: size_t = 10240;
    let mut res = Vec::with_capacity(10240);
    let c_cmd = CString::new(cmd)?;
    let c_cmd_len = c_cmd.as_bytes().len();

    match unsafe {
        wpa_ctrl_request(
            handle,
            c_cmd.as_ptr(),
            c_cmd_len,
            res.as_mut_ptr() as *mut c_char,
            &mut res_len,
            request_cb(cb),
        )
    } {
        0 => {
            unsafe {
                res.set_len(res_len);
            }
            Ok(String::from_utf8(res)?)
        }
        -1 => Err(WpaError::Failure.into()),
        -2 => Err(WpaError::Timeout.into()),
        x => Err(WpaError::Unknown(x).into()),
    }
}


#[derive(Default)]
pub struct WpaCtrlBuilder {
    cli_path: Option<PathBuf>,
    ctrl_path: Option<PathBuf>,
}

impl WpaCtrlBuilder {
    /// A path-like object for client UNIX domain socket
    pub fn cli_path<I: Into<Option<P>>, P>(&mut self, cli_path: P) -> &mut Self where P: AsRef<Path> + Sized, Option<PathBuf>: From<P> {
        let new = self;
        new.cli_path = cli_path.into();
        new
    }

    /// A path-like object for UNIX domain sockets
    pub fn ctrl_path<I: Into<Option<P>>, P>(&mut self, ctrl_path: P) -> &mut Self where P: AsRef<Path> + Sized, Option<PathBuf>: From<P> {
        let new = self;
        new.ctrl_path = ctrl_path.into();
        new
    }

    /// Open a control interface to wpa_supplicant.
    /// 
    /// # Errors
    ///
    /// * WpaError::Interface - Unable to open the interface
    ///
    /// # Examples
    ///
    /// ```
    /// use wpactrl::WpaCtrl;
    /// let wpa = WpaCtrl::new().open().unwrap();
    /// ```
    pub fn open(self) -> Result<WpaCtrl> {
        let ctrl_path = self.ctrl_path.unwrap_or("/var/run/wpa_supplicant/wlan0".into());
        let handle = unsafe { wpa_ctrl_open2(
            CString::new(ctrl_path.as_path().as_os_str().as_bytes())?.as_ptr(),
            if let Some(cli_path) = self.cli_path {
                CString::new(cli_path.as_path().as_os_str().as_bytes())?.as_ptr()
            } else {
                ptr::null()
            }
        ) };
        if handle == ptr::null_mut() {
            Err(WpaError::Interface)?
        } else {
            Ok(WpaCtrl(handle))
        }
    }
}

/// A connection to wpa_supplicant / hostap
pub struct WpaCtrl(*mut c_void);

impl WpaCtrl {
    /// Creates a builder for a wpa_supplicant / hostap connection
    ///
    /// # Examples
    ///
    /// ```
    /// let wpa = wpactrl::WpaCtrl::new().open().unwrap();
    /// ```
    pub fn new() -> WpaCtrlBuilder {
        WpaCtrlBuilder::default()
    }

    /// Register as an event monitor for the control interface.
    /// 
    /// # Examples
    ///
    /// ```
    /// let mut wpa = wpactrl::WpaCtrl::new().open().unwrap();
    /// let wpa_attached = wpa.attach().unwrap();
    /// ```
    pub fn attach(self) -> Result<WpaCtrlAttached> {
        if request_helper(self.0, "ATTACH", None)? != "OK\n" {
            Err(WpaError::Failure.into())
        } else {
            let handle = self.0;
            std::mem::forget(self);
            Ok(WpaCtrlAttached(handle))
        }
    }

    /// Send a command to wpa_supplicant/hostapd. 
    /// 
    /// # Examples
    ///
    /// ```
    /// let mut wpa = wpactrl::WpaCtrl::new().open().unwrap();
    /// wpa.request("PING").unwrap();
    /// ```
    pub fn request(&mut self, cmd: &str) -> Result<String> {
        request_helper(self.0, cmd, None)
    }
}

impl Drop for WpaCtrl {
    fn drop(&mut self) {
        unsafe {
            wpa_ctrl_close(self.0);
        }
    }
}

/// A connection to wpa_supplicant / hostap that receives status messages
pub struct WpaCtrlAttached(*mut c_void);

impl WpaCtrlAttached {

    /// Unregister event monitor from the control interface.
    /// 
    /// # Examples
    ///
    /// ```
    /// let mut wpa = wpactrl::WpaCtrl::new().open().unwrap().attach().unwrap();
    /// wpa.detach().unwrap();
    /// ```
    pub fn detach(self) -> Result<WpaCtrl> {
        if request_helper(self.0, "DETACH", None)? != "OK\n" {
            Err(WpaError::Failure.into())
        } else {
            let handle = self.0;
            std::mem::forget(self);
            Ok(WpaCtrl(handle))
        }
    }

    /// Check whether there are pending event messages.
    /// 
    /// # Examples
    ///
    /// ```
    /// let mut wpa = wpactrl::WpaCtrl::new().open().unwrap().attach().unwrap();
    /// wpa.pending().unwrap();
    /// ```
    pub fn pending(&mut self) -> Result<bool> {
        match unsafe { wpa_ctrl_pending(self.0) } {
            0 => Ok(false),
            1 => Ok(true),
            -1 => Err(WpaError::Failure.into()),
            x => Err(WpaError::Unknown(x).into()),
        }
    }

    /// Receive a pending control interface message.
    /// 
    /// # Examples
    ///
    /// ```
    /// let mut wpa = wpactrl::WpaCtrl::new().open().unwrap().attach().unwrap();
    /// if wpa.pending().unwrap() {
    ///     wpa.recv().unwrap();
    /// }
    /// ```
    pub fn recv(&mut self) -> Result<String> {
        let mut res_len: size_t = 10240;
        let mut res = Vec::with_capacity(res_len);
        match unsafe { wpa_ctrl_recv(self.0, res.as_mut_ptr() as *mut c_char, &mut res_len) } {
            0 => {
                unsafe {
                    res.set_len(res_len);
                }
                Ok(String::from_utf8(res)?)
            }
            -1 => Err(WpaError::Failure.into()),
            x => Err(WpaError::Unknown(x).into()),
        }
    }

    pub fn request(&mut self, cmd: &str, cb: fn(Result<&str>)) -> Result<String> {
        request_helper(self.0, cmd, Some(cb))
    }
}

impl Drop for WpaCtrlAttached {
    fn drop(&mut self) {
        unsafe {
            wpa_ctrl_close(self.0);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn assert_err<T: std::fmt::Debug>(r: Result<T>, e2: WpaError) {
        assert_eq!(r.unwrap_err().downcast::<WpaError>().unwrap(), e2);
    }

    fn wpa_ctrl() -> WpaCtrl {
        WpaCtrl::new().open().unwrap()
    }

    #[test]
    fn attach() {
        wpa_ctrl().attach().unwrap().detach().unwrap().attach().unwrap().detach().unwrap();
    }

    #[test]
    fn detach() {
        let wpa = wpa_ctrl().attach().unwrap();
        wpa.detach().unwrap();
    }

    #[test]
    fn new() {
        wpa_ctrl();
    }

    #[test]
    fn request() {
        let mut wpa = wpa_ctrl();
        assert_eq!(wpa.request("PING").unwrap(), "PONG\n");
        let mut wpa_attached = wpa.attach().unwrap();
        // FIXME: This may not trigger the callback
        assert_eq!(wpa_attached.request("PING", |s|println!("CB: {:?}", s.unwrap())).unwrap(), "PONG\n");
    }

    #[test]
    fn pending() {
        let mut wpa = wpa_ctrl().attach().unwrap();
        assert_eq!(wpa.pending().unwrap(), false);
        wpa.detach().unwrap();
    }

    #[test]
    fn recv() {
        let mut wpa = wpa_ctrl().attach().unwrap();
        assert_err(wpa.recv(), WpaError::Failure);
        assert_eq!(wpa.request("SCAN", |_|()).unwrap(), "OK\n");
        while !wpa.pending().unwrap() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(&wpa.recv().unwrap()[3..], "CTRL-EVENT-SCAN-STARTED ");
        wpa.detach().unwrap();
    }
}
