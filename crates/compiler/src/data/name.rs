//! Port of `Data.Name`.
//!
//! The Haskell version is a packed UTF-8 array with hand-tuned primops.
//! We use a cheaply-clonable immutable string; interning can come later
//! if profiling calls for it.

use std::fmt;
use std::rc::Rc;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(Rc<str>);

impl Name {
    pub fn from_str(s: &str) -> Name {
        Name(Rc::from(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_upper(&self) -> bool {
        self.0.chars().next().is_some_and(|c| c.is_uppercase())
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", &*self.0)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Name {
    fn from(s: &str) -> Name {
        Name::from_str(s)
    }
}

impl From<String> for Name {
    fn from(s: String) -> Name {
        Name(Rc::from(s.as_str()))
    }
}

impl std::ops::Deref for Name {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}
