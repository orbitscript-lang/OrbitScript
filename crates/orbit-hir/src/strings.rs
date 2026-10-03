//! The chunk's string pool: names and string constants, deduplicated.

use std::collections::HashMap;

use crate::StrId;

/// Byte strings (Lua strings need not be UTF-8), each stored once.
#[derive(Debug, Clone, Default)]
pub struct StringPool {
    strings: Vec<Box<[u8]>>,
    index: HashMap<Box<[u8]>, StrId>,
}

impl StringPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the id of `bytes`, adding it if it's new.
    pub fn intern(&mut self, bytes: &[u8]) -> StrId {
        if let Some(&id) = self.index.get(bytes) {
            return id;
        }
        let id = StrId::from_index(self.strings.len());
        self.strings.push(bytes.into());
        self.index.insert(bytes.into(), id);
        id
    }

    pub fn get(&self, id: StrId) -> &[u8] {
        &self.strings[id.index()]
    }

    /// The string as text, with invalid UTF-8 replaced (for messages and dumps).
    pub fn display(&self, id: StrId) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(self.get(id))
    }

    pub fn len(&self) -> usize {
        self.strings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedups() {
        let mut p = StringPool::new();
        let a = p.intern(b"x");
        let b = p.intern(b"\xffy");
        assert_eq!(p.intern(b"x"), a);
        assert_ne!(a, b);
        assert_eq!(p.get(b), b"\xffy");
        assert_eq!(p.len(), 2);
    }
}
