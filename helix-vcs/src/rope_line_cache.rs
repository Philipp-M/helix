//! This modules encapsulates a tiny bit of unsafe code that
//! makes diffing significantly faster and more ergonomic to implement.
//! This code is necessary because diffing requires quick random
//! access to the lines of the text that is being diffed.
//!
//! Therefore it is best to collect the `Rope::lines` iterator into a vec
//! first because access to the vec is `O(1)` where `Rope::line` is `O(log N)`.
//! However this process can allocate a (potentially quite large) vector.
//!
//! To avoid reallocation for every diff, the vector is reused.
//! However the RopeSlice references the original rope and therefore forms a self-referential data structure.
//! A transmute is used to change the lifetime of the slice to static to circumvent that project.
use std::mem::transmute;

use helix_core::{Rope, RopeSlice};

/// A cache that stores the `lines` of a rope as a vector.
/// It allows safely reusing the allocation of the vec when updating the rope
pub(crate) struct RopeLineCache {
    rope: Rope,
    lines: Vec<RopeSlice<'static>>,
}

impl RopeLineCache {
    pub fn new(rope: Rope) -> RopeLineCache {
        let mut res = RopeLineCache {
            rope,
            lines: Vec::new(),
        };
        res.update_lines();
        res
    }

    pub fn update(&mut self, rope: Rope) {
        self.lines.clear();
        self.rope = rope;
        self.update_lines();
    }

    fn update_lines(&mut self) {
        debug_assert_eq!(self.lines.len(), 0);
        // Safety: This transmute is save because it only transmutes a lifetime, which has no effect.
        // The backing storage for the RopeSlices referred to by the lifetime is stored in `self.rope`.
        // Therefore as long as `self.rope` is not dropped/replaced this memory remains valid.
        // `self.rope` is only changed `self.update`, which clear the generated slices.
        // Furthermore when these slices are exposed to consumer in `self.lines`, the lifetime is bounded to a reference to self.
        // That means that on calls to update there exist no references to the slices in `self.lines`.
        let lines = self
            .rope
            .lines()
            .map(|line: RopeSlice| -> RopeSlice<'static> { unsafe { transmute(line) } });
        self.lines.extend(lines);

        // if self
        //     .lines
        //     .last()
        //     .and_then(|last| last.as_str())
        //     .map_or(false, |last| last.is_empty())
        // {
        //     self.lines.pop();
        // }
    }

    // pub fn rope(&self) -> &Rope {
    //     &self.rope
    // }

    pub fn lines(&self) -> &[RopeSlice] {
        &self.lines
    }
}
