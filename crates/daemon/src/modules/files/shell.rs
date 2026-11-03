//! Splits the text a shell runs into simple commands (program + arguments), keeping track of
//! `cd`. Good enough for finding downloads in what agents ran; not a full shell grammar:
//! no expansion, no functions, no subshells. Quotes, PowerShell here-strings, bash
//! here-documents and line continuations are handled so that their contents stay one word.

use agents::model::Shell;

use super::paths::{PathEnv, resolve};

/// One simple command: words after quote removal, and the folder it ran in when known.
#[derive(Debug, Clone, PartialEq)]
pub struct Simple {
    pub words: Vec<String>,
    /// The raw text of this command (for a summary).
    pub raw: String,
    pub cwd: Option<String>,
}

impl Simple {
    /// Lower-case program name without folder and `.exe` / `.cmd` / `.bat` / `.ps1`.
    pub fn program(&self) -> String {
        program_name(self.words.first().map(String::as_str).unwrap_or(""))
    }
}

pub fn program_name(word: &str) -> String {
    let name = agents::file_name(word).to_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".ps1"] {
        if let Some(n) = name.strip_suffix(ext) {
            return n.to_owned();
        }
    }
    name
}

/// The simple commands in `text`, in order. `cwd` is where the text started running; a `cd`
/// (`Set-Location`, `pushd`, ...) changes it for the commands after it.
pub fn commands(text: &str, shell: &Shell, cwd: Option<&str>, env: &PathEnv) -> Vec<Simple> {
    let ps = matches!(shell, Shell::PowerShell);
    let mut out = Vec::new();
    let mut cwd = cwd.map(str::to_owned);
    for (words, raw) in split(text, ps) {
        let mut words = words;
        // Leading call operator, environment assignments and wrappers.
        while let Some(first) = words.first() {
            let lower = first.to_lowercase();
            let assignment = !ps && first.contains('=') && !first.starts_with('-') && !first.starts_with('=');
            if (ps && first == "&") || assignment || ["sudo", "time", "nohup", "env", "call", "exec", "command", "builtin"].contains(&lower.as_str()) {
                words.remove(0);
            } else {
                break;
            }
        }
        if words.is_empty() {
            continue;
        }
        let program = program_name(&words[0]);
        if ["cd", "chdir", "set-location", "sl", "pushd", "push-location"].contains(&program.as_str()) {
            let target = words.iter().skip(1).find(|w| !w.starts_with('-') || w.as_str() == "-").cloned();
            if let Some(t) = target {
                cwd = resolve(&t, cwd.as_deref(), env);
            }
            continue;
        }
        out.push(Simple { words, raw: raw.trim().to_owned(), cwd: cwd.clone() });
    }
    out
}

/// Words of each simple command with its raw text.
fn split(text: &str, ps: bool) -> Vec<(Vec<String>, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut raw = String::new();
    let mut heredocs: Vec<String> = Vec::new();
    let mut i = 0;
    let flush_word = |words: &mut Vec<String>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    macro_rules! end_command {
        () => {{
            flush_word(&mut words, &mut word, &mut in_word);
            if !words.is_empty() {
                out.push((std::mem::take(&mut words), std::mem::take(&mut raw)));
            }
            raw.clear();
        }};
    }
    while i < chars.len() {
        let c = chars[i];
        // PowerShell here-string: @' ... '@ / @" ... "@ (closing marker at the start of a line).
        if ps && c == '@' && matches!(chars.get(i + 1), Some('\'') | Some('"')) && !in_word {
            let q = chars[i + 1];
            let mut j = i + 2;
            while j < chars.len() && chars[j] != '\n' {
                j += 1;
            }
            let start = (j + 1).min(chars.len());
            let mut k = start;
            let mut end = chars.len();
            let mut after = chars.len();
            while k < chars.len() {
                let line_start = k == start || chars[k - 1] == '\n';
                if line_start && chars[k] == q && chars.get(k + 1) == Some(&'@') {
                    end = k.saturating_sub(1).max(start);
                    after = k + 2;
                    break;
                }
                k += 1;
            }
            let body: String = chars[start..end.min(chars.len())].iter().collect();
            words.push(body);
            raw.push_str("@'...'@");
            i = after;
            continue;
        }
        match c {
            '\'' => {
                in_word = true;
                let mut j = i + 1;
                while j < chars.len() {
                    if chars[j] == '\'' {
                        if ps && chars.get(j + 1) == Some(&'\'') {
                            word.push('\'');
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    word.push(chars[j]);
                    j += 1;
                }
                raw.extend(&chars[i..(j + 1).min(chars.len())]);
                i = j + 1;
                continue;
            }
            '"' => {
                in_word = true;
                let esc = if ps { '`' } else { '\\' };
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '"' {
                    if chars[j] == esc && j + 1 < chars.len() {
                        let n = chars[j + 1];
                        if ps || matches!(n, '"' | '\\' | '$' | '`' | '\n') {
                            if n != '\n' {
                                word.push(n);
                            }
                            j += 2;
                            continue;
                        }
                    }
                    word.push(chars[j]);
                    j += 1;
                }
                raw.extend(&chars[i..(j + 1).min(chars.len())]);
                i = j + 1;
                continue;
            }
            '`' if ps => {
                // Escape; at the end of a line it continues the command.
                if let Some(&n) = chars.get(i + 1) {
                    if n == '\n' || (n == '\r' && chars.get(i + 2) == Some(&'\n')) {
                        i += if n == '\r' { 3 } else { 2 };
                        continue;
                    }
                    word.push(n);
                    in_word = true;
                    raw.push(c);
                    raw.push(n);
                    i += 2;
                    continue;
                }
            }
            '\\' if !ps => {
                if let Some(&n) = chars.get(i + 1) {
                    if n == '\n' {
                        i += 2;
                        continue;
                    }
                    word.push(n);
                    in_word = true;
                    raw.push(c);
                    raw.push(n);
                    i += 2;
                    continue;
                }
            }
            '#' if !in_word && (i == 0 || chars[i - 1].is_whitespace() || chars[i - 1] == ';') => {
                // A comment runs to the end of the line.
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '\n' => {
                end_command!();
                i += 1;
                // Bash here-documents: skip their bodies.
                while let Some(tag) = heredocs.first().cloned() {
                    let mut line = String::new();
                    while i < chars.len() && chars[i] != '\n' {
                        line.push(chars[i]);
                        i += 1;
                    }
                    i += 1;
                    if line.trim() == tag || i >= chars.len() {
                        heredocs.remove(0);
                    }
                }
                continue;
            }
            ';' | '|' => {
                end_command!();
                i += if chars.get(i + 1) == Some(&c) { 2 } else { 1 };
                continue;
            }
            '&' => {
                if chars.get(i + 1) == Some(&'&') {
                    end_command!();
                    i += 2;
                    continue;
                }
                if ps {
                    // The call operator.
                    flush_word(&mut words, &mut word, &mut in_word);
                    words.push("&".to_owned());
                    raw.push('&');
                    i += 1;
                    continue;
                }
                end_command!();
                i += 1;
                continue;
            }
            '<' if !ps && chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                flush_word(&mut words, &mut word, &mut in_word);
                let mut j = i + 2;
                if chars.get(j) == Some(&'-') {
                    j += 1;
                }
                while j < chars.len() && chars[j] == ' ' {
                    j += 1;
                }
                let mut tag = String::new();
                while j < chars.len() && !chars[j].is_whitespace() && !matches!(chars[j], ';' | '|' | '&') {
                    if !matches!(chars[j], '\'' | '"') {
                        tag.push(chars[j]);
                    }
                    j += 1;
                }
                if !tag.is_empty() {
                    heredocs.push(tag);
                }
                i = j;
                continue;
            }
            c if c.is_whitespace() => {
                flush_word(&mut words, &mut word, &mut in_word);
                raw.push(' ');
                i += 1;
                continue;
            }
            _ => {}
        }
        word.push(c);
        in_word = true;
        raw.push(c);
        i += 1;
    }
    end_command!();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> PathEnv {
        PathEnv::from_agents_env(&agents::Env::with_home(r"C:\Users\u"))
    }

    fn words(text: &str, shell: Shell) -> Vec<Vec<String>> {
        commands(text, &shell, Some(r"D:\w"), &env()).into_iter().map(|s| s.words).collect()
    }

    #[test]
    fn splits_powershell() {
        let w = words("Set-Location D:\\x; git clone 'https://a/b.git' \"my dir\" && & \"C:\\Program Files\\Git\\bin\\git.exe\" status | Out-Null", Shell::PowerShell);
        assert_eq!(w[0], ["git", "clone", "https://a/b.git", "my dir"]);
        assert_eq!(w[1], [r"C:\Program Files\Git\bin\git.exe", "status"]);
        assert_eq!(w[2], ["Out-Null"]);
        let s = commands("cd D:\\x; git clone u", &Shell::PowerShell, Some(r"C:\a"), &env());
        assert_eq!(s[0].cwd.as_deref(), Some(r"D:\x"));
        let here = words("$s = @'\nline one; not a command\n'@\npython -c $s", Shell::PowerShell);
        assert_eq!(here.len(), 2);
        assert_eq!(here[0], ["$s", "=", "line one; not a command"]);
        assert_eq!(here[1], ["python", "-c", "$s"]);
        let cont = words("Invoke-WebRequest `\n  -Uri https://x/y.zip `\n  -OutFile y.zip", Shell::PowerShell);
        assert_eq!(cont, [vec!["Invoke-WebRequest", "-Uri", "https://x/y.zip", "-OutFile", "y.zip"]]);
        assert_eq!(words("# just a comment\n", Shell::PowerShell).len(), 0);
    }

    #[test]
    fn splits_bash() {
        let w = words("cd /d/x && FOO=1 curl -sSLo out.bin \"https://h/f?a=1\" ; echo 'a;b' # c", Shell::Bash);
        assert_eq!(w[0], ["curl", "-sSLo", "out.bin", "https://h/f?a=1"]);
        assert_eq!(w[1], ["echo", "a;b"]);
        let s = commands("cd /d/x && wget u", &Shell::Bash, None, &env());
        assert_eq!(s[0].cwd.as_deref(), Some(r"D:\x"));
        let here = words("cat <<'EOF' > f.py\ngit clone nope\nEOF\nls", Shell::Bash);
        assert_eq!(here, [vec!["cat", ">", "f.py"], vec!["ls"]]);
        assert_eq!(words("a \\\n b", Shell::Bash), [vec!["a", "b"]]);
    }
}
