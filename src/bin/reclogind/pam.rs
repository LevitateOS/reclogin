//! PAM for one login, used only by the session worker ([`crate::worker`]).
//!
//! The few libpam functions needed are declared here by hand, so that the
//! crate compiles on a machine without libpam's development files (the
//! superproject's CI); the per-OS build links `libpam.so`. Both OSes use
//! Linux-PAM (Fedora's on LevitateOS, Alpine's `linux-pam` on AcornOS), whose
//! numbers and structures are the ones below.
//!
//! Secrets: an answer reaches PAM as a buffer from `malloc`, which PAM frees
//! with `free` after copying it (Linux-PAM's modules overwrite their copy
//! before freeing it; this program cannot reach it after handing it over).
//! A reply that is not handed over is overwritten here before it is freed.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use reclogin::protocol::{MessageKind, PromptKind, Secret};
use zeroize::Zeroize;

pub const SUCCESS: c_int = 0;
pub const NEW_AUTHTOK_REQD: c_int = 12;
const BUF_ERR: c_int = 5;
const CONV_ERR: c_int = 19;

/// Flags.
pub const ESTABLISH_CRED: c_int = 0x0002;
pub const DELETE_CRED: c_int = 0x0004;
pub const REINITIALIZE_CRED: c_int = 0x0008;
pub const CHANGE_EXPIRED_AUTHTOK: c_int = 0x0020;

/// Items.
pub const TTY: c_int = 3;
const USER: c_int = 2;

/// Message styles.
const PROMPT_ECHO_OFF: c_int = 1;
const PROMPT_ECHO_ON: c_int = 2;
const ERROR_MSG: c_int = 3;
const TEXT_INFO: c_int = 4;

/// Linux-PAM never sends more messages in one call.
const MAX_NUM_MSG: c_int = 32;

#[repr(C)]
struct Handle {
    _opaque: [u8; 0],
}

#[repr(C)]
struct Message {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct Response {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn =
    unsafe extern "C" fn(c_int, *mut *const Message, *mut *mut Response, *mut c_void) -> c_int;

#[repr(C)]
struct Conv {
    conv: Option<ConvFn>,
    appdata_ptr: *mut c_void,
}

#[link(name = "pam")]
extern "C" {
    fn pam_start(
        service_name: *const c_char,
        user: *const c_char,
        pam_conversation: *const Conv,
        pamh: *mut *mut Handle,
    ) -> c_int;
    fn pam_end(pamh: *mut Handle, pam_status: c_int) -> c_int;
    fn pam_authenticate(pamh: *mut Handle, flags: c_int) -> c_int;
    fn pam_acct_mgmt(pamh: *mut Handle, flags: c_int) -> c_int;
    fn pam_chauthtok(pamh: *mut Handle, flags: c_int) -> c_int;
    fn pam_setcred(pamh: *mut Handle, flags: c_int) -> c_int;
    fn pam_open_session(pamh: *mut Handle, flags: c_int) -> c_int;
    fn pam_close_session(pamh: *mut Handle, flags: c_int) -> c_int;
    fn pam_set_item(pamh: *mut Handle, item_type: c_int, item: *const c_void) -> c_int;
    fn pam_get_item(pamh: *const Handle, item_type: c_int, item: *mut *const c_void) -> c_int;
    fn pam_putenv(pamh: *mut Handle, name_value: *const c_char) -> c_int;
    fn pam_getenvlist(pamh: *mut Handle) -> *mut *mut c_char;
    fn pam_strerror(pamh: *mut Handle, errnum: c_int) -> *const c_char;
}

/// The side of the conversation that talks to the person.
pub trait Conversation {
    /// A question, shown word for word. `None` abandons the conversation
    /// (the person cancelled, or the greeter went away).
    fn ask(&mut self, kind: PromptKind, text: &str) -> Option<Secret>;
    /// A message, shown word for word, in order.
    fn tell(&mut self, kind: MessageKind, text: &str);
}

/// One PAM handle, from `pam_start` to `pam_end` (on drop).
pub struct Pam<C: Conversation> {
    handle: *mut Handle,
    /// Where PAM's `appdata_ptr` points; boxed so that it never moves.
    talker: Box<C>,
    /// Linux-PAM copies the structure in `pam_start`; kept for other PAMs.
    _conv: Box<Conv>,
    /// The last result, for `pam_end`.
    last: c_int,
}

impl<C: Conversation> Pam<C> {
    pub fn start(service: &str, user: &str, talker: C) -> Result<Pam<C>, String> {
        let service = CString::new(service).map_err(|_| "service name".to_string())?;
        let user = CString::new(user).map_err(|_| "user name".to_string())?;
        let mut talker = Box::new(talker);
        let conv = Box::new(Conv {
            conv: Some(converse::<C>),
            appdata_ptr: (&mut *talker as *mut C).cast(),
        });
        let mut handle: *mut Handle = ptr::null_mut();
        let rc = unsafe { pam_start(service.as_ptr(), user.as_ptr(), &*conv, &mut handle) };
        if rc != SUCCESS || handle.is_null() {
            return Err(format!("pam_start failed ({rc})"));
        }
        Ok(Pam {
            handle,
            talker,
            _conv: conv,
            last: SUCCESS,
        })
    }

    pub fn talker(&mut self) -> &mut C {
        &mut self.talker
    }

    fn keep(&mut self, rc: c_int) -> c_int {
        self.last = rc;
        rc
    }

    pub fn authenticate(&mut self) -> c_int {
        let rc = unsafe { pam_authenticate(self.handle, 0) };
        self.keep(rc)
    }

    pub fn acct_mgmt(&mut self) -> c_int {
        let rc = unsafe { pam_acct_mgmt(self.handle, 0) };
        self.keep(rc)
    }

    pub fn chauthtok(&mut self, flags: c_int) -> c_int {
        let rc = unsafe { pam_chauthtok(self.handle, flags) };
        self.keep(rc)
    }

    pub fn setcred(&mut self, flags: c_int) -> c_int {
        let rc = unsafe { pam_setcred(self.handle, flags) };
        self.keep(rc)
    }

    pub fn open_session(&mut self) -> c_int {
        let rc = unsafe { pam_open_session(self.handle, 0) };
        self.keep(rc)
    }

    pub fn close_session(&mut self) -> c_int {
        let rc = unsafe { pam_close_session(self.handle, 0) };
        self.keep(rc)
    }

    /// Set a text item (`TTY`).
    pub fn set_item(&mut self, item: c_int, value: &str) -> c_int {
        let Ok(value) = CString::new(value) else {
            return BUF_ERR;
        };
        // PAM copies the string.
        unsafe { pam_set_item(self.handle, item, value.as_ptr().cast()) }
    }

    /// Put `name=value` into the PAM environment.
    pub fn putenv(&mut self, name: &str, value: &str) -> c_int {
        let Ok(pair) = CString::new(format!("{name}={value}")) else {
            return BUF_ERR;
        };
        unsafe { pam_putenv(self.handle, pair.as_ptr()) }
    }

    /// The user PAM settled on (a module may have changed the name).
    pub fn user(&self) -> Option<String> {
        let mut item: *const c_void = ptr::null();
        let rc = unsafe { pam_get_item(self.handle, USER, &mut item) };
        if rc != SUCCESS || item.is_null() {
            return None;
        }
        let name = unsafe { CStr::from_ptr(item.cast()) };
        name.to_str().ok().map(String::from)
    }

    /// The PAM environment, as `name=value` strings.
    pub fn env(&mut self) -> Vec<CString> {
        let mut out = Vec::new();
        unsafe {
            let list = pam_getenvlist(self.handle);
            if list.is_null() {
                return out;
            }
            let mut i = 0;
            loop {
                let entry = *list.add(i);
                if entry.is_null() {
                    break;
                }
                out.push(CStr::from_ptr(entry).to_owned());
                libc::free(entry.cast());
                i += 1;
            }
            libc::free(list.cast());
        }
        out
    }

    /// PAM's description of a result, for the system log only: it can say
    /// which part failed, so it is never shown on the screen.
    pub fn describe(&self, rc: c_int) -> String {
        let text = unsafe { pam_strerror(self.handle, rc) };
        if text.is_null() {
            return format!("PAM error {rc}");
        }
        let text = unsafe { CStr::from_ptr(text) }.to_string_lossy();
        format!("{text} ({rc})")
    }
}

impl<C: Conversation> Drop for Pam<C> {
    fn drop(&mut self) {
        unsafe {
            pam_end(self.handle, self.last);
        }
    }
}

/// PAM's conversation function: every message in order, each question
/// answered before the next message is looked at.
unsafe extern "C" fn converse<C: Conversation>(
    count: c_int,
    messages: *mut *const Message,
    out: *mut *mut Response,
    data: *mut c_void,
) -> c_int {
    if count <= 0 || count > MAX_NUM_MSG || messages.is_null() || out.is_null() || data.is_null() {
        return CONV_ERR;
    }
    let talker = &mut *(data as *mut C);
    let n = count as usize;
    let replies = libc::calloc(n, std::mem::size_of::<Response>()) as *mut Response;
    if replies.is_null() {
        return BUF_ERR;
    }
    for i in 0..n {
        // Linux-PAM passes an array of pointers to messages.
        let message = *messages.add(i);
        if message.is_null() {
            drop_replies(replies, n);
            return CONV_ERR;
        }
        let text = if (*message).msg.is_null() {
            String::new()
        } else {
            CStr::from_ptr((*message).msg)
                .to_string_lossy()
                .into_owned()
        };
        match (*message).msg_style {
            style @ (PROMPT_ECHO_OFF | PROMPT_ECHO_ON) => {
                let kind = if style == PROMPT_ECHO_OFF {
                    PromptKind::Secret
                } else {
                    PromptKind::Visible
                };
                let Some(answer) = talker.ask(kind, &text) else {
                    drop_replies(replies, n);
                    return CONV_ERR;
                };
                let bytes = answer.expose();
                if bytes.contains(&0) {
                    // A C string cannot hold it; refuse rather than cut it.
                    drop_replies(replies, n);
                    return CONV_ERR;
                }
                let buffer = libc::malloc(bytes.len() + 1) as *mut u8;
                if buffer.is_null() {
                    drop_replies(replies, n);
                    return BUF_ERR;
                }
                ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
                *buffer.add(bytes.len()) = 0;
                (*replies.add(i)).resp = buffer.cast();
                // `answer` is overwritten as it goes out of scope here.
            }
            ERROR_MSG => talker.tell(MessageKind::Error, &text),
            TEXT_INFO => talker.tell(MessageKind::Info, &text),
            // Binary and radio prompts are not Linux-PAM's text conversation.
            _ => {
                drop_replies(replies, n);
                return CONV_ERR;
            }
        }
    }
    *out = replies;
    SUCCESS
}

/// Overwrite and free replies that are not handed to PAM.
unsafe fn drop_replies(replies: *mut Response, n: usize) {
    for i in 0..n {
        let resp = (*replies.add(i)).resp;
        if !resp.is_null() {
            let len = libc::strlen(resp);
            std::slice::from_raw_parts_mut(resp.cast::<u8>(), len).zeroize();
            libc::free(resp.cast());
        }
    }
    libc::free(replies.cast());
}
