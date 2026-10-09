//! The few libcurl easy-interface calls the HTTP client makes (declared by hand: no headers or
//! bindings crate needed). One handle per thread, kept between requests so that connections
//! are reused.

use libc::{c_char, c_int, c_long, c_void, size_t};
use std::ffi::{CStr, CString};

#[allow(non_camel_case_types)]
type CURL = c_void;
#[allow(non_camel_case_types)]
pub type curl_off_t = i64;

#[repr(C)]
struct Slist {
    data: *mut c_char,
    next: *mut Slist,
}

extern "C" {
    fn curl_global_init(flags: c_long) -> c_int;
    fn curl_easy_init() -> *mut CURL;
    fn curl_easy_reset(h: *mut CURL);
    fn curl_easy_setopt(h: *mut CURL, opt: c_int, ...) -> c_int;
    fn curl_easy_perform(h: *mut CURL) -> c_int;
    fn curl_easy_getinfo(h: *mut CURL, info: c_int, ...) -> c_int;
    fn curl_easy_strerror(code: c_int) -> *const c_char;
    fn curl_slist_append(l: *mut Slist, s: *const c_char) -> *mut Slist;
    fn curl_slist_free_all(l: *mut Slist);
}

pub const URL: c_int = 10002;
pub const WRITEFUNCTION: c_int = 20011;
pub const WRITEDATA: c_int = 10001;
pub const HEADERFUNCTION: c_int = 20079;
pub const HEADERDATA: c_int = 10029;
pub const READFUNCTION: c_int = 20012;
pub const READDATA: c_int = 10009;
pub const UPLOAD: c_int = 46;
pub const INFILESIZE_LARGE: c_int = 30115;
pub const CUSTOMREQUEST: c_int = 10036;
pub const HTTPHEADER: c_int = 10023;
pub const NOBODY: c_int = 44;
pub const HTTPGET: c_int = 80;
pub const SSL_VERIFYPEER: c_int = 64;
pub const SSL_VERIFYHOST: c_int = 81;
pub const CAINFO: c_int = 10065;
pub const SSLCERT: c_int = 10025;
pub const SSLKEY: c_int = 10087;
pub const KEYPASSWD: c_int = 10026;
pub const PROXY: c_int = 10004;
pub const NOPROXY: c_int = 10177;
pub const CONNECTTIMEOUT: c_int = 78;
pub const LOW_SPEED_LIMIT: c_int = 19;
pub const LOW_SPEED_TIME: c_int = 20;
pub const HTTP_VERSION: c_int = 84;
pub const COOKIEFILE: c_int = 10031;
pub const NOSIGNAL: c_int = 99;
pub const ACCEPT_ENCODING: c_int = 10102;
pub const HTTP_CONTENT_DECODING: c_int = 158;
pub const TCP_KEEPALIVE: c_int = 213;
pub const TCP_KEEPIDLE: c_int = 214;

pub const INFO_NAMELOOKUP_TIME_T: c_int = 0x600000 + 52;
pub const INFO_CONNECT_TIME_T: c_int = 0x600000 + 53;
pub const INFO_APPCONNECT_TIME_T: c_int = 0x600000 + 56;
pub const INFO_STARTTRANSFER_TIME_T: c_int = 0x600000 + 55;

pub const HTTP_VERSION_1_1: c_long = 2;
pub const HTTP_VERSION_2TLS: c_long = 4;

pub const E_COULDNT_RESOLVE_HOST: c_int = 6;
pub const E_COULDNT_CONNECT: c_int = 7;
pub const E_OPERATION_TIMEDOUT: c_int = 28;
pub const E_PEER_FAILED_VERIFICATION: c_int = 60;

pub const READFUNC_ABORT: size_t = 0x10000000;

pub struct Easy {
    h: *mut CURL,
    /// Strings and lists handed to libcurl, alive until the next reset.
    strings: Vec<CString>,
    lists: Vec<*mut Slist>,
}

impl Drop for Easy {
    fn drop(&mut self) {
        self.free_lists();
    }
}

impl Easy {
    pub fn new() -> Easy {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| unsafe {
            curl_global_init(3);
        });
        let h = unsafe { curl_easy_init() };
        assert!(!h.is_null(), "curl_easy_init failed");
        Easy { h, strings: vec![], lists: vec![] }
    }

    fn free_lists(&mut self) {
        for l in self.lists.drain(..) {
            unsafe { curl_slist_free_all(l) };
        }
        self.strings.clear();
    }

    /// Options back to their defaults (the connection cache stays).
    pub fn reset(&mut self) {
        unsafe { curl_easy_reset(self.h) };
        self.free_lists();
        self.long(NOSIGNAL, 1);
    }

    pub fn long(&mut self, opt: c_int, v: c_long) {
        unsafe { curl_easy_setopt(self.h, opt, v) };
    }

    pub fn off(&mut self, opt: c_int, v: curl_off_t) {
        unsafe { curl_easy_setopt(self.h, opt, v) };
    }

    pub fn str(&mut self, opt: c_int, v: &str) {
        let c = CString::new(v.replace('\0', "")).unwrap();
        unsafe { curl_easy_setopt(self.h, opt, c.as_ptr()) };
        self.strings.push(c);
    }

    pub fn list(&mut self, opt: c_int, items: &[String]) {
        let mut l: *mut Slist = std::ptr::null_mut();
        for i in items {
            let c = CString::new(i.replace('\0', "")).unwrap();
            l = unsafe { curl_slist_append(l, c.as_ptr()) };
        }
        unsafe { curl_easy_setopt(self.h, opt, l) };
        self.lists.push(l);
    }

    pub fn write_function(&mut self, f: extern "C" fn(*mut c_char, size_t, size_t, *mut c_void) -> size_t, data: *mut c_void) {
        unsafe {
            curl_easy_setopt(self.h, WRITEFUNCTION, f);
            curl_easy_setopt(self.h, WRITEDATA, data);
        }
    }

    pub fn header_function(&mut self, f: extern "C" fn(*mut c_char, size_t, size_t, *mut c_void) -> size_t, data: *mut c_void) {
        unsafe {
            curl_easy_setopt(self.h, HEADERFUNCTION, f);
            curl_easy_setopt(self.h, HEADERDATA, data);
        }
    }

    pub fn read_function(&mut self, f: extern "C" fn(*mut c_char, size_t, size_t, *mut c_void) -> size_t, data: *mut c_void) {
        unsafe {
            curl_easy_setopt(self.h, READFUNCTION, f);
            curl_easy_setopt(self.h, READDATA, data);
        }
    }

    pub fn perform(&mut self) -> c_int {
        unsafe { curl_easy_perform(self.h) }
    }

    pub fn info_off(&mut self, info: c_int) -> curl_off_t {
        let mut v: curl_off_t = 0;
        unsafe { curl_easy_getinfo(self.h, info, &mut v as *mut curl_off_t) };
        v
    }
}

pub fn strerror(code: c_int) -> String {
    unsafe { CStr::from_ptr(curl_easy_strerror(code)) }.to_string_lossy().into_owned()
}

thread_local! {
    static HANDLE: std::cell::RefCell<Option<Easy>> = const { std::cell::RefCell::new(None) };
}

/// This thread's handle, reset, for one request.
pub fn with_handle<T>(f: impl FnOnce(&mut Easy) -> T) -> T {
    HANDLE.with(|h| {
        let mut h = h.borrow_mut();
        let e = h.get_or_insert_with(Easy::new);
        e.reset();
        f(e)
    })
}
