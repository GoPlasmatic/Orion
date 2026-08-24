//! String similarity, shared by the "did you mean" suggestions.
//!
//! Two callers want the same distance and different windows: unknown
//! `ORION_*` variables at startup ([`crate::config`]) and unknown task
//! function names at workflow validation ([`crate::engine`]). The distance is
//! here so a change to it — Damerau transposition, say, which is the typo
//! shape both callers care most about — reaches both rather than half.
//! The *window* policy stays at each call site, where it differs on purpose.

/// Levenshtein distance, two-row form.
///
/// Both callers compare short names against small candidate sets (~27
/// function names, ~110 env keys), so this is a few thousand cells on a path
/// that has already decided to reject its input.
pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    edit_distance_chars(&a, &b)
}

/// [`edit_distance`] over pre-collected chars, so a caller comparing one name
/// against many candidates collects that name once rather than per candidate.
pub(crate) fn edit_distance_chars(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != cb);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distance_counts_single_character_edits() {
        assert_eq!(edit_distance("port", "port"), 0);
        assert_eq!(edit_distance("portt", "port"), 1);
        assert_eq!(edit_distance("prot", "port"), 2);
        assert_eq!(edit_distance("", "port"), 4);
        assert_eq!(edit_distance("port", ""), 4);
    }

    /// The chars form is what the loops call; it must agree with the `&str`
    /// front door rather than being a second implementation.
    #[test]
    fn the_chars_form_agrees_with_the_str_form() {
        for (a, b) in [("http_call", "http_cal"), ("map", "x"), ("", "")] {
            let (ac, bc): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
            assert_eq!(edit_distance_chars(&ac, &bc), edit_distance(a, b));
        }
    }
}
