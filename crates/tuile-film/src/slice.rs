// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

/// Splits `first..=last` into at most `parts` contiguous ranges.
///
/// The farm's rule (`tuile-farm`'s `process_ranges`): equal spans, the
/// remainder on the last one. Fewer frames than parts is a small film, not an
/// error, so the number of ranges is capped by the number of frames.
pub fn slice(first: u32, last: u32, parts: u32) -> Vec<(u32, u32)> {
    if last < first || parts == 0 {
        return Vec::new();
    }
    let total = last - first + 1;
    let parts = parts.min(total);
    let span = total / parts;
    (0..parts)
        .map(|i| {
            let a = first + i * span;
            let b = if i == parts - 1 { last } else { a + span - 1 };
            (a, b)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_cover_every_frame_once() {
        let r = slice(1, 64, 3);
        assert_eq!(r, vec![(1, 21), (22, 42), (43, 64)]);
    }

    #[test]
    fn fewer_frames_than_parts() {
        assert_eq!(slice(5, 6, 4), vec![(5, 5), (6, 6)]);
    }

    #[test]
    fn nothing_to_slice() {
        assert!(slice(3, 2, 4).is_empty());
        assert!(slice(1, 2, 0).is_empty());
    }
}
