//! 流式文本处理工具

/// 从累积式上游文本流中提取增量 delta。
///
/// Kiro 上游按「累积全文」推送（每帧包含从头到当前的完整文本），
/// 此函数用 `previous` 记录上次全文，返回本次新增的后缀差量并更新 `previous`。
///
/// 处理四种情形：
/// - `chunk` 以 `prev` 为前缀：正常累积，返回后缀差量
/// - `prev` 以 `chunk` 为前缀：回退（重发旧内容），无新内容
/// - 二者部分重叠：找最大「prev 后缀 == chunk 前缀」重叠，返回重叠后的部分
/// - 完全不同：返回整个 `chunk`
///
/// Anthropic 与 OpenAI 两条流式路径共用（消除重复实现）。
pub(crate) fn normalize_chunk(chunk: &str, previous: &mut String) -> String {
    if chunk.is_empty() {
        return String::new();
    }

    if previous.is_empty() {
        previous.push_str(chunk);
        return chunk.to_string();
    }

    let prev = previous.as_str();

    if chunk == prev {
        return String::new();
    }

    // chunk 以 prev 开头：正常累积。上游按累积全文推送，此分支最常见；
    // 只把新增后缀 push 进 previous（复用已有分配），避免每帧重拷贝整段累积文本（O(L²)→O(L)）。
    if let Some(delta) = chunk.strip_prefix(prev) {
        let delta_owned = delta.to_string();
        previous.push_str(delta);
        return delta_owned;
    }

    // prev 以 chunk 开头：回退场景，无新内容
    if prev.starts_with(chunk) {
        return String::new();
    }

    // 寻找最大重叠：prev 的后缀与 chunk 的前缀匹配
    let max_overlap_len = prev.len().min(chunk.len());
    let mut max_overlap = 0;
    for i in chunk
        .char_indices()
        .map(|(idx, _)| idx)
        .skip(1)
        .chain(std::iter::once(chunk.len()))
    {
        if i > max_overlap_len {
            break;
        }
        if prev.ends_with(&chunk[..i]) {
            max_overlap = i;
        }
    }

    let result = if max_overlap > 0 {
        chunk[max_overlap..].to_string()
    } else {
        chunk.to_string()
    };
    *previous = chunk.to_string();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_chunk_returns_empty() {
        let mut prev = String::new();
        assert_eq!(normalize_chunk("", &mut prev), "");
    }

    #[test]
    fn first_chunk_returns_full() {
        let mut prev = String::new();
        assert_eq!(normalize_chunk("hello", &mut prev), "hello");
        assert_eq!(prev, "hello");
    }

    #[test]
    fn cumulative_returns_suffix_delta() {
        let mut prev = "hello".to_string();
        assert_eq!(normalize_chunk("hello, world", &mut prev), ", world");
        assert_eq!(prev, "hello, world");
    }

    #[test]
    fn identical_returns_empty() {
        let mut prev = "hello".to_string();
        assert_eq!(normalize_chunk("hello", &mut prev), "");
    }

    #[test]
    fn rollback_returns_empty() {
        let mut prev = "hello, world".to_string();
        assert_eq!(normalize_chunk("hello", &mut prev), "");
    }

    #[test]
    fn multibyte_overlap_without_panic() {
        let mut previous = "你好世界".to_string();
        let delta = normalize_chunk("世界🙂继续", &mut previous);
        assert_eq!(delta, "🙂继续");
        assert_eq!(previous, "世界🙂继续");
    }

    #[test]
    fn many_cumulative_frames_accumulate_correctly() {
        let full = "The quick brown fox jumps over the lazy dog 你好🙂";
        let mut previous = String::new();
        let mut reassembled = String::new();
        let mut end = 0;
        for (idx, _) in full.char_indices().skip(1).chain(std::iter::once((full.len(), ' '))) {
            if idx <= end {
                continue;
            }
            end = idx;
            let cumulative = &full[..end];
            reassembled.push_str(&normalize_chunk(cumulative, &mut previous));
        }
        assert_eq!(reassembled, full);
        assert_eq!(previous, full);
    }
}
