//! Who owns the debugger's socket directory on Windows and who its access
//! list lets in, through the safe wrappers of the `windows-permissions`
//! crate: no `unsafe` here. SIDs are compared in their string form.

use std::io;
use std::path::Path;

use windows_permissions::constants::{AceType, SeObjectType, SecurityInformation};
use windows_permissions::wrappers::{
    ConvertStringSecurityDescriptorToSecurityDescriptor, GetNamedSecurityInfo, SetNamedSecurityInfo,
};

/// `NT AUTHORITY\SYSTEM`.
const SYSTEM: &str = "S-1-5-18";
/// `BUILTIN\Administrators`, which also owns what an elevated process
/// creates.
const ADMINISTRATORS: &str = "S-1-5-32-544";
/// `CREATOR OWNER`: stands for whoever creates something inside, so it
/// lets no one else in.
const CREATOR_OWNER: &str = "S-1-3-0";

/// The SID that owns what this process creates.
#[derive(Debug)]
pub(crate) struct Owner(String);

impl Owner {
    /// The owner of a new file made in `dir` (and removed again).
    pub(crate) fn of_new_file_in(dir: &Path) -> io::Result<Owner> {
        let probe = dir.join(format!(
            "orbit-sid-{}-{:016x}",
            std::process::id(),
            crate::socket::nonce()
        ));
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        let owner = owner_of(&probe);
        drop(f);
        let _ = std::fs::remove_file(&probe);
        owner?
            .map(Owner)
            .ok_or_else(|| io::Error::other("a new file has no owner"))
    }

    /// Give `dir` an access list of its own: full control for this owner,
    /// SYSTEM and Administrators, passed on to what is made inside, and
    /// nothing inherited from the directory above.
    pub(crate) fn make_private(&self, dir: &Path) -> io::Result<()> {
        let sddl = format!(
            "D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
            self.0
        );
        let sd = ConvertStringSecurityDescriptorToSecurityDescriptor(&sddl)?;
        SetNamedSecurityInfo(
            dir.as_os_str(),
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
            None,
            None,
            sd.dacl(),
            None,
        )
    }

    /// Whether `dir` is owned by this owner (or SYSTEM or Administrators)
    /// and its access list allows no one else in. A missing access list
    /// allows everyone.
    pub(crate) fn is_private(&self, dir: &Path) -> io::Result<bool> {
        let trusted = |sid: &str| sid == self.0 || sid == SYSTEM || sid == ADMINISTRATORS;
        let sd = GetNamedSecurityInfo(
            dir.as_os_str(),
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Owner | SecurityInformation::Dacl,
        )?;
        if !sd.owner().is_some_and(|o| trusted(&o.to_string())) {
            return Ok(false);
        }
        let Some(dacl) = sd.dacl() else {
            return Ok(false);
        };
        for i in 0..dacl.len() {
            let Some(ace) = dacl.get_ace(i) else {
                return Ok(false);
            };
            let denies = matches!(
                ace.ace_type(),
                AceType::ACCESS_DENIED_ACE_TYPE
                    | AceType::ACCESS_DENIED_CALLBACK_ACE_TYPE
                    | AceType::ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE
                    | AceType::ACCESS_DENIED_OBJECT_ACE_TYPE
            );
            if denies {
                continue;
            }
            match ace.sid().map(|s| s.to_string()) {
                Some(sid) if trusted(&sid) || sid == CREATOR_OWNER => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    }
}

/// The owner SID of `path`, as a string.
fn owner_of(path: &Path) -> io::Result<Option<String>> {
    let sd = GetNamedSecurityInfo(
        path.as_os_str(),
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Owner,
    )?;
    Ok(sd.owner().map(|o| o.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_directories() {
        let base = std::env::temp_dir().join(format!(
            "orbit-acl-test-{}-{:016x}",
            std::process::id(),
            crate::socket::nonce()
        ));
        std::fs::create_dir(&base).unwrap();
        let me = Owner::of_new_file_in(&base).unwrap();
        assert!(me.0.starts_with("S-1-"), "{}", me.0);

        let dir = base.join("orbit");
        std::fs::create_dir(&dir).unwrap();
        me.make_private(&dir).unwrap();
        assert!(me.is_private(&dir).unwrap());

        // Everyone let in: refused.
        let open = format!("D:P(A;OICI;FA;;;{})(A;OICI;FA;;;WD)", me.0);
        let sd = ConvertStringSecurityDescriptorToSecurityDescriptor(&open).unwrap();
        SetNamedSecurityInfo(
            dir.as_os_str(),
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
            None,
            None,
            sd.dacl(),
            None,
        )
        .unwrap();
        assert!(!me.is_private(&dir).unwrap());

        // Another user is no owner of ours.
        let other = Owner("S-1-5-21-1-2-3-1001".to_owned());
        me.make_private(&dir).unwrap();
        assert!(!other.is_private(&dir).unwrap());

        std::fs::remove_dir_all(&base).unwrap();
    }
}
