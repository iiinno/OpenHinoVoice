//! 接続先のプリセットの表。設定の provider の文字列は既存の値のまま使うので、
//! 古い config.toml のままでも起動できる。
//! ローカル LLM は特定のエンジンに縛らない。OpenAI 互換の API を話すエンジンなら
//! どれでもつなげられる。エンジン独自の API や名前に依存するコードは書かない

/// 接続先 1 行分の情報。設定画面の選択肢と、選んだときの初期値を 1 か所にまとめる
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// 設定の provider に入る文字列。"custom" と "local" は URL を利用者が変えられる
    pub id: &'static str,
    /// 設定画面に出す表示名
    pub label: &'static str,
    /// 初期の base_url。"custom" と "local" では初期値
    pub base_url: &'static str,
    /// API キーが必須か
    pub needs_key: bool,
    /// 初期のモデル名
    pub default_model: &'static str,
}

const PRESETS: &[Preset] = &[
    Preset {
        id: "opencode-go",
        label: "OpenCode Go",
        base_url: "https://opencode.ai/inference/go/openai/v1",
        needs_key: true,
        default_model: "deepseek-v4.1-flash",
    },
    Preset {
        id: "chatgpt",
        label: "ChatGPT のプラン(Plus、Pro)",
        base_url: "",
        needs_key: false,
        default_model: "gpt-6-luna",
    },
    Preset {
        id: "openai",
        label: "OpenAI の API キー",
        base_url: "https://api.openai.com/v1",
        needs_key: true,
        default_model: "gpt-6-luna",
    },
    Preset {
        id: "anthropic",
        label: "Anthropic(Claude)の API キー",
        base_url: "https://api.anthropic.com/v1",
        needs_key: true,
        default_model: "claude-haiku-5-5",
    },
    Preset {
        id: "gemini",
        label: "Google Gemini",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        needs_key: true,
        default_model: "gemini-2.5-flash",
    },
    Preset {
        id: "openrouter",
        label: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        needs_key: true,
        default_model: "openrouter/auto",
    },
    Preset {
        id: "groq",
        label: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        needs_key: true,
        default_model: "llama-3.3-70b-versatile",
    },
    Preset {
        id: "local",
        label: "この PC の LLM(OpenAI 互換)",
        base_url: "http://127.0.0.1:8080/v1",
        needs_key: false,
        default_model: "",
    },
    Preset {
        id: "custom",
        label: "その他の OpenAI 互換サービス",
        base_url: "",
        needs_key: true,
        default_model: "",
    },
];

/// 設定画面の選択肢の順番
pub fn all() -> &'static [Preset] {
    PRESETS
}

pub fn find(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// URL がこの PC(localhost)向きか。オフラインモードの許可と、
/// ローカル LLM へのモデル読み込みの判定に使う
pub fn is_local_url(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(parsed) => matches!(
            parsed.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
        ),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_cover_known_providers_and_keep_old_ids() {
        let ids: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        for old in ["opencode-go", "chatgpt", "openai", "custom"] {
            assert!(ids.contains(&old), "既存の provider を消さない: {old}");
        }
        for new in ["anthropic", "gemini", "openrouter", "groq", "local"] {
            assert!(ids.contains(&new), "新しい接続先がある: {new}");
        }
        // id は重複しない
        assert_eq!(ids.len(), PRESETS.len());
        // Anthropic の初期モデルは Plan.md 6 章の決定どおり
        assert_eq!(find("anthropic").unwrap().default_model, "claude-haiku-5-5");
    }

    #[test]
    fn local_urls_are_recognized_and_remote_ones_are_not() {
        assert!(is_local_url("http://127.0.0.1:8080/v1"));
        assert!(is_local_url("http://localhost:8000/v1"));
        assert!(is_local_url("http://[::1]:8000/v1"));
        assert!(!is_local_url("https://api.openai.com/v1"));
        assert!(!is_local_url("http://127.0.0.1.evil.org/v1"));
        assert!(!is_local_url("not a url"));
    }
}
