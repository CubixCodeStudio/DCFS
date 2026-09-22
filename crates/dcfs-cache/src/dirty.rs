//! Dirty extent tracking and merging.

use std::ops::Range;

/// Tracks dirty (modified) byte ranges within a cached file.
#[derive(Debug, Clone, Default)]
pub struct DirtyExtents {
    ranges: Vec<Range<u64>>,
}

impl DirtyExtents {
    pub fn new() -> Self {
        Self { ranges: Vec::new() }
    }

    /// Insert a dirty range, merging with overlapping/adjacent ranges.
    pub fn insert(&mut self, range: Range<u64>) {
        if range.is_empty() {
            return;
        }

        let mut merged = range;
        let mut new_ranges = Vec::new();

        for existing in &self.ranges {
            if existing.end < merged.start || existing.start > merged.end {
                // No overlap or adjacency
                new_ranges.push(existing.clone());
            } else {
                // Merge
                merged.start = merged.start.min(existing.start);
                merged.end = merged.end.max(existing.end);
            }
        }

        new_ranges.push(merged);
        new_ranges.sort_by_key(|r| r.start);
        self.ranges = new_ranges;
    }

    /// Mark a range as clean (remove it).
    pub fn clear_range(&mut self, range: Range<u64>) {
        if range.is_empty() {
            return;
        }

        let mut new_ranges = Vec::new();
        for existing in &self.ranges {
            if existing.end <= range.start || existing.start >= range.end {
                // No overlap
                new_ranges.push(existing.clone());
            } else {
                // Partial overlap: keep non-overlapping parts
                if existing.start < range.start {
                    new_ranges.push(existing.start..range.start);
                }
                if existing.end > range.end {
                    new_ranges.push(range.end..existing.end);
                }
            }
        }
        self.ranges = new_ranges;
    }

    /// Clear all dirty extents.
    pub fn clear_all(&mut self) {
        self.ranges.clear();
    }

    /// Check if any dirty extents exist.
    pub fn is_dirty(&self) -> bool {
        !self.ranges.is_empty()
    }

    /// Get all dirty ranges.
    pub fn ranges(&self) -> &[Range<u64>] {
        &self.ranges
    }

    /// Handle truncate: remove ranges beyond new size.
    pub fn truncate(&mut self, new_size: u64) {
        let mut new_ranges = Vec::new();
        for existing in &self.ranges {
            if existing.start >= new_size {
                // Entirely beyond new size: remove
            } else if existing.end > new_size {
                // Partially beyond: trim
                new_ranges.push(existing.start..new_size);
            } else {
                // Entirely within: keep
                new_ranges.push(existing.clone());
            }
        }
        self.ranges = new_ranges;
    }

    /// Handle truncate grow: mark new range as dirty.
    pub fn truncate_grow(&mut self, old_size: u64, new_size: u64) {
        if new_size > old_size {
            self.insert(old_size..new_size);
        }
    }
}

#[cfg(test)]
// Comparing `ranges()` against a slice that happens to hold one range is
// exactly what these assertions mean.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn insert_single_range() {
        let mut extents = DirtyExtents::new();
        extents.insert(10..20);
        assert_eq!(extents.ranges(), &[10..20]);
    }

    #[test]
    fn insert_overlapping_ranges() {
        let mut extents = DirtyExtents::new();
        extents.insert(10..20);
        extents.insert(15..25);
        assert_eq!(extents.ranges(), &[10..25]);
    }

    #[test]
    fn insert_adjacent_ranges() {
        let mut extents = DirtyExtents::new();
        extents.insert(10..20);
        extents.insert(20..30);
        assert_eq!(extents.ranges(), &[10..30]);
    }

    #[test]
    fn insert_non_overlapping_ranges() {
        let mut extents = DirtyExtents::new();
        extents.insert(10..20);
        extents.insert(30..40);
        assert_eq!(extents.ranges(), &[10..20, 30..40]);
    }

    #[test]
    fn clear_range_removes_overlap() {
        let mut extents = DirtyExtents::new();
        extents.insert(10..30);
        extents.clear_range(15..25);
        assert_eq!(extents.ranges(), &[10..15, 25..30]);
    }

    #[test]
    fn clear_all_empties() {
        let mut extents = DirtyExtents::new();
        extents.insert(10..20);
        extents.insert(30..40);
        extents.clear_all();
        assert!(!extents.is_dirty());
    }

    #[test]
    fn truncate_shrink() {
        let mut extents = DirtyExtents::new();
        extents.insert(10..50);
        extents.truncate(30);
        assert_eq!(extents.ranges(), &[10..30]);
    }

    #[test]
    fn truncate_grow_marks_dirty() {
        let mut extents = DirtyExtents::new();
        extents.truncate_grow(10, 30);
        assert_eq!(extents.ranges(), &[10..30]);
    }

    #[test]
    fn empty_range_ignored() {
        let mut extents = DirtyExtents::new();
        extents.insert(10..10);
        assert!(!extents.is_dirty());
    }

    #[test]
    fn clear_range_no_overlap() {
        let mut extents = DirtyExtents::new();
        extents.insert(10..20);
        extents.clear_range(30..40);
        assert_eq!(extents.ranges(), &[10..20]);
    }

    #[test]
    fn clear_range_exact_match() {
        let mut extents = DirtyExtents::new();
        extents.insert(10..20);
        extents.clear_range(10..20);
        assert!(!extents.is_dirty());
    }

    #[test]
    fn clear_range_superset() {
        let mut extents = DirtyExtents::new();
        extents.insert(10..20);
        extents.insert(30..40);
        extents.clear_range(0..100);
        assert!(!extents.is_dirty());
    }

    #[test]
    fn insert_multiple_merges() {
        let mut extents = DirtyExtents::new();
        extents.insert(0..10);
        extents.insert(20..30);
        extents.insert(40..50);
        extents.insert(5..45); // merges all three
        assert_eq!(extents.ranges(), &[0..50]);
    }

    #[test]
    fn truncate_grow_no_op() {
        let mut extents = DirtyExtents::new();
        extents.insert(0..10);
        extents.truncate_grow(20, 10); // new_size <= old_size
        assert_eq!(extents.ranges(), &[0..10]);
    }

    #[test]
    fn truncate_beyond_all_extents() {
        let mut extents = DirtyExtents::new();
        extents.insert(50..100);
        extents.truncate(10);
        assert!(!extents.is_dirty());
    }

    #[test]
    fn clear_range_empty() {
        let mut extents = DirtyExtents::new();
        extents.insert(10..20);
        extents.clear_range(10..10);
        assert_eq!(extents.ranges(), &[10..20]);
    }
}
