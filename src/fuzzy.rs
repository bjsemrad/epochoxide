use crate::types::FuzzyInfo;

pub fn mask(s: &str) -> u64 {
    s.bytes().fold(0u64, |m, b| m | (1u64 << (b as u64 % 64)))
}

pub fn score(query: &str, candidate: &str, exact: bool, field: &str) -> Option<(i32, FuzzyInfo)> {
    score_lower(&query.to_lowercase(), &candidate.to_lowercase(), exact, field)
}

/// Like `score`, but assumes `query`/`candidate` are already lowercase. Callers whose search
/// fields are pre-lowercased at index time (files/apps/runner) should call this directly and
/// lowercase `query` once per query rather than once per candidate via `score`.
pub fn score_lower(query: &str, candidate: &str, exact: bool, field: &str) -> Option<(i32, FuzzyInfo)> {
    if query.is_empty() {
        return Some((1, FuzzyInfo { start: 0, field: field.to_string(), positions: Vec::new() }));
    }

    if exact {
        let start = candidate.find(query)?;
        let positions = (start..start + query.len()).collect::<Vec<_>>();
        let score = 10_000 - start as i32 + query.len() as i32 * 100;
        return Some((score, FuzzyInfo { start, field: field.to_string(), positions }));
    }

    let mut positions = Vec::with_capacity(query.len());
    let mut needle = query.chars();
    let mut current = needle.next()?;
    for (idx, ch) in candidate.chars().enumerate() {
        if ch == current {
            positions.push(idx);
            if let Some(next) = needle.next() {
                current = next;
            } else {
                let start = positions[0];
                let span = positions.last().copied().unwrap_or(start) - start + 1;
                let compact_bonus = (100usize.saturating_sub(span) as i32).max(0);
                let prefix_bonus = if start == 0 { 500 } else { 0 };
                let score = 1_000 + query.len() as i32 * 100 + compact_bonus + prefix_bonus - start as i32;
                return Some((score, FuzzyInfo { start, field: field.to_string(), positions }));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{mask, score};

    #[test]
    fn mask_is_necessary_condition_for_match() {
        let m = mask("firefox");
        assert_eq!(mask("fox") & m, mask("fox"));
        assert_ne!(mask("zzz") & m, mask("zzz"));
    }

    #[test]
    fn fuzzy_matches_in_order() {
        let (score, info) = score("ff", "Firefox", false, "text").unwrap();
        assert!(score > 0);
        assert_eq!(info.positions, vec![0, 4]);
    }

    #[test]
    fn exact_requires_substring() {
        assert!(score("fox", "Firefox", true, "text").is_some());
        assert!(score("fx", "Firefox", true, "text").is_none());
    }
}
