use libc::{c_int, c_void, c_long};
use std::any::TypeId;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fmt;
use std::io;
use std::io::prelude::*;
use std::ffi::AsOsStr;
use std::mem;
use std::net;
use std::path::Path;
use std::ptr;
use std::sync::{Once, ONCE_INIT, Arc, Mutex};
use std::ops::{Deref, DerefMut};
use std::cmp;
use std::marker::Reflect;
#[cfg(feature = "npn")]
use libc::{c_uchar, c_uint};
#[cfg(feature = "npn")]
use std::slice;

use bio::{MemBio};
use ffi;
use ssl::error::{SslError, SslSessionClosed, StreamError, OpenSslErrors};
use x509::{X509StoreContext, X509FileType, X509};

pub mod error;
#[cfg(test)]
mod tests;

static mut VERIFY_IDX: c_int = -1;

fn init() {
    static mut INIT: Once = ONCE_INIT;

    unsafe {
        INIT.call_once(|| {
            ffi::init();

            let verify_idx = ffi::SSL_CTX_get_ex_new_index(0, ptr::null(), None,
                                                           None, None);
            assert!(verify_idx >= 0);
            VERIFY_IDX = verify_idx;
        });
    }
}

bitflags! {
    flags SslContextOptions: c_long {
        const SSL_OP_LEGACY_SERVER_CONNECT = 0x00000004,
        const SSL_OP_NETSCAPE_REUSE_CIPHER_CHANGE_BUG = 0x00000008,
        const SSL_OP_TLSEXT_PADDING = 0x00000010,
        const SSL_OP_MICROSOFT_BIG_SSLV3_BUFFER = 0x00000020,
        const SSL_OP_SAFARI_ECDHE_ECDSA_BUG = 0x00000040,
        const SSL_OP_SSLEAY_080_CLIENT_DH_BUG = 0x00000080,
        const SSL_OP_TLS_D5_BUG = 0x00000100,
        const SSL_OP_TLS_BLOCK_PADDING_BUG = 0x00000200,
        const SSL_OP_DONT_INSERT_EMPTY_FRAGMENTS = 0x00000800,
        const SSL_OP_ALL = 0x80000BFF,
        const SSL_OP_NO_QUERY_MTU = 0x00001000,
        const SSL_OP_COOKIE_EXCHANGE = 0x00002000,
        const SSL_OP_NO_TICKET = 0x00004000,
        const SSL_OP_CISCO_ANYCONNECT = 0x00008000,
        const SSL_OP_NO_SESSION_RESUMPTION_ON_RENEGOTIATION = 0x00010000,
        const SSL_OP_NO_COMPRESSION = 0x00020000,
        const SSL_OP_ALLOW_UNSAFE_LEGACY_RENEGOTIATION = 0x00040000,
        const SSL_OP_SINGLE_ECDH_USE = 0x00080000,
        const SSL_OP_SINGLE_DH_USE = 0x00100000,
        const SSL_OP_CIPHER_SERVER_PREFERENCE = 0x00400000,
        const SSL_OP_TLS_ROLLBACK_BUG = 0x00800000,
        const SSL_OP_NO_SSLV2 = 0x00000000,
        const SSL_OP_NO_SSLV3 = 0x02000000,
        const SSL_OP_NO_TLSV1 = 0x04000000,
        const SSL_OP_NO_TLSV1_2 = 0x08000000,
        const SSL_OP_NO_TLSV1_1 = 0x10000000,
        const SSL_OP_NO_DTLSV1 = 0x04000000,
        const SSL_OP_NO_DTLSV1_2 = 0x08000000
    }
}

/// Determines the SSL method supported
#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum SslMethod {
    #[cfg(feature = "sslv2")]
    /// Only support the SSLv2 protocol, requires the `sslv2` feature.
    Sslv2,
    /// Support the SSLv2, SSLv3 and TLSv1 protocols.
    Sslv23,
    /// Only support the SSLv3 protocol.
    Sslv3,
    /// Only support the TLSv1 protocol.
    Tlsv1,
    #[cfg(feature = "tlsv1_1")]
    /// Support TLSv1.1 protocol, requires the `tlsv1_1` feature.
    Tlsv1_1,
    #[cfg(feature = "tlsv1_2")]
    /// Support TLSv1.2 protocol, requires the `tlsv1_2` feature.
    Tlsv1_2,
}

impl SslMethod {
    unsafe fn to_raw(&self) -> *const ffi::SSL_METHOD {
        match *self {
            #[cfg(feature = "sslv2")]
            SslMethod::Sslv2 => ffi::SSLv2_method(),
            SslMethod::Sslv3 => ffi::SSLv3_method(),
            SslMethod::Tlsv1 => ffi::TLSv1_method(),
            SslMethod::Sslv23 => ffi::SSLv23_method(),
            #[cfg(feature = "tlsv1_1")]
            SslMethod::Tlsv1_1 => ffi::TLSv1_1_method(),
            #[cfg(feature = "tlsv1_2")]
            SslMethod::Tlsv1_2 => ffi::TLSv1_2_method()
        }
    }
}

/// Determines the type of certificate verification used
#[derive(Copy, Clone, Debug)]
#[repr(i32)]
pub enum SslVerifyMode {
    /// Verify that the server's certificate is trusted
    SslVerifyPeer = ffi::SSL_VERIFY_PEER,
    /// Do not verify the server's certificate
    SslVerifyNone = ffi::SSL_VERIFY_NONE
}

lazy_static! {
    static ref INDEXES: Mutex<HashMap<TypeId, c_int>> = Mutex::new(HashMap::new());
}

// Creates a static index for user data of type T
// Registers a destructor for the data which will be called
// when context is freed
fn get_verify_data_idx<T: Reflect + 'static>() -> c_int {
    extern fn free_data_box<T>(_parent: *mut c_void, ptr: *mut c_void,
                               _ad: *mut ffi::CRYPTO_EX_DATA, _idx: c_int,
                               _argl: c_long, _argp: *mut c_void) {
        if ptr != 0 as *mut _ {
            let _: Box<T> = unsafe { mem::transmute(ptr) };
        }
    }

    *INDEXES.lock().unwrap().entry(TypeId::of::<T>()).or_insert_with(|| {
        unsafe {
            let f: ffi::CRYPTO_EX_free = free_data_box::<T>;
            let idx = ffi::SSL_CTX_get_ex_new_index(0, ptr::null(), None, None, Some(f));
            assert!(idx >= 0);
            idx
        }
    })
}

/// Creates a static index for the list of NPN protocols.
/// Registers a destructor for the data which will be called
/// when the context is freed.
#[cfg(feature = "npn")]
fn get_npn_protos_idx() -> c_int {
    static mut NPN_PROTOS_IDX: c_int = -1;
    static mut INIT: Once = ONCE_INIT;

    extern fn free_data_box(_parent: *mut c_void, ptr: *mut c_void,
                            _ad: *mut ffi::CRYPTO_EX_DATA, _idx: c_int,
                            _argl: c_long, _argp: *mut c_void) {
        if !ptr.is_null() {
            let _: Box<Vec<u8>> = unsafe { mem::transmute(ptr) };
        }
    }

    unsafe {
        INIT.call_once(|| {
            let f: ffi::CRYPTO_EX_free = free_data_box;
            let idx = ffi::SSL_CTX_get_ex_new_index(0, ptr::null(), None,
                                                    None, Some(f));
            assert!(idx >= 0);
            NPN_PROTOS_IDX = idx;
        });
        NPN_PROTOS_IDX
    }
}

extern fn raw_verify(preverify_ok: c_int, x509_ctx: *mut ffi::X509_STORE_CTX)
        -> c_int {
    unsafe {
        let idx = ffi::SSL_get_ex_data_X509_STORE_CTX_idx();
        let ssl = ffi::X509_STORE_CTX_get_ex_data(x509_ctx, idx);
        let ssl_ctx = ffi::SSL_get_SSL_CTX(ssl);
        let verify = ffi::SSL_CTX_get_ex_data(ssl_ctx, VERIFY_IDX);
        let verify: Option<VerifyCallback> = mem::transmute(verify);

        let ctx = X509StoreContext::new(x509_ctx);

        match verify {
            None => preverify_ok,
            Some(verify) => verify(preverify_ok != 0, &ctx) as c_int
        }
    }
}

extern fn raw_verify_with_data<T>(preverify_ok: c_int,
                                  x509_ctx: *mut ffi::X509_STORE_CTX) -> c_int
                                  where T: Reflect + 'static {
    unsafe {
        let idx = ffi::SSL_get_ex_data_X509_STORE_CTX_idx();
        let ssl = ffi::X509_STORE_CTX_get_ex_data(x509_ctx, idx);
        let ssl_ctx = ffi::SSL_get_SSL_CTX(ssl);

        let verify = ffi::SSL_CTX_get_ex_data(ssl_ctx, VERIFY_IDX);
        let verify: Option<VerifyCallbackData<T>> = mem::transmute(verify);

        let data = ffi::SSL_CTX_get_ex_data(ssl_ctx, get_verify_data_idx::<T>());
        let data: Box<T> = mem::transmute(data);

        let ctx = X509StoreContext::new(x509_ctx);

        let res = match verify {
            None => preverify_ok,
            Some(verify) => verify(preverify_ok != 0, &ctx, &*data) as c_int
        };

        // Since data might be required on the next verification
        // it is time to forget about it and avoid dropping
        // data will be freed once OpenSSL considers it is time
        // to free all context data
        mem::forget(data);
        res
    }
}

/// The function is given as the callback to `SSL_CTX_set_next_proto_select_cb`.
///
/// It chooses the protocol that the client wishes to use, out of the given list of protocols
/// supported by the server. It achieves this by delegating to the `SSL_select_next_proto`
/// function. The list of protocols supported by the client is found in the extra data of the
/// OpenSSL context.
#[cfg(feature = "npn")]
extern fn raw_next_proto_select_cb(ssl: *mut ffi::SSL,
                                   out: *mut *mut c_uchar, outlen: *mut c_uchar,
                                   inbuf: *const c_uchar, inlen: c_uint,
                                   _arg: *mut c_void) -> c_int {
    unsafe {
        // First, get the list of protocols (that the client should support) saved in the context
        // extra data.
        let ssl_ctx = ffi::SSL_get_SSL_CTX(ssl);
        let protocols = ffi::SSL_CTX_get_ex_data(ssl_ctx, get_npn_protos_idx());
        let protocols: &Vec<u8> = mem::transmute(protocols);
        // Prepare the client list parameters to be passed to the OpenSSL function...
        let client = protocols.as_ptr();
        let client_len = protocols.len() as c_uint;
        // Finally, let OpenSSL find a protocol to be used, by matching the given server and
        // client lists.
        ffi::SSL_select_next_proto(out, outlen, inbuf, inlen, client, client_len);
    }

    ffi::SSL_TLSEXT_ERR_OK
}

/// The function is given as the callback to `SSL_CTX_set_next_protos_advertised_cb`.
///
/// It causes the parameter `out` to point at a `*const c_uchar` instance that
/// represents the list of protocols that the server should advertise as those
/// that it supports.
/// The list of supported protocols is found in the extra data of the OpenSSL
/// context.
#[cfg(feature = "npn")]
extern fn raw_next_protos_advertise_cb(ssl: *mut ffi::SSL,
                                       out: *mut *const c_uchar, outlen: *mut c_uint,
                                       _arg: *mut c_void) -> c_int {
    unsafe {
        // First, get the list of (supported) protocols saved in the context extra data.
        let ssl_ctx = ffi::SSL_get_SSL_CTX(ssl);
        let protocols = ffi::SSL_CTX_get_ex_data(ssl_ctx, get_npn_protos_idx());
        if protocols.is_null() {
            *out = b"".as_ptr();
            *outlen = 0;
        } else {
            // If the pointer is valid, put the pointer to the actual byte array into the
            // output parameter `out`, as well as its length into `outlen`.
            let protocols: &Vec<u8> = mem::transmute(protocols);
            *out = protocols.as_ptr();
            *outlen = protocols.len() as c_uint;
        }
    }

    ffi::SSL_TLSEXT_ERR_OK
}

/// The signature of functions that can be used to manually verify certificates
pub type VerifyCallback = fn(preverify_ok: bool,
                             x509_ctx: &X509StoreContext) -> bool;

/// The signature of functions that can be used to manually verify certificates
/// when user-data should be carried for all verification process
pub type VerifyCallbackData<T> = fn(preverify_ok: bool,
                                    x509_ctx: &X509StoreContext,
                                    data: &T) -> bool;

// FIXME: macro may be instead of inlining?
#[inline]
fn wrap_ssl_result(res: c_int) -> Option<SslError> {
    if res == 0 {
        Some(SslError::get())
    } else {
        None
    }
}

/// An SSL context object
pub struct SslContext {
    ctx: ptr::Unique<ffi::SSL_CTX>
}

// TODO: add useful info here
impl fmt::Debug for SslContext {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "SslContext")
    }
}

impl Drop for SslContext {
    fn drop(&mut self) {
        unsafe { ffi::SSL_CTX_free(*self.ctx) }
    }
}

impl SslContext {
    /// Creates a new SSL context.
    pub fn new(method: SslMethod) -> Result<SslContext, SslError> {
        init();

        let ctx = unsafe { ffi::SSL_CTX_new(method.to_raw()) };
        if ctx == ptr::null_mut() {
            return Err(SslError::get());
        }

        let ctx = unsafe { SslContext { ctx: ptr::Unique::new(ctx) } };
        Ok(ctx)
    }

    /// Configures the certificate verification method for new connections.
    pub fn set_verify(&mut self, mode: SslVerifyMode,
                      verify: Option<VerifyCallback>) {
        unsafe {
            ffi::SSL_CTX_set_ex_data(*self.ctx, VERIFY_IDX,
                                     mem::transmute(verify));
            let f: extern fn(c_int, *mut ffi::X509_STORE_CTX) -> c_int =
                                raw_verify;
            ffi::SSL_CTX_set_verify(*self.ctx, mode as c_int, Some(f));
        }
    }

    /// Configures the certificate verification method for new connections also
    /// carrying supplied data.
    // Note: no option because there is no point to set data without providing
    // a function handling it
    pub fn set_verify_with_data<T>(&mut self, mode: SslVerifyMode,
                                   verify: VerifyCallbackData<T>,
                                   data: T)
                                   where T: Reflect + 'static {
        let data = Box::new(data);
        unsafe {
            ffi::SSL_CTX_set_ex_data(*self.ctx, VERIFY_IDX,
                                     mem::transmute(Some(verify)));
            ffi::SSL_CTX_set_ex_data(*self.ctx, get_verify_data_idx::<T>(),
                                     mem::transmute(data));
            let f: extern fn(c_int, *mut ffi::X509_STORE_CTX) -> c_int =
                                raw_verify_with_data::<T>;
            ffi::SSL_CTX_set_verify(*self.ctx, mode as c_int, Some(f));
        }
    }

    /// Sets verification depth
    pub fn set_verify_depth(&mut self, depth: u32) {
        unsafe {
            ffi::SSL_CTX_set_verify_depth(*self.ctx, depth as c_int);
        }
    }

    #[allow(non_snake_case)]
    /// Specifies the file that contains trusted CA certificates.
    pub fn set_CA_file(&mut self, file: &Path) -> Option<SslError> {
        let file = CString::new(file.as_os_str().to_str().expect("invalid utf8")).unwrap();
        wrap_ssl_result(
            unsafe {
                ffi::SSL_CTX_load_verify_locations(*self.ctx, file.as_ptr(), ptr::null())
            })
    }

    /// Specifies the file that contains certificate
    pub fn set_certificate_file(&mut self, file: &Path,
                                file_type: X509FileType) -> Option<SslError> {
        let file = CString::new(file.as_os_str().to_str().expect("invalid utf8")).unwrap();
        wrap_ssl_result(
            unsafe {
                ffi::SSL_CTX_use_certificate_file(*self.ctx, file.as_ptr(), file_type as c_int)
            })
    }

    /// Specifies the file that contains private key
    pub fn set_private_key_file(&mut self, file: &Path,
                                file_type: X509FileType) -> Option<SslError> {
        let file = CString::new(file.as_os_str().to_str().expect("invalid utf8")).unwrap();
        wrap_ssl_result(
            unsafe {
                ffi::SSL_CTX_use_PrivateKey_file(*self.ctx, file.as_ptr(), file_type as c_int)
            })
    }

    pub fn set_cipher_list(&mut self, cipher_list: &str) -> Option<SslError> {
        wrap_ssl_result(
            unsafe {
                let cipher_list = CString::new(cipher_list.as_bytes()).unwrap();
                ffi::SSL_CTX_set_cipher_list(*self.ctx, cipher_list.as_ptr())
            })
    }

    pub fn set_options(&mut self, option: SslContextOptions) -> SslContextOptions {
        let raw_bits = option.bits();
        let ret = unsafe {
            ffi::SSL_CTX_set_options(*self.ctx, raw_bits)
        };
        SslContextOptions::from_bits(ret).unwrap()
    }

    pub fn get_options(&mut self) -> SslContextOptions {
        let ret = unsafe {
            ffi::SSL_CTX_get_options(*self.ctx)
        };
        SslContextOptions::from_bits(ret).unwrap()
    }

    pub fn clear_options(&mut self, option: SslContextOptions) -> SslContextOptions {
        let raw_bits = option.bits();
        let ret = unsafe {
            ffi::SSL_CTX_clear_options(*self.ctx, raw_bits)
        };
        SslContextOptions::from_bits(ret).unwrap()
    }

    /// Set the protocols to be used during Next Protocol Negotiation (the protocols
    /// supported by the application).
    ///
    /// This method needs the `npn` feature.
    #[cfg(feature = "npn")]
    pub fn set_npn_protocols(&mut self, protocols: &[&[u8]]) {
        // Firstly, convert the list of protocols to a byte-array that can be passed to OpenSSL
        // APIs -- a list of length-prefixed strings.
        let mut npn_protocols = Vec::new();
        for protocol in protocols {
            let len = protocol.len() as u8;
            npn_protocols.push(len);
            // If the length is greater than the max `u8`, this truncates the protocol name.
            npn_protocols.extend(protocol[..len as usize].to_vec());
        }
        let protocols: Box<Vec<u8>> = Box::new(npn_protocols);

        unsafe {
            // Attach the protocol list to the OpenSSL context structure,
            // so that we can refer to it within the callback.
            ffi::SSL_CTX_set_ex_data(*self.ctx, get_npn_protos_idx(),
                                     mem::transmute(protocols));
            // Now register the callback that performs the default protocol
            // matching based on the client-supported list of protocols that
            // has been saved.
            ffi::SSL_CTX_set_next_proto_select_cb(*self.ctx, raw_next_proto_select_cb, ptr::null_mut());
            // Also register the callback to advertise these protocols, if a server socket is
            // created with the context.
            ffi::SSL_CTX_set_next_protos_advertised_cb(*self.ctx, raw_next_protos_advertise_cb, ptr::null_mut());
        }
    }
}

#[allow(dead_code)]
struct MemBioRef<'ssl> {
    ssl: &'ssl Ssl,
    bio: MemBio,
}

impl<'ssl> Deref for MemBioRef<'ssl> {
    type Target = MemBio;

    fn deref(&self) -> &MemBio {
        &self.bio
    }
}

impl<'ssl> DerefMut for MemBioRef<'ssl> {
    fn deref_mut(&mut self) -> &mut MemBio {
        &mut self.bio
    }
}

pub struct Ssl {
    ssl: ptr::Unique<ffi::SSL>
}

// TODO: put useful information here
impl fmt::Debug for Ssl {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "Ssl")
    }
}

impl Drop for Ssl {
    fn drop(&mut self) {
        unsafe { ffi::SSL_free(*self.ssl) }
    }
}

impl Ssl {
    pub fn new(ctx: &SslContext) -> Result<Ssl, SslError> {
        let ssl = unsafe { ffi::SSL_new(*ctx.ctx) };
        if ssl == ptr::null_mut() {
            return Err(SslError::get());
        }
        let ssl = unsafe { Ssl { ssl: ptr::Unique::new(ssl) } };

        let rbio = try!(MemBio::new());
        let wbio = try!(MemBio::new());

        unsafe { ffi::SSL_set_bio(*ssl.ssl, rbio.unwrap(), wbio.unwrap()) }
        Ok(ssl)
    }

    fn get_rbio<'a>(&'a self) -> MemBioRef<'a> {
        unsafe { self.wrap_bio(ffi::SSL_get_rbio(*self.ssl)) }
    }

    fn get_wbio<'a>(&'a self) -> MemBioRef<'a> {
        unsafe { self.wrap_bio(ffi::SSL_get_wbio(*self.ssl)) }
    }

    fn wrap_bio<'a>(&'a self, bio: *mut ffi::BIO) -> MemBioRef<'a> {
        assert!(bio != ptr::null_mut());
        MemBioRef {
            ssl: self,
            bio: MemBio::borrowed(bio)
        }
    }

    fn connect(&self) -> c_int {
        unsafe { ffi::SSL_connect(*self.ssl) }
    }

    fn accept(&self) -> c_int {
        unsafe { ffi::SSL_accept(*self.ssl) }
    }

    fn read(&self, buf: &mut [u8]) -> c_int {
        let len = cmp::min(c_int::max_value() as usize, buf.len()) as c_int;
        unsafe { ffi::SSL_read(*self.ssl, buf.as_ptr() as *mut c_void, len) }
    }

    fn write(&self, buf: &[u8]) -> c_int {
        let len = cmp::min(c_int::max_value() as usize, buf.len()) as c_int;
        unsafe { ffi::SSL_write(*self.ssl, buf.as_ptr() as *const c_void, len) }
    }

    fn get_error(&self, ret: c_int) -> LibSslError {
        let err = unsafe { ffi::SSL_get_error(*self.ssl, ret) };
        match LibSslError::from_i32(err as i32) {
            Some(err) => err,
            None => unreachable!()
        }
    }

    /// Set the host name to be used with SNI (Server Name Indication).
    pub fn set_hostname(&self, hostname: &str) -> Result<(), SslError> {
        let ret = unsafe {
                // This is defined as a macro:
                //      #define SSL_set_tlsext_host_name(s,name) \
                //          SSL_ctrl(s,SSL_CTRL_SET_TLSEXT_HOSTNAME,TLSEXT_NAMETYPE_host_name,(char *)name)

                let hostname = CString::new(hostname.as_bytes()).unwrap();
                ffi::SSL_ctrl(*self.ssl, ffi::SSL_CTRL_SET_TLSEXT_HOSTNAME,
                              ffi::TLSEXT_NAMETYPE_host_name,
                              hostname.as_ptr() as *mut c_void)
        };

        // For this case, 0 indicates failure.
        if ret == 0 {
            Err(SslError::get())
        } else {
            Ok(())
        }
    }

    pub fn get_peer_certificate(&self) -> Option<X509> {
        unsafe {
            let ptr = ffi::SSL_get_peer_certificate(*self.ssl);
            if ptr.is_null() {
                None
            } else {
                Some(X509::new(ptr, true))
            }
        }
    }

    /// Returns the protocol selected by performing Next Protocol Negotiation, if any.
    ///
    /// The protocol's name is returned is an opaque sequence of bytes. It is up to the client
    /// to interpret it.
    ///
    /// This method needs the `npn` feature.
    #[cfg(feature = "npn")]
    pub fn get_selected_npn_protocol(&self) -> Option<&[u8]> {
        unsafe {
            let mut data: *const c_uchar = ptr::null();
            let mut len: c_uint = 0;
            // Get the negotiated protocol from the SSL instance.
            // `data` will point at a `c_uchar` array; `len` will contain the length of this array.
            ffi::SSL_get0_next_proto_negotiated(*self.ssl, &mut data, &mut len);

            if data.is_null() {
                None
            } else {
                Some(slice::from_raw_parts(data, len as usize))
            }
        }
    }
}

macro_rules! make_LibSslError {
    ($($variant:ident = $value:ident),+) => {
        #[derive(Debug)]
        #[repr(i32)]
        enum LibSslError {
            $($variant = ffi::$value),+
        }

        impl LibSslError {
            fn from_i32(val: i32) -> Option<LibSslError> {
                match val {
                    $(ffi::$value => Some(LibSslError::$variant),)+
                    _ => None
                }
            }
        }
    }
}

make_LibSslError! {
    ErrorNone = SSL_ERROR_NONE,
    ErrorSsl = SSL_ERROR_SSL,
    ErrorWantRead = SSL_ERROR_WANT_READ,
    ErrorWantWrite = SSL_ERROR_WANT_WRITE,
    ErrorWantX509Lookup = SSL_ERROR_WANT_X509_LOOKUP,
    ErrorSyscall = SSL_ERROR_SYSCALL,
    ErrorZeroReturn = SSL_ERROR_ZERO_RETURN,
    ErrorWantConnect = SSL_ERROR_WANT_CONNECT,
    ErrorWantAccept = SSL_ERROR_WANT_ACCEPT
}

/// A stream wrapper which handles SSL encryption for an underlying stream.
#[derive(Clone)]
pub struct SslStream<S> {
    stream: S,
    ssl: Arc<Ssl>,
    buf: Vec<u8>
}

impl SslStream<net::TcpStream> {
    /// Create a new independently owned handle to the underlying socket.
    pub fn try_clone(&self) -> io::Result<SslStream<net::TcpStream>> {
        Ok(SslStream { 
            stream: try!(self.stream.try_clone()),
            ssl: self.ssl.clone(),
            buf: self.buf.clone(),
        })
    }
}

impl<S> fmt::Debug for SslStream<S> where S: fmt::Debug {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "SslStream {{ stream: {:?}, ssl: {:?} }}", self.stream, self.ssl)
    }
}

impl<S: Read+Write> SslStream<S> {
    fn new_base(ssl:Ssl, stream: S) -> SslStream<S> {
        SslStream {
            stream: stream,
            ssl: Arc::new(ssl),
            // Maximum TLS record size is 16k
            // We're just using this as a buffer, so there's no reason to pay
            // to memset it
            buf: {
                const CAP: usize = 16 * 1024;
                let mut v = Vec::with_capacity(CAP);
                unsafe { v.set_len(CAP); }
                v
            }
        }
    }

    pub fn new_server_from(ssl: Ssl, stream: S) -> Result<SslStream<S>, SslError> {
        let mut ssl = SslStream::new_base(ssl, stream);
        ssl.in_retry_wrapper(|ssl| { ssl.accept() }).and(Ok(ssl))
    }

    /// Attempts to create a new SSL stream from a given `Ssl` instance.
    pub fn new_from(ssl: Ssl, stream: S) -> Result<SslStream<S>, SslError> {
        let mut ssl = SslStream::new_base(ssl, stream);
        ssl.in_retry_wrapper(|ssl| { ssl.connect() }).and(Ok(ssl))
    }

    /// Creates a new SSL stream
    pub fn new(ctx: &SslContext, stream: S) -> Result<SslStream<S>, SslError> {
        let ssl = try!(Ssl::new(ctx));
        SslStream::new_from(ssl, stream)
    }

    /// Creates a new SSL server stream
    pub fn new_server(ctx: &SslContext, stream: S) -> Result<SslStream<S>, SslError> {
        let ssl = try!(Ssl::new(ctx));
        SslStream::new_server_from(ssl, stream)
    }

    /// Returns a mutable reference to the underlying stream.
    ///
    /// ## Warning
    ///
    /// `read`ing or `write`ing directly to the underlying stream will most
    /// likely desynchronize the SSL session.
    #[deprecated="use get_mut instead"]
    pub fn get_inner(&mut self) -> &mut S {
        self.get_mut()
    }

    /// Returns a reference to the underlying stream.
    pub fn get_ref(&self) -> &S {
        &self.stream
    }

    /// Returns a mutable reference to the underlying stream.
    ///
    /// ## Warning
    ///
    /// It is inadvisable to read from or write to the underlying stream as it
    /// will most likely desynchronize the SSL session.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    fn in_retry_wrapper<F>(&mut self, mut blk: F)
            -> Result<c_int, SslError> where F: FnMut(&Ssl) -> c_int {
        loop {
            let ret = blk(&*self.ssl);
            if ret > 0 {
                return Ok(ret);
            }

            let e = self.ssl.get_error(ret);
            match e {
                LibSslError::ErrorWantRead => {
                    try_ssl_stream!(self.flush());
                    let len = try_ssl_stream!(self.stream.read(&mut self.buf[..]));
                    if len == 0 {
                        return Ok(0);
                    }
                    try_ssl_stream!(self.ssl.get_rbio().write_all(&self.buf[..len]));
                }
                LibSslError::ErrorWantWrite => { try_ssl_stream!(self.flush()) }
                LibSslError::ErrorZeroReturn => return Err(SslSessionClosed),
                LibSslError::ErrorSsl => return Err(SslError::get()),
                err => panic!("unexpected error {:?}", err),
            }
        }
    }

    fn write_through(&mut self) -> io::Result<()> {
        io::copy(&mut *self.ssl.get_wbio(), &mut self.stream).map(|_| ())
    }

    /// Get the compression currently in use.  The result will be
    /// either None, indicating no compression is in use, or a string
    /// with the compression name.
    pub fn get_compression(&self) -> Option<String> {
        let ptr = unsafe { ffi::SSL_get_current_compression(*self.ssl.ssl) };
        if ptr == ptr::null() {
            return None;
        }

        let meth = unsafe { ffi::SSL_COMP_get_name(ptr) };
        let s = unsafe {
            String::from_utf8(CStr::from_ptr(meth).to_bytes().to_vec()).unwrap()
        };

        Some(s)
    }

    /// Returns the protocol selected by performing Next Protocol Negotiation, if any.
    ///
    /// The protocol's name is returned is an opaque sequence of bytes. It is up to the client
    /// to interpret it.
    ///
    /// This method needs the `npn` feature.
    #[cfg(feature = "npn")]
    pub fn get_selected_npn_protocol(&self) -> Option<&[u8]> {
        self.ssl.get_selected_npn_protocol()
    }
}

impl<S: Read+Write> Read for SslStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.in_retry_wrapper(|ssl| { ssl.read(buf) }) {
            Ok(len) => Ok(len as usize),
            Err(SslSessionClosed) => Ok(0),
            Err(StreamError(e)) => Err(e),
            Err(e @ OpenSslErrors(_)) => {
                Err(io::Error::new(io::ErrorKind::Other, "OpenSSL error", Some(format!("{}", e))))
            }
        }
    }
}

impl<S: Read+Write> Write for SslStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.in_retry_wrapper(|ssl| ssl.write(buf)) {
            Ok(len) => Ok(len as usize),
            Err(SslSessionClosed) => Ok(0),
            Err(StreamError(e)) => return Err(e),
            Err(e @ OpenSslErrors(_)) => {
                Err(io::Error::new(io::ErrorKind::Other, "OpenSSL error", Some(format!("{}", e))))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        try!(self.write_through());
        self.stream.flush()
    }
}

/// A utility type to help in cases where the use of SSL is decided at runtime.
#[derive(Debug)]
pub enum MaybeSslStream<S> where S: Read+Write {
    /// A connection using SSL
    Ssl(SslStream<S>),
    /// A connection not using SSL
    Normal(S),
}

impl<S> Read for MaybeSslStream<S> where S: Read+Write {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            MaybeSslStream::Ssl(ref mut s) => s.read(buf),
            MaybeSslStream::Normal(ref mut s) => s.read(buf),
        }
    }
}

impl<S> Write for MaybeSslStream<S> where S: Read+Write {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            MaybeSslStream::Ssl(ref mut s) => s.write(buf),
            MaybeSslStream::Normal(ref mut s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match *self {
            MaybeSslStream::Ssl(ref mut s) => s.flush(),
            MaybeSslStream::Normal(ref mut s) => s.flush(),
        }
    }
}

impl<S> MaybeSslStream<S> where S: Read+Write {
    /// Returns a reference to the underlying stream.
    pub fn get_ref(&self) -> &S {
        match *self {
            MaybeSslStream::Ssl(ref s) => s.get_ref(),
            MaybeSslStream::Normal(ref s) => s,
        }
    }

    /// Returns a mutable reference to the underlying stream.
    ///
    /// ## Warning
    ///
    /// It is inadvisable to read from or write to the underlying stream.
    pub fn get_mut(&mut self) -> &mut S {
        match *self {
            MaybeSslStream::Ssl(ref mut s) => s.get_mut(),
            MaybeSslStream::Normal(ref mut s) => s,
        }
    }
}
