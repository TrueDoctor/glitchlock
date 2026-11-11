//! A child process that runs a single PAM authentication.
//!
//! PAM loads C modules such as `pam_fprintd` into this process, so a crash in one of them
//! only kills the helper and never the process that holds the session lock.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs::File;
use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::sync::{Mutex, OnceLock};

use crate::helper_message::HelperMessage;
use crate::secret::zero;

#[repr(C)]
struct PamMessage {
    style: c_int,
    text: *const c_char,
}

#[repr(C)]
struct PamResponse {
    text: *mut c_char,
    return_code: c_int,
}

type ConversationCallback =
    extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int;

#[repr(C)]
struct PamConversation {
    callback: ConversationCallback,
    user_data: *mut c_void,
}

enum PamHandle {}

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conversation: *const PamConversation,
        handle: *mut *mut PamHandle,
    ) -> c_int;
    fn pam_authenticate(handle: *mut PamHandle, flags: c_int) -> c_int;
}

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 19;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;

const MAX_PASSWORD_LENGTH: usize = 1024;

static USER: OnceLock<CString> = OnceLock::new();
static CONVERSATION: Mutex<()> = Mutex::new(());

/// Authenticates the current user with `service`, reports the result on stdout and exits.
pub fn run(service: &str) -> ! {
    let authenticated = authenticate(service);
    send(if authenticated {
        HelperMessage::Success
    } else {
        HelperMessage::Failure
    });

    // Modules running in parallel threads may still wait in the conversation, so `pam_end`
    // could block forever.
    // SAFETY: `_exit` is always safe to call.
    unsafe { libc::_exit(0) }
}

fn authenticate(service: &str) -> bool {
    let Some(user) = current_user_name() else {
        return false;
    };
    let Ok(service) = CString::new(service) else {
        return false;
    };
    let user = USER.get_or_init(|| user);

    let conversation = PamConversation {
        callback: converse,
        user_data: std::ptr::null_mut(),
    };
    let mut handle = std::ptr::null_mut();

    // SAFETY: All pointers are valid for the duration of the calls, PAM copies the strings
    // and `conversation` outlives the process because the process exits right after.
    unsafe {
        pam_start(service.as_ptr(), user.as_ptr(), &conversation, &mut handle) == PAM_SUCCESS
            && pam_authenticate(handle, 0) == PAM_SUCCESS
    }
}

fn current_user_name() -> Option<CString> {
    let mut buffer = vec![0 as c_char; 4096];
    // SAFETY: `passwd` is plain old data, all zeroes is a valid value.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();

    // SAFETY: All pointers are valid and `buffer.len()` is the buffer's real size.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || entry.pw_name.is_null() {
        return None;
    }

    // SAFETY: `pw_name` points to a nul terminated string inside `buffer`.
    Some(unsafe { CStr::from_ptr(entry.pw_name) }.to_owned())
}

extern "C" fn converse(
    count: c_int,
    messages: *mut *const PamMessage,
    responses: *mut *mut PamResponse,
    _: *mut c_void,
) -> c_int {
    let Ok(count) = usize::try_from(count) else {
        return PAM_CONV_ERR;
    };
    if count == 0 || messages.is_null() || responses.is_null() {
        return PAM_CONV_ERR;
    }

    // PAM frees the responses with `free`, so they must come from the C allocator.
    // SAFETY: `calloc` is safe to call with any arguments.
    let replies = unsafe { libc::calloc(count, size_of::<PamResponse>()) }.cast::<PamResponse>();
    if replies.is_null() {
        return PAM_BUF_ERR;
    }

    let _guard = CONVERSATION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    for index in 0..count {
        // SAFETY: PAM passes an array of `count` message pointers.
        let message = unsafe { *messages.add(index) };
        match answer(message) {
            // SAFETY: `replies` has room for `count` entries.
            Ok(reply) => unsafe { (*replies.add(index)).text = reply },
            Err(code) => {
                free_replies(replies, index);
                return code;
            }
        }
    }

    // SAFETY: `responses` was checked to be non-null.
    unsafe { *responses = replies };
    PAM_SUCCESS
}

fn answer(message: *const PamMessage) -> Result<*mut c_char, c_int> {
    if message.is_null() {
        return Err(PAM_CONV_ERR);
    }
    // SAFETY: PAM passes valid messages.
    let (style, text) = unsafe { ((*message).style, (*message).text) };
    let text = if text.is_null() {
        String::new()
    } else {
        // SAFETY: Message texts are nul terminated.
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    };

    match style {
        PAM_PROMPT_ECHO_OFF => {
            send(HelperMessage::Prompt);
            read_password().ok_or(PAM_CONV_ERR)
        }
        PAM_PROMPT_ECHO_ON => USER
            .get()
            .and_then(|user| duplicate(user.as_bytes()))
            .ok_or(PAM_CONV_ERR),
        PAM_ERROR_MSG => {
            send(HelperMessage::Error(text));
            Ok(std::ptr::null_mut())
        }
        PAM_TEXT_INFO => {
            send(HelperMessage::Info(text));
            Ok(std::ptr::null_mut())
        }
        _ => Err(PAM_CONV_ERR),
    }
}

/// Reads one password line from stdin without any intermediate buffering.
fn read_password() -> Option<*mut c_char> {
    // SAFETY: Stdin stays open for the lifetime of the process and is never closed here.
    let mut stdin = ManuallyDrop::new(unsafe { File::from_raw_fd(libc::STDIN_FILENO) });
    let mut password = Vec::with_capacity(MAX_PASSWORD_LENGTH);
    let mut byte = [0];

    let complete = loop {
        match stdin.read(&mut byte) {
            Ok(1) if byte == *b"\n" => break true,
            Ok(1) if byte != [0] && password.len() < MAX_PASSWORD_LENGTH => {
                password.extend_from_slice(&byte)
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            _ => break false,
        }
    };

    let copy = complete.then(|| duplicate(&password)).flatten();
    zero(&mut password);
    zero(&mut byte);
    copy
}

/// Copies bytes into a nul terminated string allocated with `malloc`.
fn duplicate(bytes: &[u8]) -> Option<*mut c_char> {
    // SAFETY: `calloc` is safe to call with any arguments.
    let copy = unsafe { libc::calloc(bytes.len() + 1, 1) }.cast::<u8>();
    if copy.is_null() {
        return None;
    }
    // SAFETY: `copy` has room for `bytes.len() + 1` bytes and does not overlap `bytes`.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), copy, bytes.len()) };
    Some(copy.cast())
}

fn free_replies(replies: *mut PamResponse, count: usize) {
    for index in 0..count {
        // SAFETY: The first `count` replies were initialized by `converse`.
        let text = unsafe { (*replies.add(index)).text };
        if !text.is_null() {
            // SAFETY: Reply texts are nul terminated strings from `duplicate`.
            let length = unsafe { libc::strlen(text) };
            // SAFETY: `text` points to `length` writable bytes.
            zero(unsafe { std::slice::from_raw_parts_mut(text.cast::<u8>(), length) });
            // SAFETY: `text` was allocated with `calloc`.
            unsafe { libc::free(text.cast()) };
        }
    }
    // SAFETY: `replies` was allocated with `calloc`.
    unsafe { libc::free(replies.cast()) };
}

fn send(message: HelperMessage) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{}", message.encode());
    let _ = stdout.flush();
}
