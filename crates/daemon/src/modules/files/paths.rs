//! Windows-style path handling for paths found in transcripts: expanding `~` and the common
//! environment variables, turning Git Bash paths (`/d/x`) into `D:\x`, resolving relative
//! paths against the command's folder, and a case-insensitive key for lookups.

use std::collections::BTreeMap;

/// What a path may refer to: the home folder and a few variables (upper-case names).
#[derive(Debug, Clone, Default)]
pub struct PathEnv {
    pub home: String,
    pub vars: BTreeMap<String, String>,
}

impl PathEnv {
    pub fn from_agents_env(env: &agents::Env) -> PathEnv {
        let home = env.home.to_string_lossy().into_owned();
        let mut vars: BTreeMap<String, String> = BTreeMap::new();
        vars.insert("USERPROFILE".into(), home.clone());
        vars.insert("HOME".into(), home.clone());
        vars.insert("LOCALAPPDATA".into(), env.local_app_data().to_string_lossy().into_owned());
        vars.insert("APPDATA".into(), env.app_data().to_string_lossy().into_owned());
        let temp = env.local_app_data().join("Temp").to_string_lossy().into_owned();
        vars.insert("TEMP".into(), temp.clone());
        vars.insert("TMP".into(), temp);
        PathEnv { home, vars }
    }
}

/// `path` as an absolute, normalised Windows path, or `None` when it cannot be known (a
/// relative path without a folder, an unknown variable, a remote or WSL path).
pub fn resolve(path: &str, cwd: Option<&str>, env: &PathEnv) -> Option<String> {
    let mut p = path.trim().to_owned();
    if p.is_empty() || p.contains('\n') || p.contains('*') || p.contains('?') && !p.starts_with(r"\\?\") {
        return None;
    }
    if let Some(rest) = p.strip_prefix(r"\\?\") {
        p = rest.to_owned();
    }
    // ~ and variables.
    if p == "~" || p.starts_with("~/") || p.starts_with("~\\") {
        p = format!("{}{}", env.home, &p[1..]);
    }
    p = expand(&p, env)?;
    // Git Bash / Cygwin drive paths.
    let lower = p.to_lowercase();
    if let Some(rest) = lower.strip_prefix("/cygdrive/").map(|_| &p[10..]) {
        p = drive_path(rest)?;
    } else if p.starts_with('/') && !p.starts_with("//") {
        if p.len() >= 2 && p.as_bytes()[1].is_ascii_alphabetic() && (p.len() == 2 || p.as_bytes()[2] == b'/') {
            p = drive_path(&p[1..])?;
        } else {
            // /home/..., /mnt/..., /tmp/...: not a Windows path we can see.
            return None;
        }
    }
    let p = p.replace('/', "\\");
    let absolute = is_absolute(&p);
    let joined = if absolute {
        p
    } else {
        let base = cwd.filter(|c| is_absolute(c))?;
        if p.starts_with('\\') {
            // Root of the current drive.
            format!("{}{p}", &base[..2])
        } else {
            format!("{base}\\{p}")
        }
    };
    normalise(&joined)
}

/// Lower-case form for comparing paths (Windows file names ignore case).
pub fn key(path: &str) -> String {
    path.to_lowercase()
}

pub fn is_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) || p.starts_with(r"\\")
}

/// Whether `inner` is `outer` or inside it (both normalised).
pub fn is_within(inner: &str, outer: &str) -> bool {
    let (i, o) = (key(inner), key(outer));
    i == o || (i.starts_with(&o) && i.as_bytes().get(o.len()) == Some(&b'\\'))
}

fn drive_path(rest: &str) -> Option<String> {
    let mut chars = rest.chars();
    let letter = chars.next()?.to_ascii_uppercase();
    let tail: String = chars.collect();
    if !letter.is_ascii_alphabetic() || !(tail.is_empty() || tail.starts_with('/')) {
        return None;
    }
    Some(format!("{letter}:{}", if tail.is_empty() { "\\".to_owned() } else { tail }))
}

/// Expands `$env:NAME`, `${env:NAME}`, `$NAME`, `${NAME}` and `%NAME%` for known names.
fn expand(p: &str, env: &PathEnv) -> Option<String> {
    if !p.contains('$') && !p.contains('%') {
        return Some(p.to_owned());
    }
    let mut out = String::new();
    let chars: Vec<char> = p.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '%' {
            let end = chars[i + 1..].iter().position(|&x| x == '%')?;
            let name: String = chars[i + 1..i + 1 + end].iter().collect();
            out.push_str(env.vars.get(&name.to_uppercase())?);
            i += end + 2;
            continue;
        }
        if c == '$' {
            let mut j = i + 1;
            let braced = chars.get(j) == Some(&'{');
            if braced {
                j += 1;
            }
            let mut name = String::new();
            while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_' || chars[j] == ':') {
                name.push(chars[j]);
                j += 1;
            }
            if braced {
                if chars.get(j) != Some(&'}') {
                    return None;
                }
                j += 1;
            }
            let name = name.strip_prefix("env:").or_else(|| name.strip_prefix("ENV:")).unwrap_or(&name).to_uppercase();
            out.push_str(env.vars.get(&name)?);
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    Some(out)
}

/// Collapses `.`, `..` and repeated separators; upper-cases the drive letter; no trailing `\`.
fn normalise(p: &str) -> Option<String> {
    let (prefix, rest) = if let Some(r) = p.strip_prefix(r"\\") {
        // \\server\share\...
        let mut parts = r.splitn(3, '\\');
        let server = parts.next().filter(|s| !s.is_empty())?;
        let share = parts.next().filter(|s| !s.is_empty())?;
        (format!(r"\\{server}\{share}"), parts.next().unwrap_or("").to_owned())
    } else {
        let drive = p[..1].to_uppercase();
        (format!("{drive}:"), p[2..].to_owned())
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('\\') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            x => parts.push(x),
        }
    }
    if parts.is_empty() {
        return Some(format!("{prefix}\\"));
    }
    Some(format!("{prefix}\\{}", parts.join("\\")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> PathEnv {
        PathEnv::from_agents_env(&agents::Env::with_home(r"C:\Users\u"))
    }

    #[test]
    fn resolves_windows_bash_and_relative_paths() {
        let e = env();
        assert_eq!(resolve(r"D:\a\.\b\..\c\", None, &e).as_deref(), Some(r"D:\a\c"));
        assert_eq!(resolve("d:/a/b", None, &e).as_deref(), Some(r"D:\a\b"));
        assert_eq!(resolve("/d/work/x", None, &e).as_deref(), Some(r"D:\work\x"));
        assert_eq!(resolve("/cygdrive/c/x", None, &e).as_deref(), Some(r"C:\x"));
        assert_eq!(resolve("sub/f.txt", Some(r"D:\w"), &e).as_deref(), Some(r"D:\w\sub\f.txt"));
        assert_eq!(resolve(r"..\up", Some(r"D:\w\x"), &e).as_deref(), Some(r"D:\w\up"));
        assert_eq!(resolve(r"\root", Some(r"D:\w"), &e).as_deref(), Some(r"D:\root"));
        assert_eq!(resolve("rel", None, &e), None);
        assert_eq!(resolve("/home/user/x", None, &e), None);
        assert_eq!(resolve("~/models", None, &e).as_deref(), Some(r"C:\Users\u\models"));
        assert_eq!(resolve(r"$env:USERPROFILE\.cache", None, &e).as_deref(), Some(r"C:\Users\u\.cache"));
        assert_eq!(resolve(r"%LOCALAPPDATA%\x", None, &e).as_deref(), Some(r"C:\Users\u\AppData\Local\x"));
        assert_eq!(resolve("$HOME/y", None, &e).as_deref(), Some(r"C:\Users\u\y"));
        assert_eq!(resolve(r"$unknown\x", None, &e), None);
        assert_eq!(resolve(r"\\srv\share\a\..\b", None, &e).as_deref(), Some(r"\\srv\share\b"));
        assert_eq!(resolve(r"D:\*.txt", None, &e), None);
    }

    #[test]
    fn keys_and_containment() {
        assert!(is_within(r"D:\A\b", r"d:\a"));
        assert!(is_within(r"D:\a", r"D:\A"));
        assert!(!is_within(r"D:\ab", r"D:\a"));
        assert_eq!(key(r"D:\A"), r"d:\a");
    }
}
