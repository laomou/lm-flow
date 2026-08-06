//! 建图期诊断辅助：稳定路径与拼写建议。

pub(crate) fn suggestion<'a, I>(value: &str, candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    if value.is_empty() {
        return None;
    }
    candidates
        .into_iter()
        .map(|candidate| (edit_distance(value, candidate), candidate))
        .filter(|(distance, candidate)| {
            let limit = if candidate.len().max(value.len()) <= 4 {
                1
            } else {
                2
            };
            *distance <= limit
        })
        .min_by(|(left_distance, left), (right_distance, right)| {
            left_distance
                .cmp(right_distance)
                .then_with(|| left.cmp(right))
        })
        .map(|(_, candidate)| candidate)
}

pub(crate) fn did_you_mean<'a, I>(value: &str, candidates: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    suggestion(value, candidates)
        .map(|candidate| format!("; did you mean `{candidate}`?"))
        .unwrap_or_default()
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];

    for (left_index, left_char) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_char) in right.iter().enumerate() {
            current[right_index + 1] = if left_char == *right_char {
                previous[right_index]
            } else {
                1 + previous[right_index]
                    .min(previous[right_index + 1])
                    .min(current[right_index])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_only_close_names() {
        assert_eq!(
            suggestion("metdata", ["video", "metadata", "control"]),
            Some("metadata")
        );
        assert_eq!(suggestion("unrelated", ["video", "metadata"]), None);
    }
}
