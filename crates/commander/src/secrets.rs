//! Passwords and secret keys of remembered connections: in the macOS Keychain (generic passwords, service
//! `uison-commander`, account `<connection id> <parameter>`) or the Windows Credential Manager (generic
//! credentials `uison-commander:<connection id> <parameter>`), never in our files. Elsewhere nothing is kept
//! and they are asked again.

#[cfg(any(target_os = "macos", windows))]
const SERVICE: &str = "uison-commander";

/// The texts that name the store: the Keychain on a Mac, the Credential Manager on Windows (English; `t!`).
pub const REMEMBER: &str = if cfg!(windows) { "Remember the password in the Credential Manager" } else { "Remember the password in the Keychain" };
pub const KEPT_HINT: &str = if cfg!(windows) { "empty — the one in the Credential Manager" } else { "empty — the one in the Keychain" };
pub const KEPT_NOTE: &str = if cfg!(windows) {
    "Enter or a double click connects. Passwords are kept in the Credential Manager."
} else {
    "Enter or a double click connects. Passwords are kept in the Keychain."
};

/// Can secrets be kept here at all.
pub fn available() -> bool {
    cfg!(any(target_os = "macos", windows))
}

#[cfg(any(target_os = "macos", windows))]
fn account(id: &str, param: &str) -> String {
    format!("{id} {param}")
}

/// The Credential Manager's name of a secret: the program's, then the account.
#[cfg(windows)]
fn target(id: &str, param: &str) -> String {
    format!("{SERVICE}:{}", account(id, param))
}

pub fn store(id: &str, param: &str, value: &str) -> bool {
    #[cfg(target_os = "macos")]
    return security_framework::passwords::set_generic_password(SERVICE, &account(id, param), value.as_bytes()).is_ok();
    #[cfg(windows)]
    return crate::windows::cred_write(&target(id, param), &account(id, param), value);
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (id, param, value);
        false
    }
}

pub fn load(id: &str, param: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    return security_framework::passwords::get_generic_password(SERVICE, &account(id, param)).ok().and_then(|b| String::from_utf8(b).ok());
    #[cfg(windows)]
    return crate::windows::cred_read(&target(id, param));
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (id, param);
        None
    }
}

pub fn forget(id: &str, param: &str) {
    #[cfg(target_os = "macos")]
    let _ = security_framework::passwords::delete_generic_password(SERVICE, &account(id, param));
    #[cfg(windows)]
    crate::windows::cred_delete(&target(id, param));
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = (id, param);
}

#[cfg(all(test, any(target_os = "macos", windows)))]
mod tests {
    /// Into the Keychain (the Credential Manager) and back, then gone. Only when asked (`UISON_KEYCHAIN_TEST=1`):
    /// it touches the user's own store.
    #[test]
    fn keychain_round_trip() {
        if std::env::var("UISON_KEYCHAIN_TEST").is_err() {
            return eprintln!("UISON_KEYCHAIN_TEST is not set: skipped");
        }
        let id = format!("test://{}", std::process::id());
        assert!(super::store(&id, "password", "pa ss"));
        assert_eq!(super::load(&id, "password").as_deref(), Some("pa ss"));
        super::forget(&id, "password");
        assert_eq!(super::load(&id, "password"), None);
    }
}
