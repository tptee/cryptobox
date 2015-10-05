// This Source Code Form is subject to the terms of
// the Mozilla Public License, v. 2.0. If a copy of
// the MPL was not distributed with this file, You
// can obtain one at http://mozilla.org/MPL/2.0/.

use identity::Identity;
use libc::*;
use log;
use proteus::keys::{self, IdentityKeyPair, PreKey, PreKeyBundle, PreKeyId};
use proteus::message::Envelope;
use proteus::session::{DecryptError, PreKeyStore, Session};
use proteus::{self, DecodeError, EncodeError};
use std::borrow::Cow;
use std::boxed::Box;
use std::error::Error;
use std::ffi::{CStr, CString, NulError};
use std::path::Path;
use std::slice;
use std::str;
use std::u16;
use store::api::{Store, StorageError, StorageResult};
use store::file::FileStore;

/// Variant of std::try! that returns the unwrapped error.
macro_rules! try_unwrap {
    ($expr:expr) => (match $expr {
        Ok(val)  => val,
        Err(err) => return From::from(err)
    })
}

// CBoxIdentityMode /////////////////////////////////////////////////////////

#[repr(C)]
#[no_mangle]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CBoxIdentityMode {
    Complete = 0,
    Public   = 1
}

// CBox /////////////////////////////////////////////////////////////////////

#[no_mangle]
pub struct CBox {
    store: Box<Store<Error=StorageError>>,
    ident: IdentityKeyPair
}

impl CBox {
    fn session(&self, sid: &str) -> Result<Session, CBoxResult> {
        match try!(self.store.load_session(&self.ident, sid)) {
            Some(s) => Ok(s),
            None    => Err(CBoxResult::SessionNotFound)
        }
    }
}

#[no_mangle]
pub unsafe extern
fn cbox_file_open(c_path: *const c_char, c_box: *mut *mut CBox) -> CBoxResult {
    proteus::init();
    let name  = try_unwrap!(str::from_utf8(CStr::from_ptr(c_path).to_bytes()));
    let path  = Path::new(name);
    let store = try_unwrap!(FileStore::new(path));
    let ident = match try_unwrap!(store.load_identity()) {
        Some(Identity::Sec(i)) => i.into_owned(),
        Some(Identity::Pub(_)) => return CBoxResult::IdentityError,
        None => {
            let ident = IdentityKeyPair::new();
            try_unwrap!(store.save_identity(&Identity::Sec(Cow::Borrowed(&ident))));
            ident
        }
    };
    *c_box = Box::into_raw(Box::new(CBox { store: Box::new(store), ident: ident }));
    CBoxResult::Success
}

#[no_mangle]
pub unsafe extern
fn cbox_file_open_with(c_path:   *const c_char,
                       c_id:     *const uint8_t,
                       c_id_len: size_t,
                       c_mode:   CBoxIdentityMode,
                       c_box:    *mut *mut CBox) -> CBoxResult
{
    proteus::init();
    let name  = try_unwrap!(str::from_utf8(CStr::from_ptr(c_path).to_bytes()));
    let path  = Path::new(name);
    let store = try_unwrap!(FileStore::new(path));
    let ident = match try_unwrap!(dec_raw(&c_id, c_id_len as usize, Identity::deserialise)) {
        Identity::Sec(i) => i.into_owned(),
        Identity::Pub(_) => return CBoxResult::IdentityError
    };
    match try_unwrap!(store.load_identity()) {
        Some(Identity::Sec(local)) => {
            if ident.public_key != local.public_key {
                return CBoxResult::IdentityError
            }
            if c_mode == CBoxIdentityMode::Public {
                try_unwrap!(store.save_identity(&Identity::Pub(Cow::Borrowed(&ident.public_key))))
            }
        }
        Some(Identity::Pub(local)) => {
            if ident.public_key != *local {
                return CBoxResult::IdentityError
            }
            if c_mode == CBoxIdentityMode::Complete {
                try_unwrap!(store.save_identity(&Identity::Sec(Cow::Borrowed(&ident))))
            }
        }
        None => match c_mode {
            CBoxIdentityMode::Public =>
                try_unwrap!(store.save_identity(&Identity::Pub(Cow::Borrowed(&ident.public_key)))),
            CBoxIdentityMode::Complete =>
                try_unwrap!(store.save_identity(&Identity::Sec(Cow::Borrowed(&ident))))
        }
    }
    *c_box = Box::into_raw(Box::new(CBox { store: Box::new(store), ident: ident }));
    CBoxResult::Success
}

#[no_mangle]
pub unsafe extern
fn cbox_identity_copy(b: *const CBox, c_ident: *mut *mut CBoxVec) -> CBoxResult {
    let i = try_unwrap!(Identity::Sec(Cow::Borrowed(&(*b).ident)).serialise());
    *c_ident = CBoxVec::from_vec(i);
    CBoxResult::Success
}

#[no_mangle]
pub unsafe extern
fn cbox_close(b: *mut CBox) {
    Box::from_raw(b);
}

// Prekeys //////////////////////////////////////////////////////////////////

#[no_mangle]
pub static CBOX_LAST_PREKEY_ID: c_ushort = u16::MAX;

#[no_mangle]
pub unsafe extern
fn cbox_new_prekey(c_box: *mut CBox, c_id: c_ushort, c_bundle: *mut *mut CBoxVec) -> CBoxResult {
    let cbox = &*c_box;
    let pk = PreKey::new(PreKeyId::new(c_id));
    try_unwrap!(cbox.store.add_prekey(&pk));

    let bundle = try_unwrap!(PreKeyBundle::new(cbox.ident.public_key, &pk).serialise());
    *c_bundle  = CBoxVec::from_vec(bundle);

    CBoxResult::Success
}

// Session ID ///////////////////////////////////////////////////////////////

struct SID {
    string:  String,
    cstring: CString
}

impl SID {
    unsafe fn from_raw(c_sid: *const c_char) -> Result<SID, CBoxResult> {
        let st = CStr::from_ptr(c_sid).to_bytes();
        let cs = try!(CString::new(st));
        let ss = try!(str::from_utf8(cs.as_bytes()).map(String::from));
        Ok(SID { string: ss, cstring: cs })
    }

    fn as_c_ptr(&self) -> *const c_char {
        (*self.cstring).as_ptr()
    }
}

// Session //////////////////////////////////////////////////////////////////

#[no_mangle]
pub struct CBoxSession<'r> {
    cbox:   &'r mut CBox,
    sess:   Session<'r>,
    sid:    SID,
    pstore: ReadOnlyPks<'r>
}

impl<'r> CBoxSession<'r> {
    unsafe fn new(c_box: *mut CBox, sid: SID, sess: Session<'r>, ls: ReadOnlyPks<'r>) -> CBoxSession<'r> {
        CBoxSession { cbox: &mut *c_box, sess: sess, sid: sid, pstore: ls }
    }
}

struct ReadOnlyPks<'r> {
    store:       &'r mut (Store<Error=StorageError> + 'r),
    pub prekeys: Vec<PreKeyId>
}

impl<'r> ReadOnlyPks<'r> {
    pub fn new(store: &'r mut Store<Error=StorageError>) -> ReadOnlyPks {
        ReadOnlyPks { store: store, prekeys: Vec::new() }
    }
}

impl<'r> PreKeyStore for ReadOnlyPks<'r> {
    type Error = StorageError;

    fn prekey(&mut self, id: PreKeyId) -> StorageResult<Option<PreKey>> {
        if self.prekeys.contains(&id) {
            Ok(None)
        } else {
            self.store.prekey(id)
        }
    }

    fn remove(&mut self, id: PreKeyId) -> StorageResult<()> {
        self.prekeys.push(id);
        Ok(())
    }
}

#[no_mangle]
pub unsafe extern
fn cbox_session_init_from_prekey(c_box:        *mut   CBox,
                                 c_sid:        *const c_char,
                                 c_prekey:     *const uint8_t,
                                 c_prekey_len: size_t,
                                 c_session:    *mut *mut CBoxSession) -> CBoxResult
{
    let cbox   = &mut *c_box;
    let sid    = try_unwrap!(SID::from_raw(c_sid));
    let prekey = try_unwrap!(dec_raw(&c_prekey, c_prekey_len as usize, PreKeyBundle::deserialise));
    let sess   = Session::init_from_prekey(&cbox.ident, prekey);
    let pstore = ReadOnlyPks::new(&mut *cbox.store);
    let csess  = CBoxSession::new(c_box, sid, sess, pstore);
    *c_session = Box::into_raw(Box::new(csess));
    CBoxResult::Success
}

#[no_mangle]
pub unsafe extern
fn cbox_session_init_from_message(c_box:        *mut CBox,
                                  c_sid:        *const c_char,
                                  c_cipher:     *const uint8_t,
                                  c_cipher_len: size_t,
                                  c_sess:       *mut *mut CBoxSession,
                                  c_plain:      *mut *mut CBoxVec) -> CBoxResult
{
    let cbox   = &mut *c_box;
    let sid    = try_unwrap!(SID::from_raw(c_sid));
    let env    = try_unwrap!(dec_raw(&c_cipher, c_cipher_len as usize, Envelope::deserialise));
    let mut ps = ReadOnlyPks::new(&mut *cbox.store);
    let (s, p) = try_unwrap!(Session::init_from_message(&cbox.ident, &mut ps, &env));
    let csess  = CBoxSession::new(c_box, sid, s, ps);
    *c_plain   = CBoxVec::from_vec(p);
    *c_sess    = Box::into_raw(Box::new(csess));
    CBoxResult::Success
}

#[no_mangle]
pub unsafe extern
fn cbox_session_get(c_box: *mut CBox, c_sid: *const c_char, c_sess: *mut *mut CBoxSession) -> CBoxResult {
    let cbox   = &mut *c_box;
    let sid    = try_unwrap!(SID::from_raw(c_sid));
    let sess   = try_unwrap!(cbox.session(&sid.string));
    let pstore = ReadOnlyPks::new(&mut *(*c_box).store);
    let csess  = CBoxSession::new(c_box, sid, sess, pstore);
    *c_sess    = Box::into_raw(Box::new(csess));
    CBoxResult::Success
}

#[no_mangle]
pub unsafe extern
fn cbox_session_id(c_sess: *const CBoxSession) -> *const c_char {
    (*c_sess).sid.as_c_ptr()
}

#[no_mangle]
pub unsafe extern
fn cbox_session_save(c_sess: *mut CBoxSession) -> CBoxResult {
    let sess = &mut *c_sess;
    let cbox = &mut *sess.cbox;
    try_unwrap!(cbox.store.save_session(&sess.sid.string, &sess.sess));
    for k in sess.pstore.prekeys.iter() {
        try_unwrap!(cbox.store.remove(*k));
    }
    sess.pstore.prekeys.clear();
    CBoxResult::Success
}

#[no_mangle]
pub unsafe extern
fn cbox_session_close(c_sess: *mut CBoxSession) {
    Box::from_raw(c_sess);
}

#[no_mangle]
pub unsafe extern
fn cbox_session_delete(c_box: *mut CBox, c_sid: *const c_char) -> CBoxResult {
    let cbox = &*c_box;
    let sid  = try_unwrap!(SID::from_raw(c_sid));
    try_unwrap!(cbox.store.delete_session(&sid.string));
    CBoxResult::Success
}

#[no_mangle]
pub unsafe extern
fn cbox_encrypt(c_sess:      *mut CBoxSession,
                c_plain:     *const uint8_t,
                c_plain_len: size_t,
                c_cipher:    *mut *mut CBoxVec) -> CBoxResult
{
    let sref   = &mut *c_sess;
    let plain  = slice::from_raw_parts(c_plain, c_plain_len as usize);
    let cipher = try_unwrap!(sref.sess.encrypt(plain).and_then(|m| m.serialise()));
    *c_cipher  = CBoxVec::from_vec(cipher);
    CBoxResult::Success
}

#[no_mangle]
pub unsafe extern
fn cbox_decrypt(c_sess:       *mut CBoxSession,
                c_cipher:     *const uint8_t,
                c_cipher_len: size_t,
                c_plain:      *mut *mut CBoxVec) -> CBoxResult
{
    let session = &mut *c_sess;
    let env     = try_unwrap!(dec_raw(&c_cipher, c_cipher_len as usize, Envelope::deserialise));
    let plain   = try_unwrap!(session.sess.decrypt(&mut session.pstore, &env));
    *c_plain    = CBoxVec::from_vec(plain);
    CBoxResult::Success
}

#[no_mangle]
pub unsafe extern
fn cbox_fingerprint_local(c_box: *const CBox, buf: *mut *mut CBoxVec) {
    let fp = (*c_box).ident.public_key.fingerprint();
    *buf = CBoxVec::from_vec(fp.into_bytes());
}

#[no_mangle]
pub unsafe extern
fn cbox_fingerprint_remote(s: *const CBoxSession, buf: *mut *mut CBoxVec) {
    let fp = (*s).sess.remote_identity().fingerprint();
    *buf = CBoxVec::from_vec(fp.into_bytes());
}

// CBoxVec /////////////////////////////////////////////////////////////////////

#[no_mangle]
pub struct CBoxVec {
    vec: Vec<u8>
}

impl CBoxVec {
    unsafe fn from_vec(v: Vec<u8>) -> *mut CBoxVec {
        Box::into_raw(Box::new(CBoxVec { vec: v }))
    }
}

#[no_mangle]
pub unsafe extern fn cbox_vec_free(v: *mut CBoxVec) {
    Box::from_raw(v);
}

#[no_mangle]
pub unsafe extern fn cbox_vec_data(v: *const CBoxVec) -> *const uint8_t {
    (*v).vec.as_ptr()
}

#[no_mangle]
pub unsafe extern fn cbox_vec_len(v: *const CBoxVec) -> size_t {
    (*v).vec.len() as size_t
}

// CBoxResult ///////////////////////////////////////////////////////////////

#[repr(C)]
#[no_mangle]
#[derive(Clone, Copy, Debug)]
pub enum CBoxResult {
    Success               = 0,
    StorageError          = 1,
    SessionNotFound       = 2,
    DecodeError           = 3,
    RemoteIdentityChanged = 4,
    InvalidSignature      = 5,
    InvalidMessage        = 6,
    DuplicateMessage      = 7,
    TooDistantFuture      = 8,
    OutdatedMessage       = 9,
    Utf8Error             = 10,
    NulError              = 11,
    EncodeError           = 12,
    IdentityError         = 13,
    PreKeyNotFound        = 14
}

impl<E: Error> From<DecryptError<E>> for CBoxResult {
    fn from(err: DecryptError<E>) -> CBoxResult {
        match err {
            DecryptError::RemoteIdentityChanged   => CBoxResult::RemoteIdentityChanged,
            DecryptError::InvalidSignature        => CBoxResult::InvalidSignature,
            DecryptError::InvalidMessage          => CBoxResult::InvalidMessage,
            DecryptError::DuplicateMessage        => CBoxResult::DuplicateMessage,
            DecryptError::TooDistantFuture        => CBoxResult::TooDistantFuture,
            DecryptError::OutdatedMessage         => CBoxResult::OutdatedMessage,
            DecryptError::PreKeyNotFound(_)       => CBoxResult::PreKeyNotFound,
            DecryptError::PreKeyStoreError(ref e) => {
                log::error(e);
                CBoxResult::StorageError
            }
        }
    }
}

impl From<StorageError> for CBoxResult {
    fn from(e: StorageError) -> CBoxResult {
        log::error(&e);
        CBoxResult::StorageError
    }
}

impl From<str::Utf8Error> for CBoxResult {
    fn from(e: str::Utf8Error) -> CBoxResult {
        log::error(&e);
        CBoxResult::Utf8Error
    }
}

impl From<DecodeError> for CBoxResult {
    fn from(e: DecodeError) -> CBoxResult {
        log::error(&e);
        CBoxResult::DecodeError
    }
}

impl From<EncodeError> for CBoxResult {
    fn from(e: EncodeError) -> CBoxResult {
        log::error(&e);
        CBoxResult::EncodeError
    }
}

impl From<NulError> for CBoxResult {
    fn from(e: NulError) -> CBoxResult {
        log::error(&e);
        CBoxResult::NulError
    }
}

// Util /////////////////////////////////////////////////////////////////////

#[no_mangle]
pub unsafe extern
fn cbox_random_bytes(_: *const CBox, n: size_t) -> *mut CBoxVec {
    CBoxVec::from_vec(keys::rand_bytes(n as usize))
}

// Internal /////////////////////////////////////////////////////////////////

unsafe fn dec_raw<A, F>(ptr: & *const c_uchar, len: usize, f: F) -> Result<A, DecodeError>
where F: Fn(&[u8]) -> Result<A, DecodeError> {
    f(slice::from_raw_parts(*ptr, len))
}
