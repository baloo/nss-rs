#[macro_use]
extern crate lazy_static;
extern crate libc;
extern crate nss_sys;
pub mod cert;
pub mod error;
pub mod nspr;

use libc::c_void;
use nss_sys as ffi;
use std::borrow::Borrow;
use std::cmp;
use std::ffi::CStr;
use std::mem;
use std::ops::{Deref,DerefMut};
use std::ptr;
use std::slice;

pub use error::{Error, Result, failed};
pub use nspr::fd::{File, FileMethods, FileWrapper};
pub use cert::{Certificate, CertList};
use nspr::fd::{RawFile, BorrowedFile};
use nspr::{bool_from_nspr, bool_to_nspr};
use error::PR_WOULD_BLOCK_ERROR;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ErrorCode(ffi::nspr::PRErrorCode);

fn result_secstatus(status: ffi::SECStatus) -> Result<()> {
    // Must call this immediately after the NSS operation so that the
    // thread-local error state isn't stale.
    match status {
        ffi::SECSuccess => Ok(()),
        ffi::SECFailure => failed(),
        ffi::SECWouldBlock => Err(PR_WOULD_BLOCK_ERROR.into()),
    }
}

fn result_bool_getter<F: FnOnce(*mut ffi::nspr::PRBool) -> ffi::SECStatus>(f: F) -> Result<bool> {
    // Poison this with a bad value; bool_from_nspr will panic if it's still there.
    let mut value: ffi::nspr::PRBool = 0x5a;
    try!(result_secstatus(f(&mut value as *mut _)));
    Ok(bool_from_nspr(value))
}

// TODO: What do I do about this init/shutdown stuff vs. lifetimes/safety?

pub fn init() -> Result<()> {
    nspr::init();
    result_secstatus(unsafe { ffi::NSS_NoDB_Init(ptr::null()) })
}

// Caller must ensure this isn't one of the SECItems where the length
// is actually bits instead of bytes.
pub unsafe fn sec_item_as_slice(item: &ffi::SECItem) -> &[u8] {
    slice::from_raw_parts(item.data, item.len as usize)
}

// This is a newtype so that it can have traits on it.
pub struct TLSSocket<Callbacks>(Box<TLSSocketImpl<Callbacks>>);
// This isn't a newtype so that Deref etc. can return it.
pub type BorrowedTLSSocket<'a, Callbacks> = &'a TLSSocketImpl<Callbacks>;

pub struct TLSSocketImpl<Callbacks> {
    file: File,
    callbacks: Callbacks,
}

impl<Callbacks> Deref for TLSSocket<Callbacks> {
    type Target = TLSSocketImpl<Callbacks>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<Callbacks> DerefMut for TLSSocket<Callbacks> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<Callbacks> Deref for TLSSocketImpl<Callbacks> {
    type Target = File;
    fn deref(&self) -> &File {
        &self.file
    }
}
// DerefMut would be unsound -- could shuffle sockets holding pointers to callbacks

// So that there's a type inhabited by both File and TLSSocket<_>.
impl<Callbacks> Borrow<File> for TLSSocket<Callbacks> {
    fn borrow(&self) -> &File {
        self
    }
}

impl<Callbacks> TLSSocket<Callbacks> {
    pub fn new(inner: File, callbacks: Callbacks) -> Result<Self> {
        Self::new_with_model(inner, callbacks, None)
    }
    pub fn new_with_model(inner: File, callbacks: Callbacks, model: Option<Self>) -> Result<Self>
    {
        if let Some(_) = model {
            // This will copy the callbacks; need to unset or fix them.
            unimplemented!();
        }
        let raw_model = model.map_or(nspr::fd::null(), |fd| fd.as_raw_prfd());
        unsafe {
            let raw = ffi::SSL_ImportFD(raw_model, inner.as_raw_prfd());
            let file = try!(File::from_raw_prfd_err(raw));
            mem::forget(inner);
            Ok(TLSSocket(Box::new(TLSSocketImpl {
                file: file,
                callbacks: callbacks,
            })))
        }
    }

    pub fn use_auth_certificate_hook(&mut self) -> Result<()>
        where Callbacks: AuthCertificateHook
    {
        let this: BorrowedTLSSocket<_> = &*self;
        result_secstatus(unsafe {
            ffi::SSL_AuthCertificateHook(self.as_raw_prfd(),
                                         Some(raw_auth_certificate_hook::<Callbacks>),
                                         mem::transmute(this))
        })
    }
}

impl<Callbacks> TLSSocketImpl<Callbacks> {
    pub fn callbacks(&self) -> &Callbacks {
        &self.callbacks
    }
    // callbacks_mut would be sound, but would anything use it?

    pub fn peer_cert(&self) -> Option<Certificate> {
        unsafe { 
            Certificate::from_raw_ptr_opt(ffi::SSL_PeerCertificate(self.as_raw_prfd()))
        }
    }

    pub fn peer_cert_chain(&self) -> Option<CertList> {
        unsafe {
            CertList::from_raw_ptr_opt(ffi::SSL_PeerCertificateChain(self.as_raw_prfd()))
        }
    }

    pub fn cleartext(&self) -> BorrowedFile {
        unsafe {
            BorrowedFile::from_raw_prfd((*self.as_raw_prfd()).lower)
        }
    }

    pub fn set_url(&self, url: &CStr) -> Result<()> {
        result_secstatus(unsafe {
            ffi::SSL_SetURL(self.as_raw_prfd(), url.as_ptr())
        })
    }

    pub fn unset_bad_cert_hook(&mut self) -> Result<()> {
        // This doesn't take locks in the C code, so needs a unique ref.
        result_secstatus(unsafe {
            ffi::SSL_BadCertHook(self.as_raw_prfd(), None, ptr::null_mut())
        })
    }

    pub fn unset_auth_certificate_hook(&mut self) -> Result<()> {
        result_secstatus(unsafe {
            ffi::SSL_AuthCertificateHook(self.as_raw_prfd(), None, ptr::null_mut())
        })
    }

    // FIXME: turn this into an actual callback now that that's possible?
    pub fn disable_security(&mut self) -> Result<()> {
        unsafe extern "C" fn this_is_fine(_arg: *mut c_void, _fd: RawFile) -> ffi::SECStatus {
            ffi::SECSuccess
        }
        result_secstatus(unsafe {
            ffi::SSL_BadCertHook(self.as_raw_prfd(), Some(this_is_fine), ptr::null_mut())
        })
    }

    pub fn set_option(&self, option: TLSOption, value: bool) -> Result<()> {
        result_secstatus(unsafe {
            ffi::SSL_OptionSet(self.as_raw_prfd(), option.to_ffi(), bool_to_nspr(value))
        })
    }

    pub fn get_option(&self, option: TLSOption) -> Result<bool> {
        result_bool_getter(|bptr| unsafe {
            ffi::SSL_OptionGet(self.as_raw_prfd(), option.to_ffi(), bptr)
        })
    }

    pub fn set_version_range(&self, min: TLSVersion, max: TLSVersion) -> Result<()> {
        let range = ffi::SSLVersionRange {
            min: min.to_ffi(),
            max: max.to_ffi(),
        };
        result_secstatus(unsafe {
            ffi::SSL_VersionRangeSet(self.as_raw_prfd(), &range as *const _)
        })
    }


    pub fn get_version_range(&self) -> Result<(TLSVersion, TLSVersion)> {
        let mut range = ffi::SSLVersionRange { min: 0xffff, max: 0 };
        try!(result_secstatus(unsafe {
            ffi::SSL_VersionRangeSet(self.as_raw_prfd(), &mut range as *mut _)
        }));
        Ok((TLSVersion(range.min), TLSVersion(range.max)))
    }

    pub fn limit_version(&self, min: Option<TLSVersion>, max: Option<TLSVersion>) -> Result<()> {
        let (abs_min, abs_max) = try!(TLSVersion::supported_range());
        self.set_version_range(min.map_or(abs_min, |min| cmp::max(min, abs_min)),
                               max.map_or(abs_max, |max| cmp::min(max, abs_max)))
    }

    pub fn set_ciphersuite_enabled(&self, suite: TLSCipherSuite, enabled: bool) -> Result<()> {
        result_secstatus(unsafe {
            ffi::SSL_CipherPrefSet(self.as_raw_prfd(), suite.to_ffi(), bool_to_nspr(enabled))
        })
    }

    pub fn is_ciphersuite_enabled(&self, suite: TLSCipherSuite) -> Result<bool> {
        result_bool_getter(|bptr| unsafe {
            ffi::SSL_CipherPrefGet(self.as_raw_prfd(), suite.to_ffi(), bptr)
        })
    }
}

pub trait AuthCertificateHook: Sized {
    fn auth_certificate(&self, sock: BorrowedTLSSocket<Self>, check_sig: bool, is_server: bool)
                        -> Result<()>;
}

unsafe extern "C" fn raw_auth_certificate_hook<Callbacks>(arg: *mut c_void,
                                                          fd: *mut ffi::nspr::PRFileDesc,
                                                          check_sig: ffi::nspr::PRBool,
                                                          is_server: ffi::nspr::PRBool)
                                                          -> ffi::SECStatus
    where Callbacks: AuthCertificateHook
{
    // TODO: check identity?
    let this: BorrowedTLSSocket<Callbacks> = mem::transmute(arg);
    assert_eq!(this.as_raw_prfd(), fd);
    match this.callbacks.auth_certificate(this,
                                          bool_from_nspr(check_sig),
                                          bool_from_nspr(is_server)) {
        Ok(()) => ffi::SECSuccess,
        Err(err) => { err.set(); ffi::SECFailure }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TLSOption(ffi::nspr::PRInt32);

impl TLSOption {
    pub fn to_ffi(self) -> ffi::nspr::PRInt32 { self.0 }
}

macro_rules! def_options {{ $($name:ident,)* } => {
    $(pub const $name: TLSOption = TLSOption(ffi::$name);)*
}}

def_options! {
    SSL_SECURITY,
    SSL_SOCKS,
    SSL_REQUEST_CERTIFICATE,
    SSL_HANDSHAKE_AS_CLIENT,
    SSL_HANDSHAKE_AS_SERVER,
    SSL_ENABLE_SSL2,
    SSL_ENABLE_SSL3,
    SSL_NO_CACHE,
    SSL_REQUIRE_CERTIFICATE,
    SSL_ENABLE_FDX,
    SSL_V2_COMPATIBLE_HELLO,
    SSL_ENABLE_TLS,
    SSL_ROLLBACK_DETECTION,
    SSL_NO_STEP_DOWN,
    SSL_BYPASS_PKCS11,
    SSL_NO_LOCKS,
    SSL_ENABLE_SESSION_TICKETS,
    SSL_ENABLE_DEFLATE,
    SSL_ENABLE_RENEGOTIATION,
    SSL_REQUIRE_SAFE_NEGOTIATION,
    SSL_ENABLE_FALSE_START,
    SSL_CBC_RANDOM_IV,
    SSL_ENABLE_OCSP_STAPLING,
    SSL_ENABLE_NPN,
    SSL_ENABLE_ALPN,
    SSL_REUSE_SERVER_ECDHE_KEY,
    SSL_ENABLE_FALLBACK_SCSV,
    SSL_ENABLE_SERVER_DHE,
    SSL_ENABLE_EXTENDED_MASTER_SECRET,
    SSL_ENABLE_SIGNED_CERT_TIMESTAMPS,
    SSL_REQUIRE_DH_NAMED_GROUPS,
    SSL_ENABLE_0RTT_DATA,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TLSVersion(ffi::nspr::PRUint16);

impl TLSVersion {
    pub fn to_ffi(self) -> ffi::nspr::PRUint16 { self.0 }
    pub fn supported_range() -> Result<(Self, Self)> {
        let mut range = ffi::SSLVersionRange { min: 0xffff, max: 0 };
        try!(result_secstatus(unsafe {
            ffi::SSL_VersionRangeGetSupported(ffi::ssl_variant_stream, &mut range as *mut _)
        }));
        Ok((TLSVersion(range.min), TLSVersion(range.max)))
    }
}

pub const SSL_VERSION_2: TLSVersion = TLSVersion(ffi::SSL_LIBRARY_VERSION_2);
pub const SSL_VERSION_3: TLSVersion = TLSVersion(ffi::SSL_LIBRARY_VERSION_3_0);
pub const TLS_VERSION_1_0: TLSVersion = TLSVersion(ffi::SSL_LIBRARY_VERSION_TLS_1_0);
pub const TLS_VERSION_1_1: TLSVersion = TLSVersion(ffi::SSL_LIBRARY_VERSION_TLS_1_1);
pub const TLS_VERSION_1_2: TLSVersion = TLSVersion(ffi::SSL_LIBRARY_VERSION_TLS_1_2);
pub const TLS_VERSION_1_3: TLSVersion = TLSVersion(ffi::SSL_LIBRARY_VERSION_TLS_1_3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TLSCipherSuite(ffi::nspr::PRUint16);

impl TLSCipherSuite {
    // No, I don't know why the functions take what's really a u16 as an i32.
    pub fn to_ffi(self) -> ffi::nspr::PRInt32 { self.0 as ffi::nspr::PRInt32 }
    pub fn implemented() -> &'static [Self] {
        unsafe {
            slice::from_raw_parts(ffi::SSL_GetImplementedCiphers() as *const Self,
                                  ffi::SSL_GetNumImplementedCiphers() as usize)
        }
    }
    pub fn is_default_enabled(&self) -> Result<bool> {
        result_bool_getter(|bptr| unsafe {
            ffi::SSL_CipherPrefGetDefault(self.to_ffi(), bptr)
        })
    }
}

macro_rules! def_ciphers {{ $($name:ident,)* } => {
    $(pub const $name: TLSCipherSuite = TLSCipherSuite(ffi::$name);)*
}}

// Just wrap the ciphersuites that are currently actually implemented
// by NSS, not everything defined in nss-sys.  The actual library, if
// it's an older version, might implement some others, but they almost
// certainly aren't safe to use.
def_ciphers! {
    TLS_AES_128_GCM_SHA256,
    TLS_CHACHA20_POLY1305_SHA256,
    TLS_AES_256_GCM_SHA384,

    TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
    TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
    TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA,
    TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA,
    TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA,
    TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256,
    TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256,
    TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA,
    TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384,
    TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384,
    TLS_ECDHE_ECDSA_WITH_3DES_EDE_CBC_SHA,
    TLS_ECDHE_RSA_WITH_3DES_EDE_CBC_SHA,
    TLS_ECDHE_ECDSA_WITH_RC4_128_SHA,
    TLS_ECDHE_RSA_WITH_RC4_128_SHA,

    TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
    TLS_DHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    TLS_DHE_DSS_WITH_AES_128_GCM_SHA256,
    TLS_DHE_RSA_WITH_AES_256_GCM_SHA384,
    TLS_DHE_DSS_WITH_AES_256_GCM_SHA384,
    TLS_DHE_RSA_WITH_AES_128_CBC_SHA,
    TLS_DHE_DSS_WITH_AES_128_CBC_SHA,
    TLS_DHE_RSA_WITH_AES_128_CBC_SHA256,
    TLS_DHE_DSS_WITH_AES_128_CBC_SHA256,
    TLS_DHE_RSA_WITH_CAMELLIA_128_CBC_SHA,
    TLS_DHE_DSS_WITH_CAMELLIA_128_CBC_SHA,
    TLS_DHE_RSA_WITH_AES_256_CBC_SHA,
    TLS_DHE_DSS_WITH_AES_256_CBC_SHA,
    TLS_DHE_RSA_WITH_AES_256_CBC_SHA256,
    TLS_DHE_DSS_WITH_AES_256_CBC_SHA256,
    TLS_DHE_RSA_WITH_CAMELLIA_256_CBC_SHA,
    TLS_DHE_DSS_WITH_CAMELLIA_256_CBC_SHA,
    TLS_DHE_RSA_WITH_3DES_EDE_CBC_SHA,
    TLS_DHE_DSS_WITH_3DES_EDE_CBC_SHA,
    TLS_DHE_DSS_WITH_RC4_128_SHA,

    TLS_ECDH_ECDSA_WITH_AES_128_CBC_SHA,
    TLS_ECDH_RSA_WITH_AES_128_CBC_SHA,
    TLS_ECDH_ECDSA_WITH_AES_256_CBC_SHA,
    TLS_ECDH_RSA_WITH_AES_256_CBC_SHA,
    TLS_ECDH_ECDSA_WITH_3DES_EDE_CBC_SHA,
    TLS_ECDH_RSA_WITH_3DES_EDE_CBC_SHA,
    TLS_ECDH_ECDSA_WITH_RC4_128_SHA,
    TLS_ECDH_RSA_WITH_RC4_128_SHA,

    TLS_RSA_WITH_AES_128_GCM_SHA256,
    TLS_RSA_WITH_AES_256_GCM_SHA384,
    TLS_RSA_WITH_AES_128_CBC_SHA,
    TLS_RSA_WITH_AES_128_CBC_SHA256,
    TLS_RSA_WITH_CAMELLIA_128_CBC_SHA,
    TLS_RSA_WITH_AES_256_CBC_SHA,
    TLS_RSA_WITH_AES_256_CBC_SHA256,
    TLS_RSA_WITH_CAMELLIA_256_CBC_SHA,
    TLS_RSA_WITH_SEED_CBC_SHA,
    TLS_RSA_WITH_3DES_EDE_CBC_SHA,
    TLS_RSA_WITH_RC4_128_SHA,
    TLS_RSA_WITH_RC4_128_MD5,

    TLS_DHE_RSA_WITH_DES_CBC_SHA,
    TLS_DHE_DSS_WITH_DES_CBC_SHA,
    TLS_RSA_WITH_DES_CBC_SHA,

    TLS_ECDHE_ECDSA_WITH_NULL_SHA,
    TLS_ECDHE_RSA_WITH_NULL_SHA,
    TLS_ECDH_RSA_WITH_NULL_SHA,
    TLS_ECDH_ECDSA_WITH_NULL_SHA,
    TLS_RSA_WITH_NULL_SHA,
    TLS_RSA_WITH_NULL_SHA256,
    TLS_RSA_WITH_NULL_MD5,
}

#[cfg(test)]
mod tests {
    use super::*;
    use error::{PR_NOT_CONNECTED_ERROR, PR_IS_CONNECTED_ERROR, PR_END_OF_FILE_ERROR};
    use std::net::{SocketAddr,SocketAddrV4,Ipv4Addr};
    use std::sync::{Arc, Mutex};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn just_init() {
        init().unwrap();
    }

    #[test]
    fn handshake() {
        fn fake_addr() -> SocketAddr {
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 443))
        }

        struct FakeSocket {
            connected: AtomicBool,
            written: Arc<Mutex<Vec<u8>>>,
        }

        impl FakeSocket {
            fn new() -> Self {
                FakeSocket {
                    connected: AtomicBool::new(false),
                    written: Arc::new(Mutex::new(Vec::new())),
                }
            }
        }

        impl FileMethods for FakeSocket {
            fn read(&self, _buf: &mut[u8]) -> Result<usize> {
                Ok(0)
            }
            fn write(&self, buf: &[u8]) -> Result<usize> {
                self.written.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn send(&self, buf: &[u8], _timeout: Option<Duration>) -> Result<usize> {
                self.write(buf)
            }
            fn recv(&self, buf: &mut [u8], _peek: bool, _timeout: Option<Duration>) -> Result<usize>
            {
                self.read(buf)
            }
            fn getpeername(&self) -> Result<SocketAddr> {
                if self.connected.load(Ordering::SeqCst) {
                    Ok(fake_addr())
                } else {
                    Err(PR_NOT_CONNECTED_ERROR.into())
                }
            }
            fn connect(&self, addr: SocketAddr, _timeout: Option<Duration>) -> Result<()> {
                assert_eq!(addr, fake_addr());
                if self.connected.swap(true, Ordering::SeqCst) {
                    // Shouldn't be used but might as well:
                    Err(PR_IS_CONNECTED_ERROR.into())
                } else {
                    Ok(())
                }
            }
        }

        init().unwrap();
        let inner = FakeSocket::new();
        let buf = inner.written.clone();
        let sock_factory = FileWrapper::new(nspr::fd::PR_DESC_SOCKET_TCP);
        let sock = sock_factory.wrap(inner);
        let ssl = TLSSocket::new(sock, ()).unwrap();
        ssl.connect(fake_addr(), None).unwrap();
        assert_eq!(ssl.write(&[]).unwrap_err().nspr_error, PR_END_OF_FILE_ERROR);
        println!("DATA: {:?}", &buf.lock().unwrap()[..]);
    }
}
