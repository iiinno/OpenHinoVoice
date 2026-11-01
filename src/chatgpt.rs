//! Sign in with ChatGPT(オープンソース向けの ChatGPT プラン利用)。
//! 手順は https://developers.openai.com/siwc/token-sharing-open-source に従う。
//! ブラウザで認可し、127.0.0.1 の受け口でコードを受け取り、トークンに換えて DPAPI で暗号化して保存する。

use crate::format::FormatError;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const DISCOVERY_URL: &str = "https://auth.openai.com/.well-known/openid-configuration";
const JWKS_URL: &str = "https://auth.openai.com/.well-known/jwks.json";
const ISSUER: &str = "https://auth.openai.com";
const RESOURCE: &str = "https://api.openai.com/v1";
const API: &str = "https://api.openai.com/v1";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const CALLBACK_PATH: &str = "/auth/callback";
const PORTS: [u16; 4] = [1455, 1456, 1457, 1458];
pub const USAGE_URL: &str = "https://chatgpt.com/settings/usage";

/// 暗号化せずに置く登録情報。秘密ではない
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Registration {
    host_id: String,
    client_id: String,
    email: String,
}

/// DPAPI で暗号化して置くトークン
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_at: u64,
    scope: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    SignedOut,
    SignedIn { email: String, plan_usage: bool },
}

fn dir() -> PathBuf {
    crate::config::config_path()
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default()
}
fn registration_path() -> PathBuf {
    dir().join("chatgpt.json")
}
fn tokens_path() -> PathBuf {
    dir().join("chatgpt_tokens.bin")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn load_registration() -> Registration {
    std::fs::read_to_string(registration_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_registration(r: &Registration) -> Result<(), String> {
    let text = serde_json::to_string_pretty(r).map_err(|e| e.to_string())?;
    std::fs::write(registration_path(), text).map_err(|e| format!("登録情報を保存できません: {e}"))
}

fn load_tokens() -> Option<Tokens> {
    let sealed = std::fs::read(tokens_path()).ok()?;
    let plain = dpapi::unprotect(&sealed).ok()?;
    serde_json::from_slice(&plain).ok()
}

fn save_tokens(t: &Tokens) -> Result<(), String> {
    let plain = serde_json::to_vec(t).map_err(|e| e.to_string())?;
    let sealed = dpapi::protect(&plain)?;
    // 書きかけのファイルを残さないよう、別名に書いてから置き換える
    let tmp = tokens_path().with_extension("tmp");
    std::fs::write(&tmp, sealed).map_err(|e| format!("トークンを保存できません: {e}"))?;
    std::fs::rename(&tmp, tokens_path()).map_err(|e| format!("トークンを保存できません: {e}"))
}

pub fn is_signed_in() -> bool {
    tokens_path().is_file()
}

pub fn status() -> Status {
    match load_tokens() {
        Some(t) => Status::SignedIn {
            email: load_registration().email,
            plan_usage: has_plan_scope(&t.scope),
        },
        None => Status::SignedOut,
    }
}

fn has_plan_scope(scope: &str) -> bool {
    scope.split_whitespace().any(|s| s == PLAN_SCOPE)
}

// ---- サインイン ----

/// ブラウザで認可して、トークンを保存する。終わるまで(最長 5 分)戻らないので別スレッドで呼ぶ
pub fn sign_in() -> Result<Status, String> {
    let mut reg = load_registration();
    if reg.host_id.is_empty() {
        reg.host_id = format!("urn:uuid:{}", uuid_v4());
        save_registration(&reg)?;
    }
    let (listener, port) = PORTS
        .iter()
        .find_map(|p| TcpListener::bind(("127.0.0.1", *p)).ok().map(|l| (l, *p)))
        .ok_or("受け口のポート(1455〜1458)が使われていて、サインインを始められません")?;
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes(32));
    let state = URL_SAFE_NO_PAD.encode(random_bytes(16));
    let nonce = URL_SAFE_NO_PAD.encode(random_bytes(16));
    let first = reg.client_id.is_empty();
    let mut params = vec![
        (
            "client_id",
            if first {
                "dynamic_agent_client".to_string()
            } else {
                reg.client_id.clone()
            },
        ),
        ("ext_agent_host_id", reg.host_id.clone()),
        ("response_type", "code".into()),
        ("redirect_uri", redirect_uri.clone()),
        ("scope", SCOPE.into()),
        ("resource", RESOURCE.into()),
        ("state", state.clone()),
        ("nonce", nonce.clone()),
        ("code_challenge_method", "S256".into()),
        ("code_challenge", pkce_challenge(&verifier)),
    ];
    if first {
        params.push(("agent_name_hint", "OpenHinoVoice".into()));
    } else if !reg.email.is_empty() {
        params.push(("login_hint", reg.email.clone()));
    }
    let url = format!("{AUTHORIZE_URL}?{}", encode_query(&params));
    open_browser(&url);

    let query = wait_for_callback(&listener, Duration::from_secs(300))?;
    if query.get("state").map(String::as_str) != Some(state.as_str()) {
        return Err("サインインの応答が一致しません。もう一度お試しください".into());
    }
    if let Some(err) = query.get("error") {
        return Err(if err == "access_denied" {
            "サインインが取り消されました".into()
        } else {
            format!("サインインに失敗しました: {err}")
        });
    }
    let code = query
        .get("code")
        .ok_or("認可コードが返ってきませんでした")?;
    match (first, query.get("client_id")) {
        (true, Some(id)) => reg.client_id = id.clone(),
        (true, None) => return Err("登録が完了しませんでした。もう一度お試しください".into()),
        (false, Some(id)) if *id != reg.client_id => return Err("登録情報が一致しません".into()),
        _ => {}
    }

    let resp: TokenResponse = post_form(&[
        ("grant_type", "authorization_code"),
        ("client_id", &reg.client_id),
        ("code", code),
        ("code_verifier", &verifier),
        ("redirect_uri", &redirect_uri),
        ("resource", RESOURCE),
    ])?;
    // TLS だけで済ませず、公開 JWKS により RS256 署名を検証する。
    let claims = check_id_token(&resp.id_token, &reg.client_id, &nonce)?;
    reg.email = claims.email.unwrap_or_default();
    save_registration(&reg)?;
    let tokens = Tokens {
        access_token: resp.access_token,
        refresh_token: resp.refresh_token.unwrap_or_default(),
        id_token: resp.id_token,
        expires_at: now() + resp.expires_in,
        scope: resp.scope.unwrap_or_default(),
    };
    let _guard = NamedLock::acquire("Local\\OpenHinoVoice.ChatGPTRefresh")?;
    save_tokens(&tokens)?;
    Ok(status())
}

pub fn sign_out() -> Result<(), String> {
    // refresh と同じ名前付き排他を取得してから失効/削除する。
    // これによりサインアウト直後に古い refresh がトークンを復活させない。
    let _guard = NamedLock::acquire("Local\\OpenHinoVoice.ChatGPTRefresh")?;
    let mut warning: Option<String> = None;
    if let Some(t) = load_tokens() {
        let reg = load_registration();
        // 取り消し先は OpenID の設定情報から引く。失敗は利用者に通知する。
        let endpoint = agent()
            .get(DISCOVERY_URL)
            .call()
            .ok()
            .and_then(|r| r.into_json::<serde_json::Value>().ok())
            .and_then(|v| v["revocation_endpoint"].as_str().map(str::to_string));
        if let Some(url) = endpoint {
            let mut revoked = false;
            for attempt in 0..3 {
                let result = agent()
                    .post(&url)
                    .timeout(Duration::from_secs(6))
                    .send_form(&[
                        ("token", t.refresh_token.as_str()),
                        ("token_type_hint", "refresh_token"),
                        ("client_id", &reg.client_id),
                    ]);
                match result {
                    Ok(_) => {
                        revoked = true;
                        break;
                    }
                    Err(ureq::Error::Status(code, _)) if code >= 500 && attempt < 2 => {
                        std::thread::sleep(Duration::from_millis(200 * (1 << attempt)));
                    }
                    Err(ureq::Error::Transport(_)) if attempt < 2 => {
                        std::thread::sleep(Duration::from_millis(200 * (1 << attempt)));
                    }
                    _ => break,
                }
            }
            if !revoked {
                warning = Some("サーバー側のセッション失効を確認できませんでした".into());
            }
        } else {
            warning = Some("失効用URLを取得できませんでした".into());
        }
    }
    match std::fs::remove_file(tokens_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("トークンを消せません: {e}")),
    }
    if let Some(warning) = warning {
        return Err(format!("ローカルではサインアウト済みですが、{warning}。ChatGPT設定からこのアプリの接続を解除できます"));
    }
    Ok(())
}

fn wait_for_callback(
    listener: &TcpListener,
    timeout: Duration,
) -> Result<std::collections::HashMap<String, String>, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).ok();
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).ok();
                let target = line.split_whitespace().nth(1).unwrap_or("");
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                if path != CALLBACK_PATH {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }
                let body = "<!doctype html><meta charset=utf-8><title>OpenHinoVoice</title><body style=\"font-family:sans-serif;padding:3em\"><h2>OpenHinoVoice にサインインしました</h2><p>このタブは閉じてかまいません。</p>";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                return Ok(parse_query(query));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() > deadline {
                    return Err("サインインが 5 分以内に終わりませんでした".into());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("サインインの受け口でエラー: {e}")),
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: String,
    expires_in: u64,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct IdClaims {
    iss: String,
    aud: serde_json::Value,
    exp: u64,
    nonce: Option<String>,
    sub: String,
    email: Option<String>,
}

fn check_id_token(id_token: &str, client_id: &str, nonce: &str) -> Result<IdClaims, String> {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation};
    let header =
        jsonwebtoken::decode_header(id_token).map_err(|_| "IDトークンのヘッダーが不正です")?;
    if header.alg != Algorithm::RS256 {
        return Err("想定外のIDトークン署名方式です".into());
    }
    let kid = header.kid.ok_or("IDトークンの署名鍵IDがありません")?;
    let jwks: serde_json::Value = agent()
        .get(JWKS_URL)
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| format!("署名鍵を取得できません: {e}"))?
        .into_json()
        .map_err(|e| format!("署名鍵を読めません: {e}"))?;
    let key = jwks["keys"]
        .as_array()
        .and_then(|keys| {
            keys.iter().find(|k| {
                k["kid"].as_str() == Some(kid.as_str())
                    && k["kty"] == "RSA"
                    && k["alg"] == "RS256"
                    && k["use"] == "sig"
            })
        })
        .ok_or("対応するOpenAI署名鍵が見つかりません")?;
    let (n, e) = (
        key["n"].as_str().ok_or("署名鍵が不正です")?,
        key["e"].as_str().ok_or("署名鍵が不正です")?,
    );
    let decoding_key =
        DecodingKey::from_rsa_components(n, e).map_err(|_| "署名鍵を解析できません")?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[client_id]);
    validation.set_issuer(&[ISSUER]);
    let claims = jsonwebtoken::decode::<IdClaims>(id_token, &decoding_key, &validation)
        .map_err(|_| "IDトークンの署名・有効期限の検証に失敗しました")?
        .claims;
    validate_id_claims(&claims, client_id, nonce)?;
    Ok(claims)
}

fn validate_id_claims(claims: &IdClaims, client_id: &str, nonce: &str) -> Result<(), String> {
    let aud_ok = match &claims.aud {
        serde_json::Value::String(a) => a == client_id,
        serde_json::Value::Array(a) => a.iter().any(|v| v == client_id),
        _ => false,
    };
    if claims.iss != ISSUER
        || !aud_ok
        || claims.exp <= now()
        || claims.nonce.as_deref() != Some(nonce)
        || claims.sub.is_empty()
    {
        return Err("ID トークンの検証に失敗しました".into());
    }
    Ok(())
}

fn post_form<T: serde::de::DeserializeOwned>(form: &[(&str, &str)]) -> Result<T, String> {
    match agent().post(TOKEN_URL).send_form(form) {
        Ok(r) => r
            .into_json()
            .map_err(|e| format!("トークンの応答を読めません: {e}")),
        Err(ureq::Error::Status(code, r)) => {
            let body: serde_json::Value = r.into_json().unwrap_or_default();
            Err(format!(
                "トークンの取得に失敗しました({code} {})",
                body["error"].as_str().unwrap_or("")
            ))
        }
        Err(e) => Err(format!("認証サーバーに接続できません: {e}")),
    }
}

// ---- アクセストークン ----

/// 期限が近ければ更新した、使えるアクセストークンを返す
fn access_token() -> Result<String, String> {
    let mut t = load_tokens().ok_or("ChatGPT にサインインしていません")?;
    if !has_plan_scope(&t.scope) {
        return Err(
            "ChatGPT プランの利用が許可されていません。設定からサインインし直してください".into(),
        );
    }
    if t.expires_at > now() + 60 {
        return Ok(t.access_token);
    }
    // 常駐側と設定ウィンドウが同時に更新しないよう、プロセスをまたいで順番にする
    let _guard = NamedLock::acquire("Local\\OpenHinoVoice.ChatGPTRefresh")?;
    // ロック取得待ち中に他プロセスがサインアウトしていた場合、
    // 古いメモリ上の refresh_token を使って再認証してはならない。
    let latest = load_tokens().ok_or("ChatGPT からサインアウトしています")?;
    if latest.expires_at > now() + 60 {
        return Ok(latest.access_token);
    }
    t = latest;
    let reg = load_registration();
    let resp: Result<TokenResponse, String> = post_form(&[
        ("grant_type", "refresh_token"),
        ("client_id", &reg.client_id),
        ("refresh_token", &t.refresh_token),
        ("resource", RESOURCE),
    ]);
    let resp = resp.map_err(|e| format!("{e}。設定から ChatGPT にサインインし直してください"))?;
    t.access_token = resp.access_token;
    if let Some(r) = resp.refresh_token {
        t.refresh_token = r;
    }
    t.expires_at = now() + resp.expires_in;
    if let Some(s) = resp.scope {
        t.scope = s;
    }
    save_tokens(&t)?;
    Ok(t.access_token)
}

/// 録音中に呼ぶ。期限が近ければ、Enter のあとではなく今のうちにトークンを更新する
pub fn refresh_if_needed() {
    let _ = access_token();
}

pub struct Model {
    pub slug: String,
    pub display_name: String,
}

pub fn list_models() -> Result<Vec<Model>, String> {
    let token = access_token()?;
    let resp = agent()
        .get(&format!("{API}/models"))
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| format!("モデル一覧を取得できません: {e}"))?;
    let v: serde_json::Value = resp.into_json().map_err(|e| e.to_string())?;
    Ok(v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .filter_map(|m| {
            Some(Model {
                slug: m["slug"].as_str()?.to_string(),
                display_name: m["display_name"]
                    .as_str()
                    .unwrap_or(m["slug"].as_str()?)
                    .to_string(),
            })
        })
        .collect())
}

/// Responses API で整形する。プラン利用では store と stream が必須で、temperature などは送れない
pub fn format(
    model: &str,
    instructions: &str,
    raw: &str,
    fast: bool,
    wait: Duration,
) -> Result<String, FormatError> {
    let token = access_token().map_err(FormatError::Provider)?;
    let mut body = serde_json::json!({
        "model": model,
        "instructions": instructions,
        "input": [{ "role": "user", "content": raw }],
        "store": false,
        "stream": true,
    });
    if fast {
        body["service_tier"] = "priority".into();
    }
    // 合計時間では切らず、応答が wait の間ずっと途切れたときだけ打ち切る
    let started = std::time::Instant::now();
    let resp = crate::format::streaming_agent(wait, false)
        .post(&format!("{API}/responses"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body);
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let v: serde_json::Value = r.into_json().unwrap_or_default();
            let err_code = v["error"]["code"].as_str().unwrap_or("").to_string();
            // Fast モードが使えない場合は通常の速さでもう一度だけ送る
            if fast && err_code == "subscription_sharing_unsupported_capability" {
                return format(model, instructions, raw, false, wait);
            }
            return Err(classify(code, &err_code));
        }
        Err(e) => return Err(crate::format::transport_error(&e)),
    };
    read_stream(resp.into_reader(), started)
}

/// ChatGPT のエラーを、やり直す価値のあるものとないものに分ける
fn classify(http: u16, code: &str) -> FormatError {
    match code {
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            FormatError::Busy(explain_error(http, code))
        }
        "" if http == 408 || http == 429 || http >= 500 => FormatError::Http(http),
        _ => FormatError::Provider(explain_error(http, code)),
    }
}

/// Server-Sent Events を読み、response.completed までの本文を返す
fn read_stream(reader: impl Read, started: std::time::Instant) -> Result<String, FormatError> {
    let mut text = String::new();
    for line in BufReader::new(reader).lines() {
        let line = line.map_err(|e| crate::format::transport_error(&e))?;
        if started.elapsed() > crate::format::CEILING {
            return Err(FormatError::Timeout);
        }
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<serde_json::Value>(data.trim()) else {
            continue;
        };
        match event["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => text.push_str(event["delta"].as_str().unwrap_or("")),
            "response.completed" => return Ok(text),
            "response.failed" => {
                let code = event["response"]["error"]["code"].as_str().unwrap_or("");
                return Err(classify(0, code));
            }
            "response.incomplete" => {
                return Err(FormatError::Network(
                    "応答が最後まで返りませんでした".into(),
                ))
            }
            _ => {}
        }
    }
    Err(FormatError::Network("応答が途中で切れました".into()))
}

fn explain_error(http: u16, code: &str) -> String {
    match code {
        "subscription_sharing_usage_limit_exceeded" => {
            "ChatGPT プランの利用上限に達しました".into()
        }
        "subscription_sharing_user_not_eligible" => {
            "この ChatGPT プランでは使えません(Plus か Pro が必要です)".into()
        }
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            "ChatGPT が一時的に使えません".into()
        }
        "subscription_sharing_invalid_user" => "ChatGPT にサインインし直してください".into(),
        "" => format!("ChatGPT のエラー(HTTP {http})"),
        other => format!("ChatGPT のエラー({other})"),
    }
}

fn agent() -> &'static ureq::Agent {
    crate::format::openai_agent()
}

// ---- 小道具 ----

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(sha256(verifier.as_bytes()))
}

fn sha256(data: &[u8]) -> [u8; 32] {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    assert!(status.is_ok(), "BCryptHash failed: {status:?}");
    out
}

fn random_bytes(n: usize) -> Vec<u8> {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut buf = vec![0u8; n];
    let status = unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    assert!(status.is_ok(), "BCryptGenRandom failed: {status:?}");
    buf
}

fn uuid_v4() -> String {
    let mut b = random_bytes(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn parse_query(q: &str) -> std::collections::HashMap<String, String> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() && hex_byte(&b[i + 1..i + 3]).is_some() => {
                out.push(hex_byte(&b[i + 1..i + 3]).unwrap_or(b'%'));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_byte(pair: &[u8]) -> Option<u8> {
    u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()
}

pub(crate) fn open_browser(url: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("open"),
            windows::core::PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

/// プロセスをまたいだ排他。名前付きミューテックスを取り、抜けるときに離す
struct NamedLock(windows::Win32::Foundation::HANDLE);

impl NamedLock {
    fn acquire(name: &str) -> Result<NamedLock, String> {
        use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let h = CreateMutexW(None, false, windows::core::PCWSTR(wide.as_ptr()))
                .map_err(|e| format!("ChatGPTの排他ロックを作成できません: {e}"))?;
            let outcome = WaitForSingleObject(h, 30_000).0;
            // WAIT_OBJECT_0 (0), WAIT_ABANDONED (0x80) のみ排他取得済み。
            // WAIT_TIMEOUT / WAIT_FAILED 等では Drop が ReleaseMutex しないよう閉じる。
            if outcome != 0 && outcome != 0x80 {
                let _ = windows::Win32::Foundation::CloseHandle(h);
                return Err(format!(
                    "ChatGPTの排他ロックを取得できません ({outcome:#x})"
                ));
            }
            Ok(NamedLock(h))
        }
    }
}

impl Drop for NamedLock {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Threading::ReleaseMutex(self.0);
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

pub(crate) mod dpapi {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    /// 今の Windows ユーザーだけが復号できる形に暗号化する
    pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &input,
                windows::core::w!("OpenHinoVoice"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| format!("トークンを暗号化できません: {e}"))?;
            Ok(take(out))
        }
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| format!("トークンを復号できません: {e}"))?;
            Ok(take(out))
        }
    }

    unsafe fn take(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let v = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(blob.pbData as *mut core::ffi::c_void));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn query_round_trip() {
        let q = encode_query(&[
            ("scope", SCOPE.into()),
            ("redirect_uri", "http://127.0.0.1:1455/auth/callback".into()),
        ]);
        assert!(q.contains("scope=openid%20profile"), "{q}");
        assert!(
            q.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback"),
            "{q}"
        );
        let back = parse_query("code=a%2Bb&scope=openid+email&state=x&bad=%E3%81%82%&jp=あ%zz");
        assert_eq!(back["bad"], "あ%");
        assert_eq!(back["jp"], "あ%zz");
        assert_eq!(back["code"], "a+b");
        assert_eq!(back["scope"], "openid email");
    }

    #[test]
    fn uuid_is_version_4() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn dpapi_round_trip() {
        let sealed = dpapi::protect(b"secret").unwrap();
        assert_ne!(sealed, b"secret");
        assert_eq!(dpapi::unprotect(&sealed).unwrap(), b"secret");
    }

    #[test]
    fn id_token_claims_are_checked() {
        let make = |iss: &str, aud: &str, exp: u64, nonce: &str| IdClaims {
            iss: iss.to_string(),
            aud: serde_json::Value::String(aud.to_string()),
            exp,
            nonce: Some(nonce.to_string()),
            sub: "account-123".to_string(),
            email: Some("a@example.com".to_string()),
        };
        let later = now() + 600;
        assert!(validate_id_claims(&make(ISSUER, "oaiapp_1", later, "n"), "oaiapp_1", "n").is_ok());
        assert!(validate_id_claims(
            &make("https://evil", "oaiapp_1", later, "n"),
            "oaiapp_1",
            "n"
        )
        .is_err());
        assert!(validate_id_claims(&make(ISSUER, "other", later, "n"), "oaiapp_1", "n").is_err());
        assert!(validate_id_claims(&make(ISSUER, "oaiapp_1", 1, "n"), "oaiapp_1", "n").is_err());
        assert!(
            validate_id_claims(&make(ISSUER, "oaiapp_1", later, "m"), "oaiapp_1", "n").is_err()
        );
        // 偽造・無署名 JWT は JWKS 取得前に拒否される。
        assert!(check_id_token("x.y.z", "oaiapp_1", "n").is_err());
    }

    #[test]
    fn stream_collects_deltas_until_completed() {
        let sse = "event: x\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"明日\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"です。\"}\n\ndata: {\"type\":\"response.completed\"}\n";
        assert_eq!(
            read_stream(sse.as_bytes(), std::time::Instant::now()).unwrap(),
            "明日です。"
        );
        let failed = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\"}}}\n";
        let err = read_stream(failed.as_bytes(), std::time::Instant::now()).unwrap_err();
        assert!(err.to_string().contains("上限"));
        assert!(!err.retryable(), "利用上限はやり直しても通らない");
        let busy = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"subscription_sharing_usage_unavailable\"}}}\n";
        assert!(
            read_stream(busy.as_bytes(), std::time::Instant::now())
                .unwrap_err()
                .retryable(),
            "一時的に使えないときはやり直す"
        );
        assert!(read_stream(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n".as_bytes(),
            std::time::Instant::now()
        )
        .is_err());
    }
}
