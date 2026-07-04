//! Port of `Reporting.Annotation`.
//!
//! The Haskell compiler packs positions into a Word64 for memory density.
//! We keep row/col as plain u32 fields; Rust structs make this free.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    pub row: u32,
    pub col: u32,
}

impl Position {
    pub const fn new(row: u32, col: u32) -> Position {
        Position { row, col }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Region {
    pub start: Position,
    pub end: Position,
}

impl Region {
    pub const fn new(start: Position, end: Position) -> Region {
        Region { start, end }
    }

    pub const ZERO: Region = Region {
        start: Position::new(0, 0),
        end: Position::new(0, 0),
    };

    /// Port of `mergeRegions`: spans from the start of the first to the end of the second.
    pub fn merge(self, other: Region) -> Region {
        Region {
            start: self.start,
            end: other.end,
        }
    }
}

/// Port of `Located a` (the `At` constructor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located<T> {
    pub region: Region,
    pub value: T,
}

impl<T> Located<T> {
    pub fn new(region: Region, value: T) -> Located<T> {
        Located { region, value }
    }

    pub fn at(start: Position, end: Position, value: T) -> Located<T> {
        Located {
            region: Region::new(start, end),
            value,
        }
    }

}
