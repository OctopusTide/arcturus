//! Command summaries for the database: one command, cut short, with anything that looks like
//! a credential replaced by `***` (URL user info, secret query parameters, values after
//! `--token`-style flags, `Authorization:` headers, `NAME=value` where the name says secret,
//! and well-known token shapes such as `sk-...`, `hf_...`, `ghp_...`).

/// Longest summary kept, in characters.
pub const MAX_CHARS: usize = 240;

const SECRET_WORDS: &[&str] = &["token", "secret", "password", "passwd", "pwd", "apikey", "api_key", "api-key", "auth", "credential", "cookie", "session"];
const SECRET_FLAGS: &[&str] = &[
    "--token", "--hf-token", "--access-token", "--auth-token", "--password", "--passwd", "--api-key", "--apikey", "--secret",
    "--auth", "--user", "-u", "-token", "-password", "-apikey", "-credential", "--cookie", "-b", "--oauth2-bearer",
];
const TOKEN_PREFIXES: &[&str] = &[
    "sk-", "sk_", "hf_", "ghp_", "gho_", "ghs_", "ghu_", "ghr_", "github_pat_", "xoxb-", "xoxp-", "xoxa-", "glpat-", "akia", "aiza", "eyj",
    "npm_", "pypi-",
];

fn secret_name(name: &str) -> bool {
    let n = name.to_lowercase();
    let n = n.trim_start_matches('-').trim_start_matches("$env:").trim_start_matches('$');
    SECRET_WORDS.iter().any(|w| n.contains(w))
}

fn looks_like_token(core: &str) -> bool {
    let lower = core.to_lowercase();
    if core.len() >= 16 && TOKEN_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    let plain = core.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '=' | '/'));
    let has_digit = core.chars().any(|c| c.is_ascii_digit());
    let has_alpha = core.chars().any(|c| c.is_ascii_alphabetic());
    plain && core.len() >= 32 && has_digit && has_alpha && !core.contains("//") && core.matches('/').count() <= 1
}

/// Masks user info and secret query values inside URLs in `word`.
fn mask_url(word: &str) -> String {
    let Some(scheme_end) = word.find("://") else { return word.to_owned() };
    let start = scheme_end + 3;
    let rest = &word[start..];
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let mut out = word[..start].to_owned();
    let authority = &rest[..auth_end];
    match authority.rfind('@') {
        Some(at) => {
            out.push_str("***");
            out.push_str(&authority[at..]);
        }
        None => out.push_str(authority),
    }
    let tail = &rest[auth_end..];
    match tail.find('?') {
        None => out.push_str(tail),
        Some(q) => {
            out.push_str(&tail[..=q]);
            let params: Vec<String> = tail[q + 1..]
                .split('&')
                .map(|kv| match kv.split_once('=') {
                    Some((k, _)) if secret_name(k) || matches!(k.to_lowercase().as_str(), "sig" | "signature" | "key" | "x-amz-signature" | "x-amz-credential") => {
                        format!("{k}=***")
                    }
                    _ => kv.to_owned(),
                })
                .collect();
            out.push_str(&params.join("&"));
        }
    }
    out
}

/// The summary of `command`: credentials masked, whitespace runs collapsed, at most
/// [`MAX_CHARS`] characters.
pub fn summary(command: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut mask_next = 0u8;
    let mut prev_secret_name = false;
    for word in command.split_whitespace() {
        let core = word.trim_matches(|c| matches!(c, '"' | '\'' | '`' | '(' | ')' | ','));
        let lower = core.to_lowercase();
        let masked = if mask_next > 0 {
            mask_next -= 1;
            // "Authorization: Bearer xyz": the scheme word is kept, the value after it masked.
            if matches!(lower.as_str(), "bearer" | "basic" | "token" | "digest") {
                mask_next = 1;
                word.to_owned()
            } else {
                word.replace(core, "***")
            }
        } else if core == "=" && prev_secret_name {
            mask_next = 1;
            word.to_owned()
        } else if SECRET_FLAGS.contains(&lower.as_str()) {
            mask_next = 1;
            word.to_owned()
        } else if lower.ends_with(':') && (lower.contains("authorization") || secret_name(&lower[..lower.len() - 1])) {
            mask_next = 1;
            word.to_owned()
        } else if let Some((name, value)) = core.split_once('=')
            && !name.is_empty()
            && !name.contains("://")
            && (secret_name(name) || SECRET_FLAGS.contains(&name.to_lowercase().as_str()))
            && !value.is_empty()
        {
            word.replace(value, "***")
        } else if core.contains("://") {
            word.replace(core, &mask_url(core))
        } else if looks_like_token(core) {
            word.replace(core, "***")
        } else {
            word.to_owned()
        };
        prev_secret_name = secret_name(core) && !core.contains('=') && !core.contains("://");
        out.push(masked);
    }
    let joined = out.join(" ");
    if joined.chars().count() > MAX_CHARS {
        let cut: String = joined.chars().take(MAX_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_masked() {
        let cases = [
            ("git clone https://user:ghp_abcdefghijklmnopqrstuvwxyz0123@github.com/o/r.git", "git clone https://***@github.com/o/r.git"),
            ("curl -H \"Authorization: Bearer abc.def\" -o f https://h/x", "curl -H \"Authorization: Bearer ***\" -o f https://h/x"),
            ("huggingface-cli download m --token hf_ABCDEFGHIJKLMNOPQRSTUV", "huggingface-cli download m --token ***"),
            ("wget 'https://s3/x.bin?X-Amz-Signature=deadbeef&part=1'", "wget 'https://s3/x.bin?X-Amz-Signature=***&part=1'"),
            ("$env:HF_TOKEN = \"hf_ABCDEFGHIJKLMNOPQRSTUV\"", "$env:HF_TOKEN = \"***\""),
            ("OPENAI_API_KEY=sk-proj-abcdefghijklmnop python x.py", "OPENAI_API_KEY=*** python x.py"),
            ("curl -u me:pw https://h/", "curl -u *** https://h/"),
            ("pip install torch==2.4.0 --index-url https://download.pytorch.org/whl/cu121", "pip install torch==2.4.0 --index-url https://download.pytorch.org/whl/cu121"),
            ("echo 3f2a9c1b7d6e5f4a3b2c1d0e9f8a7b6c5d4e3f2a", "echo ***"),
            ("git clone https://github.com/a/b.git D:\\w\\b", "git clone https://github.com/a/b.git D:\\w\\b"),
        ];
        for (input, want) in cases {
            assert_eq!(summary(input), want, "{input}");
        }
        let long = "x ".repeat(300);
        assert_eq!(summary(&long).chars().count(), MAX_CHARS);
    }
}
