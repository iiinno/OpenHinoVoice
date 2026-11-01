//! User-authored terminology. Only explicit aliases are replaced automatically;
//! descriptive hints are given to the formatter without making guesses about homophones.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Term {
    /// Preferred spelling. Used in the AI vocabulary even without any replacement aliases.
    pub canonical: String,
    /// Exact literal substrings to replace. No fuzzy matching or reapplication of replacements.
    #[serde(default)]
    pub replace_from: Vec<String>,
    /// Optional contextual explanation for the AI formatter, never a forced replacement rule.
    #[serde(default)]
    pub hint: String,
}

/// Reject conflicting or empty rules before a settings file is saved.
pub fn validate(terms: &[Term]) -> Result<(), String> {
    let mut seen = HashSet::new();
    let mut canonical_names = HashSet::new();
    for (index, term) in terms.iter().enumerate() {
        let row = index + 1;
        let canonical = term.canonical.as_str();
        if canonical.trim() != canonical || canonical.is_empty() || canonical.contains(['\r', '\n'])
        {
            return Err(format!(
                "用語辞書 {row} 行目: canonical は前後の空白・改行のない名称にしてください"
            ));
        }
        if !canonical_names.insert(canonical) {
            return Err(format!(
                "用語辞書: canonical の「{canonical}」が重複しています"
            ));
        }
        if term.hint.contains(['\r', '\n']) {
            return Err(format!("用語辞書 {row} 行目: hint に改行は指定できません"));
        }
        for source in &term.replace_from {
            if source.trim() != source || source.is_empty() || source.contains(['\r', '\n']) {
                return Err(format!(
                    "用語辞書 {row} 行目: replace_from に空文字や前後の空白・改行は指定できません"
                ));
            }
            if source == canonical {
                return Err(format!(
                    "用語辞書 {row} 行目: replace_from に canonical と同じ表記は不要です"
                ));
            }
            if !seen.insert(source) {
                return Err(format!(
                    "用語辞書: replace_from の「{source}」が重複しています"
                ));
            }
        }
    }
    Ok(())
}

/// Replace only explicitly listed strings, preferring the longest match.
/// Replacements are applied once against the original input (never recursively).
pub fn replace_explicit(input: &str, terms: &[Term]) -> String {
    let mut aliases: Vec<(&str, &str)> = terms
        .iter()
        .flat_map(|term| {
            term.replace_from
                .iter()
                .filter(|source| !source.is_empty())
                .map(move |source| (source.as_str(), term.canonical.as_str()))
        })
        .collect();
    if aliases.is_empty() || input.is_empty() {
        return input.to_owned();
    }
    aliases.sort_by_key(|a| std::cmp::Reverse(a.0.len()));

    let mut out = String::with_capacity(input.len());
    let mut remaining = input;
    while !remaining.is_empty() {
        if let Some((source, canonical)) = aliases
            .iter()
            .find(|(source, _)| remaining.starts_with(source))
        {
            out.push_str(canonical);
            remaining = &remaining[source.len()..];
        } else {
            let first = remaining.chars().next().expect("nonempty");
            out.push(first);
            remaining = &remaining[first.len_utf8()..];
        }
    }
    out
}

/// Append a bounded set of canonical names to speech.cpp's existing `words` hint.
/// Never truncate the user-supplied hint or include alias spellings that ASR might reinforce.
pub fn asr_hints(base_words: &str, terms: &[Term]) -> String {
    const MAX_ADDED_TERMS: usize = 16;
    const MAX_ADDED_CHARS: usize = 240;
    let mut out = base_words.to_owned();
    let mut added_terms = 0;
    let mut added_chars = 0;
    let mut seen: HashSet<&str> = base_words
        .split([',', '、', '\n', '\t'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect();
    for term in terms {
        let name = term.canonical.as_str();
        let length = name.chars().count();
        if name.is_empty() || name.contains([',', '、', '\n']) || length > 64 || !seen.insert(name)
        {
            continue;
        }
        if added_terms >= MAX_ADDED_TERMS || added_chars + length > MAX_ADDED_CHARS {
            break;
        }
        if !out.trim().is_empty() {
            out.push_str(", ");
        }
        out.push_str(name);
        added_terms += 1;
        added_chars += length;
    }
    out
}

/// Keep the custom formatter instructions intact, adding only an optional vocabulary note.
/// The foreground process is a hint about the destination, not a source of new text.
pub fn prompt_with_terms(base: &str, terms: &[Term], app: Option<&str>) -> String {
    let mut prompt = base.to_owned();
    if terms.is_empty() && app.is_none_or(|name| name.is_empty()) {
        return prompt;
    }
    prompt.push_str("\n\n【入力支援情報】以下は表記の参考情報です。元の発言の意味・文体・固有名詞を変えたり、発言にない情報を追加したりしないでください。曖昧な同音異義語は文脈で判断し、不明なら原文を維持してください。");
    if let Some(name) = app.filter(|name| !name.is_empty()) {
        prompt.push_str("\n入力先アプリの実行ファイル名: ");
        prompt.push_str(name);
        prompt.push_str("（文書の内容を示すものではありません）");
    }
    for term in terms {
        prompt.push_str("\n- 推奨表記: ");
        prompt.push_str(&term.canonical);
        if !term.hint.is_empty() {
            prompt.push('（');
            prompt.push_str(&term.hint);
            prompt.push('）');
        }
        if !term.replace_from.is_empty() {
            prompt.push_str(" / 別表記: ");
            prompt.push_str(&term.replace_from.join("、"));
        }
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(canonical: &str, aliases: &[&str], hint: &str) -> Term {
        Term {
            canonical: canonical.into(),
            replace_from: aliases.iter().map(|alias| (*alias).into()).collect(),
            hint: hint.into(),
        }
    }

    #[test]
    fn exact_alias_only_and_longest_match_wins() {
        let words = [
            term(
                "OpenHinoVoice",
                &["日の声", "日の声アプリ"],
                "音声入力アプリ",
            ),
            term("声", &["こえ"], ""),
        ];
        assert_eq!(
            replace_explicit("日の声アプリとこえ、日野声", &words),
            "OpenHinoVoiceと声、日野声"
        );
    }

    #[test]
    fn replacement_is_simultaneous_and_not_recursive() {
        let words = [term("B", &["A"], ""), term("C", &["B"], "")];
        assert_eq!(replace_explicit("A B", &words), "B C");
        assert_eq!(replace_explicit("日本語", &[]), "日本語");
    }

    #[test]
    fn validation_rejects_conflicting_or_empty_aliases() {
        assert!(validate(&[term("名前", &["", "呼び名"], "")]).is_err());
        assert!(validate(&[term("名前", &["名前"], "")]).is_err());
        assert!(validate(&[term("名前", &["呼び名"], ""), term("別名", &["呼び名"], "")]).is_err());
        assert!(validate(&[term("名前", &[], ""), term("名前", &[], "")]).is_err());
        assert!(validate(&[term("名称", &[], "名称の説明")]).is_ok());
    }

    #[test]
    fn formatter_hints_preserve_custom_prompt() {
        let custom = "整形は行わず、原文だけを返す";
        assert_eq!(prompt_with_terms(custom, &[], None), custom);
        let prompt = prompt_with_terms(
            custom,
            &[term("Rust", &["ラスト"], "言語名")],
            Some("Code.exe"),
        );
        assert!(prompt.starts_with(custom));
        assert!(prompt.contains("Code.exe"));
        assert!(prompt.contains("Rust（言語名） / 別表記: ラスト"));
    }

    #[test]
    fn asr_hint_keeps_original_words_and_bounds_added_terms() {
        let entries: Vec<_> = (0..32)
            .map(|i| term(&format!("専門用語{i}"), &[], ""))
            .collect();
        let output = asr_hints("重要語, 専門用語0", &entries);
        assert!(output.starts_with("重要語, 専門用語0"));
        assert_eq!(output.matches("専門用語0").count(), 1);
        assert!(!output.contains("専門用語31"));
        assert!(output.contains("専門用語16"));
        assert_eq!(asr_hints("既存単語", &[]), "既存単語");
    }
}
