//! Passwords and secret keys of remembered connections: in the macOS Keychain (generic passwords, service
//! `uison-commander`, account `<connection id> <parameter>`), never in our files. Elsewhere nothing is kept
//! and they are asked again.

#[cfg(target_os = "macos")]
const SERVICE: &str = "uison-commander";

/// Can secrets be kept here at all.
pub fn available() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(target_os = "macos")]
fn account(id: &str, param: &str) -> String {
    format!("{id} {param}")
}

pub fn store(id: &str, param: &str, value: &str) -> bool {
    #[cfg(target_os = "macos")]
    return security_framework::passwords::set_generic_password(SERVICE, &account(id, param), value.as_bytes()).is_ok();
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (id, param, value);
        false
    }
}

pub fn load(id: &str, param: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    return security_framework::passwords::get_generic_password(SERVICE, &account(id, param)).ok().and_then(|b| String::from_utf8(b).ok());
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (id, param);
        None
    }
}

pub fn forget(id: &str, param: &str) {
    #[cfg(target_os = "macos")]
    let _ = security_framework::passwords::delete_generic_password(SERVICE, &account(id, param));
    #[cfg(not(target_os = "macos"))]
    let _ = (id, param);
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// Into the Keychain and back, then gone. Only when asked (`UISON_KEYCHAIN_TEST=1`): it touches the
    /// user's login keychain.
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
