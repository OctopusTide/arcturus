//! Recognising, in one simple command, paths it put on disk that we can see: downloads (`git
//! clone`, `curl -o`, `Invoke-WebRequest -OutFile`, `huggingface-cli download --local-dir`,
//! `scp host:x .`), unpacking into a folder, and plain writes (`> file`, `mkdir`, `cp`,
//! `Out-File`); and the use of a shared cache (`pip install`, `npm install`, `docker pull`,
//! `wsl ...`, model downloads without a folder). Downloads whose files land in a cache only
//! count as using that cache.

use super::paths::{PathEnv, resolve};
use super::redact;
use super::shell::{Simple, program_name};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    Downloaded,
    Cloned,
    Extracted,
    /// Made by a command that is not a download (redirect, `mkdir`, copy).
    Written,
}

impl How {
    pub fn as_str(self) -> &'static str {
        match self {
            How::Downloaded => "downloaded",
            How::Cloned => "cloned",
            How::Extracted => "extracted",
            How::Written => "written",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    /// Files at `path` came from this command.
    Path { path: String, how: How, source: Option<String>, summary: String },
    /// The command used a shared cache.
    Cache { cache: &'static str, summary: String },
}

/// What recognising needs besides the command: where paths are relative to, and a way to
/// ask whether a path is an existing folder (for `scp x dir`).
pub struct Ctx<'a> {
    pub env: &'a PathEnv,
    pub is_dir: &'a dyn Fn(&str) -> bool,
}

fn is_url(w: &str) -> bool {
    w.contains("://")
}

/// Last path segment of a URL without query or fragment.
fn url_basename(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let after = path.split_once("://").map(|(_, r)| r).unwrap_or(path);
    let (_, p) = after.split_once('/')?;
    let name = p.trim_end_matches('/').rsplit('/').next()?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// `host:path` / `user@host:path` (scp-style), not a Windows drive like `C:\`.
fn is_remote_spec(w: &str) -> bool {
    if w.starts_with('-') || is_url(w) {
        return false;
    }
    match w.find(':') {
        Some(1) if w.as_bytes()[0].is_ascii_alphabetic() => false,
        Some(i) => i > 0 && !w[..i].contains(['\\', '/']),
        None => false,
    }
}

/// Value of an option given as `name value`, `name=value` or (PowerShell) `name:value`.
/// `ci`: names compare case-insensitively (PowerShell parameters); `prefix`: a name also
/// matches any longer word starting with it (PowerShell lets parameters be shortened, so
/// `-outf` stands for `-OutF`, `-OutFi`, `-OutFile`).
fn opt(words: &[String], names: &[&str], ci: bool, prefix: bool) -> Option<String> {
    for (i, w) in words.iter().enumerate().skip(1) {
        if !w.starts_with('-') {
            continue;
        }
        let (name, inline) = match w.split_once(['=', ':']) {
            Some((n, v)) => (n.to_owned(), Some(v.to_owned())),
            None => (w.clone(), None),
        };
        let name = if ci { name.to_lowercase() } else { name };
        if names.iter().any(|n| if prefix { name.starts_with(n) } else { name == *n }) {
            return inline.or_else(|| words.get(i + 1).cloned());
        }
    }
    None
}

fn has(words: &[String], names: &[&str]) -> bool {
    words.iter().skip(1).any(|w| names.contains(&w.as_str()))
}

/// Positional words after skipping options; `with_value` options (lower-case) take the next
/// word.
fn positionals(words: &[String], from: usize, with_value: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = from;
    while i < words.len() {
        let w = &words[i];
        if w == "--" {
            out.extend(words[i + 1..].iter().cloned());
            break;
        }
        if w.starts_with('-') && w.len() > 1 {
            if !w.contains('=') && with_value.contains(&w.to_lowercase().as_str()) {
                i += 1;
            }
        } else {
            out.push(w.clone());
        }
        i += 1;
    }
    out
}

fn path(c: &Simple, ctx: &Ctx<'_>, target: &str, how: How, source: Option<&str>) -> Option<Found> {
    let path = resolve(target, c.cwd.as_deref(), ctx.env)?;
    Some(Found::Path { path, how, source: source.map(redact::summary), summary: redact::summary(&c.raw) })
}

fn cache(c: &Simple, cache: &'static str) -> Found {
    Found::Cache { cache, summary: redact::summary(&c.raw) }
}

/// Everything recognised in one simple command.
pub fn find(c: &Simple, ctx: &Ctx<'_>) -> Vec<Found> {
    let w = &c.words;
    let program = c.program();
    let sub = |i: usize| w.get(i).map(|s| s.to_lowercase()).unwrap_or_default();
    let mut out = redirects(c, ctx);
    match program.as_str() {
        "git" => {
            // git [-C dir] [-c k=v] clone ...
            let mut i = 1;
            let mut cwd = c.cwd.clone();
            while i < w.len() && w[i].starts_with('-') {
                if w[i] == "-C" {
                    cwd = w.get(i + 1).and_then(|d| resolve(d, cwd.as_deref(), ctx.env));
                    i += 1;
                } else if w[i] == "-c" {
                    i += 1;
                }
                i += 1;
            }
            if sub(i) == "clone" {
                const V: &[&str] = &[
                    "-b", "--branch", "--depth", "-o", "--origin", "-c", "--config", "--reference", "--reference-if-able",
                    "--separate-git-dir", "--filter", "-j", "--jobs", "--template", "-u", "--upload-pack", "--shallow-since",
                    "--shallow-exclude", "--server-option", "--bundle-uri",
                ];
                let pos = positionals(w, i + 1, V);
                if let Some(url) = pos.first() {
                    let dir = pos.get(1).cloned().or_else(|| {
                        let base = url.trim_end_matches(['/', '\\']).rsplit(['/', '\\', ':']).next()?.to_owned();
                        Some(base.strip_suffix(".git").unwrap_or(&base).to_owned())
                    });
                    if let Some(dir) = dir {
                        let c2 = Simple { cwd, ..c.clone() };
                        out.extend(path(&c2, ctx, &dir, How::Cloned, Some(url)));
                    }
                }
            }
        }
        "gh" if sub(1) == "repo" && sub(2) == "clone" => {
            let pos = positionals(w, 3, &[]);
            if let Some(repo) = pos.first() {
                let dir = pos
                    .get(1)
                    .cloned()
                    .or_else(|| repo.trim_end_matches('/').rsplit('/').next().map(|s| s.trim_end_matches(".git").to_owned()));
                if let Some(dir) = dir {
                    out.extend(path(c, ctx, &dir, How::Cloned, Some(repo)));
                }
            }
        }
        "curl" | "wget" | "invoke-webrequest" | "iwr" | "invoke-restmethod" | "irm" => {
            if let Some(f) = opt(w, &["-outf"], true, true) {
                // PowerShell (also its curl/wget aliases): -OutFile.
                let url = opt(w, &["-uri"], true, true).or_else(|| w.iter().skip(1).find(|x| is_url(x)).cloned());
                out.extend(path(c, ctx, &f, How::Downloaded, url.as_deref()));
            } else if program == "curl" {
                out.extend(curl(c, ctx));
            } else if program == "wget" {
                out.extend(wget(c, ctx));
            }
        }
        "start-bitstransfer" => {
            let pos = positionals(w, 1, &["-source", "-destination", "-displayname", "-description", "-priority", "-transfertype"]);
            let src = opt(w, &["-source"], true, false).or_else(|| pos.first().cloned());
            let dst = opt(w, &["-destination"], true, false).or_else(|| pos.get(1).cloned());
            if let Some(d) = dst {
                let target = into_dir(&d, src.as_deref().and_then(url_basename), c, ctx);
                out.extend(path(c, ctx, &target, How::Downloaded, src.as_deref()));
            }
        }
        "aria2c" => {
            let url = w.iter().skip(1).find(|x| is_url(x)).cloned();
            let dir = opt(w, &["-d", "--dir"], false, false);
            let name = opt(w, &["-o", "--out"], false, false).or_else(|| url.as_deref().and_then(url_basename));
            if let Some(name) = name {
                let target = match dir {
                    Some(d) => format!("{d}\\{name}"),
                    None => name,
                };
                out.extend(path(c, ctx, &target, How::Downloaded, url.as_deref()));
            }
        }
        "huggingface-cli" | "hf" if sub(1) == "download" => {
            const V: &[&str] = &["--local-dir", "--cache-dir", "--revision", "--repo-type", "--include", "--exclude", "--token"];
            let repo = positionals(w, 2, V).first().cloned();
            match opt(w, &["--local-dir", "--cache-dir"], false, false) {
                Some(d) => out.extend(path(c, ctx, &d, How::Downloaded, repo.as_deref())),
                None => out.push(cache(c, "huggingface")),
            }
        }
        "gdown" => {
            if let Some(f) = opt(w, &["-O", "--output"], false, false) {
                let url = w.iter().skip(1).find(|x| is_url(x)).cloned();
                out.extend(path(c, ctx, &f, How::Downloaded, url.as_deref()));
            }
        }
        "kaggle" if sub(2) == "download" => {
            if let Some(d) = opt(w, &["-p", "--path"], false, false) {
                let what = positionals(w, 3, &["-p", "--path", "-f", "--file", "-d", "--dataset", "-c", "--competition"]).first().cloned();
                out.extend(path(c, ctx, &d, How::Downloaded, what.as_deref()));
            }
        }
        "python" | "python3" | "py" if sub(1) == "-m" && matches!(sub(2).as_str(), "pip" | "pip3") => {
            let rest = Simple { words: w[2..].to_vec(), ..c.clone() };
            out.extend(pip(&rest, ctx, "pip"));
        }
        "pip" | "pip3" | "pipx" => out.extend(pip(c, ctx, "pip")),
        "uv" => match sub(1).as_str() {
            "pip" => {
                let rest = Simple { words: w[1..].to_vec(), ..c.clone() };
                out.extend(pip(&rest, ctx, "uv"));
            }
            "add" | "sync" | "install" | "tool" | "run" | "lock" | "venv" => out.push(cache(c, "uv")),
            _ => {}
        },
        "conda" | "mamba" | "micromamba" => {
            let s = sub(1);
            let env_cmd = s == "env" && matches!(sub(2).as_str(), "create" | "update");
            if env_cmd || matches!(s.as_str(), "install" | "create" | "update" | "upgrade") {
                out.push(cache(c, "conda"));
            }
        }
        "npm" | "pnpm" | "yarn" | "bun" => {
            if matches!(sub(1).as_str(), "install" | "i" | "ci" | "add" | "update" | "upgrade" | "up" | "dlx" | "exec" | "x" | "create" | "init") {
                out.push(cache(c, "npm"));
            }
        }
        "npx" | "bunx" | "pnpx" => out.push(cache(c, "npm")),
        "docker" | "podman" | "docker-compose" | "nerdctl" => {
            if matches!(sub(1).as_str(), "pull" | "run" | "build" | "create" | "compose" | "buildx" | "up" | "load" | "import") {
                out.push(cache(c, "docker"));
            }
        }
        "wsl" => out.push(cache(c, "wsl")),
        "cargo" => {
            if matches!(
                sub(1).as_str(),
                "build" | "b" | "install" | "fetch" | "add" | "test" | "t" | "run" | "r" | "update" | "check" | "c" | "clippy" | "doc" | "bench" | "vendor"
            ) {
                out.push(cache(c, "cargo"));
            }
        }
        "scp" | "pscp" | "rsync" => out.extend(copy_from_remote(c, ctx)),
        "expand-archive" => {
            let pos = positionals(w, 1, &["-path", "-literalpath", "-destinationpath"]);
            let src = opt(w, &["-path", "-literalpath"], true, false).or_else(|| pos.first().cloned());
            if let Some(d) = opt(w, &["-destinationpath"], true, false).or_else(|| pos.get(1).cloned()) {
                out.extend(path(c, ctx, &d, How::Extracted, src.as_deref()));
            }
        }
        "tar" | "bsdtar" => {
            let extracting = w.iter().skip(1).any(|x| {
                x == "--extract" || x == "--get" || (!x.starts_with("--") && x.trim_start_matches('-').contains('x') && !x.contains(['\\', '/', '.']))
            });
            if extracting && let Some(d) = opt(w, &["-C", "--directory"], false, false) {
                let src = opt(w, &["-f", "--file"], false, false);
                out.extend(path(c, ctx, &d, How::Extracted, src.as_deref()));
            }
        }
        "unzip" => {
            if let Some(d) = opt(w, &["-d"], false, false) {
                let src = positionals(w, 1, &["-d"]).first().cloned();
                out.extend(path(c, ctx, &d, How::Extracted, src.as_deref()));
            }
        }
        "7z" | "7za" | "7zr" if matches!(sub(1).as_str(), "x" | "e") => {
            if let Some(d) = w.iter().find_map(|x| x.strip_prefix("-o").filter(|d| !d.is_empty())) {
                out.extend(path(c, ctx, d, How::Extracted, w.get(2).map(String::as_str)));
            }
        }
        "mkdir" | "md" => {
            for d in positionals(w, 1, &["-m", "--mode", "-path", "-name"]).iter().chain(opt(w, &["-path"], true, false).iter()) {
                out.extend(path(c, ctx, d, How::Written, None));
            }
        }
        "new-item" | "ni" => {
            let target = opt(w, &["-path"], true, true)
                .or_else(|| positionals(w, 1, &["-path", "-itemtype", "-type", "-name", "-value"]).first().cloned());
            if let Some(t) = target {
                let full = match opt(w, &["-name"], true, true) {
                    Some(n) => format!("{t}\\{n}"),
                    None => t,
                };
                out.extend(path(c, ctx, &full, How::Written, None));
            }
        }
        "out-file" | "set-content" | "add-content" | "tee-object" | "sc" | "ac" | "tee" => {
            let named = opt(w, &["-filepath", "-path", "-literalpath"], true, true);
            const V: &[&str] = &["-value", "-encoding", "-inputobject", "-width", "-variable"];
            let target = named.or_else(|| positionals(w, 1, V).first().cloned());
            if let Some(t) = target.filter(|t| !is_null_target(t)) {
                out.extend(path(c, ctx, &t, How::Written, None));
            }
        }
        "cp" | "copy" | "copy-item" | "cpi" | "xcopy" | "robocopy" => out.extend(copy_local(c, ctx, &program)),
        _ => {}
    }
    out
}

fn is_null_target(t: &str) -> bool {
    matches!(t.to_lowercase().as_str(), "$null" | "nul" | "/dev/null" | "-" | "nul:")
}

/// Targets of `> file` / `>> file` / `1> file` (also written without a space, `>file`).
fn redirects(c: &Simple, ctx: &Ctx<'_>) -> Vec<Found> {
    let w = &c.words;
    let mut out = Vec::new();
    for (i, word) in w.iter().enumerate().skip(1) {
        let target = match word.as_str() {
            ">" | ">>" | "1>" | "1>>" => w.get(i + 1).map(String::as_str),
            x if x.starts_with(">>") && !x.starts_with(">>&") => Some(&x[2..]),
            x if x.starts_with('>') && !x.starts_with(">&") => Some(&x[1..]),
            _ => None,
        };
        if let Some(t) = target.filter(|t| !t.is_empty() && !is_null_target(t) && !t.starts_with('&')) {
            out.extend(path(c, ctx, t, How::Written, None));
        }
    }
    out
}

/// `dest`, or `dest\name` when `dest` names a folder (ends with a separator, is `.`, or
/// already exists as one).
fn into_dir(dest: &str, name: Option<String>, c: &Simple, ctx: &Ctx<'_>) -> String {
    let Some(name) = name else { return dest.to_owned() };
    let looks_dir = dest.ends_with(['\\', '/']) || dest == "." || dest.ends_with("\\.") || dest.ends_with("/.");
    let existing = resolve(dest, c.cwd.as_deref(), ctx.env).is_some_and(|p| (ctx.is_dir)(&p));
    if looks_dir || existing { format!("{}\\{name}", dest.trim_end_matches(['\\', '/'])) } else { dest.to_owned() }
}

fn base_name(p: &str) -> Option<String> {
    let name = p.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next()?;
    (!name.is_empty() && !name.contains('*') && name != "." && name != "..").then(|| name.to_owned())
}

fn curl(c: &Simple, ctx: &Ctx<'_>) -> Vec<Found> {
    // Short options that take a value (curl 8).
    const VALUE: &str = "AbcCdDeEFHKmoPQrtTuUwxXYyz";
    let w = &c.words;
    let mut files: Vec<String> = Vec::new();
    let mut remote_name = false;
    let mut dir: Option<String> = None;
    let mut urls: Vec<String> = Vec::new();
    let mut i = 1;
    while i < w.len() {
        let a = &w[i];
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (long, None),
            };
            let value = |i: &mut usize| {
                inline.clone().or_else(|| {
                    *i += 1;
                    w.get(*i).cloned()
                })
            };
            match name {
                "output" => files.extend(value(&mut i)),
                "output-dir" => dir = value(&mut i),
                "remote-name" | "remote-name-all" => remote_name = true,
                "url" => urls.extend(value(&mut i)),
                "header" | "data" | "data-raw" | "data-binary" | "user" | "request" | "user-agent" | "referer" | "max-time"
                | "proxy" | "upload-file" | "form" | "cookie" | "cookie-jar" | "config" | "range" | "write-out"
                | "connect-timeout" | "retry" | "cacert" | "cert" | "key" | "resolve" | "oauth2-bearer" => {
                    let _ = value(&mut i);
                }
                _ => {}
            }
        } else if a.starts_with('-') && a.len() > 1 {
            let chars: Vec<char> = a.chars().skip(1).collect();
            for (k, ch) in chars.iter().enumerate() {
                if *ch == 'O' {
                    remote_name = true;
                } else if VALUE.contains(*ch) {
                    let rest: String = chars[k + 1..].iter().collect();
                    let v = if rest.is_empty() {
                        i += 1;
                        w.get(i).cloned()
                    } else {
                        Some(rest)
                    };
                    if *ch == 'o' {
                        files.extend(v);
                    }
                    break;
                }
            }
        } else if is_url(a) {
            urls.push(a.clone());
        }
        i += 1;
    }
    if remote_name {
        files.extend(urls.iter().filter_map(|u| url_basename(u)));
    }
    let source = urls.first().cloned();
    files
        .into_iter()
        .filter(|f| !is_null_target(f))
        .filter_map(|f| {
            let target = match &dir {
                Some(d) => format!("{d}\\{f}"),
                None => f,
            };
            path(c, ctx, &target, How::Downloaded, source.as_deref())
        })
        .collect()
}

fn wget(c: &Simple, ctx: &Ctx<'_>) -> Vec<Found> {
    let w = &c.words;
    let url = w.iter().skip(1).find(|x| is_url(x)).cloned();
    let file = opt(w, &["-O", "--output-document"], false, false)
        .or_else(|| w.iter().find_map(|x| x.strip_prefix("-O").filter(|f| !f.is_empty()).map(str::to_owned)));
    let dir = opt(w, &["-P", "--directory-prefix"], false, false);
    let recursive = has(w, &["-r", "--recursive", "-m", "--mirror"]);
    let target = match (file, dir, recursive) {
        (Some(f), _, _) if !is_null_target(&f) => Some(f),
        (Some(_), _, _) => None,
        (None, Some(d), true) => Some(d),
        (None, d, false) => url.as_deref().and_then(url_basename).map(|n| match d {
            Some(d) => format!("{d}\\{n}"),
            None => n,
        }),
        (None, None, true) => None,
    };
    target.and_then(|t| path(c, ctx, &t, How::Downloaded, url.as_deref())).into_iter().collect()
}

fn pip(c: &Simple, ctx: &Ctx<'_>, which: &'static str) -> Vec<Found> {
    let w = &c.words;
    let mut out = Vec::new();
    let s = w.get(1).map(|s| s.to_lowercase()).unwrap_or_default();
    let dest = match s.as_str() {
        "install" => opt(w, &["-t", "--target"], false, false),
        "download" => opt(w, &["-d", "--dest"], false, false),
        _ => return out,
    };
    out.push(cache(c, which));
    if let Some(d) = dest {
        out.extend(path(c, ctx, &d, How::Downloaded, None));
    }
    out
}

fn copy_from_remote(c: &Simple, ctx: &Ctx<'_>) -> Vec<Found> {
    const V: &[&str] = &["-p", "-i", "-o", "-f", "-c", "-l", "-j", "-s", "-e", "--rsh", "--exclude", "--include", "--port", "-pw"];
    let pos = positionals(&c.words, 1, V);
    let Some((dest, sources)) = pos.split_last() else { return Vec::new() };
    if is_remote_spec(dest) {
        return Vec::new();
    }
    let remote: Vec<&String> = sources.iter().filter(|s| is_remote_spec(s)).collect();
    remote
        .iter()
        .filter_map(|src| {
            let name = base_name(src.split_once(':').map(|(_, p)| p).unwrap_or(src));
            let target = if sources.len() > 1 {
                format!("{}\\{}", dest.trim_end_matches(['\\', '/']), name?)
            } else {
                into_dir(dest, name, c, ctx)
            };
            path(c, ctx, &target, How::Downloaded, Some(src.as_str()))
        })
        .collect()
}

/// `cp` / `Copy-Item` / `copy` / `xcopy` / `robocopy`: the copy at the destination.
fn copy_local(c: &Simple, ctx: &Ctx<'_>, program: &str) -> Vec<Found> {
    let w = &c.words;
    let (sources, dest) = match program {
        "copy-item" | "cpi" => {
            let pos = positionals(w, 1, &["-path", "-literalpath", "-destination", "-filter", "-include", "-exclude"]);
            let src = opt(w, &["-path", "-literalpath"], true, false).or_else(|| pos.first().cloned());
            let dst = opt(w, &["-destination"], true, false).or_else(|| pos.get(1).cloned());
            (src.into_iter().collect::<Vec<_>>(), dst)
        }
        "robocopy" => {
            // robocopy <source dir> <dest dir> [files]: the destination folder.
            let pos: Vec<String> = w.iter().skip(1).filter(|x| !x.starts_with('/')).cloned().collect();
            return pos.get(1).and_then(|d| path(c, ctx, d, How::Written, pos.first().map(String::as_str))).into_iter().collect();
        }
        _ => {
            let pos: Vec<String> = positionals(w, 1, &["-t", "--target-directory", "-s", "--suffix"])
                .into_iter()
                .filter(|x| !x.starts_with('/') || x.len() > 3)
                .collect();
            match pos.split_last() {
                Some((d, s)) if !s.is_empty() => (s.to_vec(), Some(d.clone())),
                _ => (Vec::new(), None),
            }
        }
    };
    let Some(dest) = dest else { return Vec::new() };
    if is_remote_spec(&dest) || sources.iter().any(|s| is_remote_spec(s)) {
        return Vec::new();
    }
    if sources.len() > 1 {
        let dir = dest.trim_end_matches(['\\', '/']);
        return sources.iter().filter_map(|s| path(c, ctx, &format!("{dir}\\{}", base_name(s)?), How::Written, None)).collect();
    }
    let target = into_dir(&dest, sources.first().and_then(|s| base_name(s)), c, ctx);
    path(c, ctx, &target, How::Written, None).into_iter().collect()
}

/// Downloads written as code rather than a command line (Python, .NET) anywhere in `text`.
/// Skipped when the text runs things over `ssh` (that code runs on another machine).
pub fn find_in_text(text: &str, cwd: Option<&str>, ctx: &Ctx<'_>, simple: &[Simple]) -> Vec<Found> {
    if simple.iter().any(|s| matches!(program_name(s.words.first().map(String::as_str).unwrap_or("")).as_str(), "ssh" | "plink")) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let summary = |at: usize| {
        let start = text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let end = text[at..].find('\n').map(|i| at + i).unwrap_or(text.len());
        redact::summary(&text[start..end])
    };
    for needle in [".DownloadFile(", "urlretrieve(", "download_url_to_file("] {
        for (at, _) in text.match_indices(needle) {
            let args = quoted_args(&text[at + needle.len()..]);
            if let Some(path) = args.get(1).and_then(|p| resolve(p, cwd, ctx.env)) {
                let source = args.first().map(|s| redact::summary(s));
                out.push(Found::Path { path, how: How::Downloaded, source, summary: summary(at) });
            }
        }
    }
    for needle in ["snapshot_download(", "hf_hub_download("] {
        for (at, _) in text.match_indices(needle) {
            let call = &text[at..text[at..].find(')').map(|i| at + i).unwrap_or(text.len())];
            let dir = keyword_arg(call, "local_dir").or_else(|| keyword_arg(call, "cache_dir"));
            match dir.and_then(|d| resolve(&d, cwd, ctx.env)) {
                Some(path) => {
                    let source = quoted_args(&call[needle.len()..]).first().map(|s| redact::summary(s));
                    out.push(Found::Path { path, how: How::Downloaded, source, summary: summary(at) });
                }
                None => out.push(Found::Cache { cache: "huggingface", summary: summary(at) }),
            }
        }
    }
    for (needle, cache) in [(".from_pretrained(", "huggingface"), ("load_dataset(", "huggingface"), ("torch.hub.load(", "torch")] {
        if let Some(at) = text.find(needle) {
            out.push(Found::Cache { cache, summary: summary(at) });
        }
    }
    out
}

/// String literals at the start of an argument list, in order (stops at the closing paren).
fn quoted_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars();
    let mut depth = 0;
    while let Some(c) = chars.next() {
        match c {
            ')' if depth == 0 => break,
            '(' => depth += 1,
            ')' => depth -= 1,
            '\'' | '"' => {
                let mut v = String::new();
                for d in chars.by_ref() {
                    if d == c {
                        break;
                    }
                    v.push(d);
                }
                out.push(v);
            }
            _ => {}
        }
    }
    out
}

/// `name="value"` / `name='value'` / `name=r"value"` inside a call.
fn keyword_arg(call: &str, name: &str) -> Option<String> {
    let at = call.find(&format!("{name}="))?;
    let rest = call[at + name.len() + 1..].trim_start().trim_start_matches(['r', 'f']);
    let q = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let body = &rest[1..];
    Some(body[..body.find(q)?].to_owned())
}

#[cfg(test)]
mod tests {
    use super::super::shell::commands;
    use super::*;
    use agents::model::Shell;

    fn env() -> PathEnv {
        PathEnv::from_agents_env(&agents::Env::with_home(r"C:\Users\u"))
    }

    fn found(text: &str, shell: Shell) -> Vec<Found> {
        let e = env();
        let is_dir = |p: &str| p.eq_ignore_ascii_case(r"D:\w\results");
        let ctx = Ctx { env: &e, is_dir: &is_dir };
        let cmds = commands(text, &shell, Some(r"D:\w"), &e);
        let mut out: Vec<Found> = cmds.iter().flat_map(|c| find(c, &ctx)).collect();
        out.extend(find_in_text(text, Some(r"D:\w"), &ctx, &cmds));
        out
    }

    fn paths(text: &str, shell: Shell) -> Vec<(String, How)> {
        found(text, shell)
            .into_iter()
            .filter_map(|f| if let Found::Path { path, how, .. } = f { Some((path, how)) } else { None })
            .collect()
    }

    fn caches(text: &str, shell: Shell) -> Vec<&'static str> {
        found(text, shell).into_iter().filter_map(|f| if let Found::Cache { cache, .. } = f { Some(cache) } else { None }).collect()
    }

    #[test]
    fn clones_and_web_downloads() {
        use How::*;
        let ps = || Shell::PowerShell;
        assert_eq!(paths("git clone --depth 1 https://github.com/a/repo.git", ps()), [(r"D:\w\repo".into(), Cloned)]);
        assert_eq!(paths("cd sub; git clone git@github.com:a/b.git c", ps()), [(r"D:\w\sub\c".into(), Cloned)]);
        assert_eq!(paths(r"git -C D:\x clone https://h/y", ps()), [(r"D:\x\y".into(), Cloned)]);
        assert_eq!(paths("gh repo clone owner/name", ps()), [(r"D:\w\name".into(), Cloned)]);
        assert_eq!(paths("curl.exe -sSLo out.bin https://h/f", ps()), [(r"D:\w\out.bin".into(), Downloaded)]);
        assert_eq!(paths("curl -L -O https://h/dir/model.pt?x=1", Shell::Bash), [(r"D:\w\model.pt".into(), Downloaded)]);
        assert_eq!(paths("curl https://h/x --output-dir /d/dl -O", Shell::Bash), [(r"D:\dl\x".into(), Downloaded)]);
        assert_eq!(paths("curl -s https://api/x | jq .", Shell::Bash), []);
        let iwr = "Invoke-WebRequest -Uri https://h/a.zip -OutFile $env:USERPROFILE\\a.zip";
        assert_eq!(paths(iwr, ps()), [(r"C:\Users\u\a.zip".into(), Downloaded)]);
        assert_eq!(paths("iwr https://h/a.zip -OutF a.zip", ps()), [(r"D:\w\a.zip".into(), Downloaded)]);
        assert_eq!(paths("wget -q https://h/data.tar.gz", Shell::Bash), [(r"D:\w\data.tar.gz".into(), Downloaded)]);
        assert_eq!(paths("wget -o log.txt -O f.bin https://h/x", Shell::Bash), [(r"D:\w\f.bin".into(), Downloaded)]);
        assert_eq!(paths("aria2c -d D:\\m -o w.safetensors https://h/x", ps()), [(r"D:\m\w.safetensors".into(), Downloaded)]);
        let dotnet = "(New-Object Net.WebClient).DownloadFile('https://h/x.exe', 'C:\\t\\x.exe')";
        assert_eq!(paths(dotnet, ps()), [(r"C:\t\x.exe".into(), Downloaded)]);
    }

    #[test]
    fn copies_unpacking_and_models() {
        use How::*;
        let ps = || Shell::PowerShell;
        assert_eq!(paths("scp box:/root/out/results.tar.gz .", ps()), [(r"D:\w\results.tar.gz".into(), Downloaded)]);
        assert_eq!(paths("scp -P 22 box:/r/a.log results", ps()), [(r"D:\w\results\a.log".into(), Downloaded)]);
        assert_eq!(paths("scp -r box:/r/run1 run1_local", ps()), [(r"D:\w\run1_local".into(), Downloaded)]);
        assert_eq!(paths(r"scp D:\w\x.py box:/root/", ps()), [], "uploads are not downloads");
        assert_eq!(paths("ssh box 'git clone https://h/r'", ps()), []);
        assert_eq!(paths("Expand-Archive a.zip -DestinationPath out", ps()), [(r"D:\w\out".into(), Extracted)]);
        assert_eq!(paths("tar -xzf d.tgz -C data", Shell::Bash), [(r"D:\w\data".into(), Extracted)]);
        assert_eq!(paths("tar -czf d.tgz -C data .", Shell::Bash), [], "creating an archive is not unpacking");
        let hf = "huggingface-cli download org/m --local-dir models/m";
        assert_eq!(paths(hf, ps()), [(r"D:\w\models\m".into(), Downloaded)]);
        assert_eq!(caches("huggingface-cli download org/m", ps()), ["huggingface"]);
        let snap = "python -c \"from huggingface_hub import snapshot_download; snapshot_download('o/m', local_dir=r'D:\\m')\"";
        assert_eq!(paths(snap, ps()), [(r"D:\m".into(), Downloaded)]);
        let pretrained = "python -c \"from transformers import AutoModel; AutoModel.from_pretrained('o/m')\"";
        assert_eq!(caches(pretrained, ps()), ["huggingface"]);
    }

    #[test]
    fn plain_writes() {
        use How::*;
        assert_eq!(paths("python gen.py > out\\list.txt 2>&1", Shell::PowerShell), [(r"D:\w\out\list.txt".into(), Written)]);
        assert_eq!(paths("echo hi >log.txt; cmd 2>$null", Shell::PowerShell), [(r"D:\w\log.txt".into(), Written)]);
        assert_eq!(paths("Get-Process | Out-File -FilePath p.txt", Shell::PowerShell), [(r"D:\w\p.txt".into(), Written)]);
        let ni = "New-Item -ItemType Directory -Force -Path D:\\runs\\a | Out-Null";
        assert_eq!(paths(ni, Shell::PowerShell), [(r"D:\runs\a".into(), Written)]);
        assert_eq!(paths("mkdir -p build/out", Shell::Bash), [(r"D:\w\build\out".into(), Written)]);
        assert_eq!(paths("cp -r ckpt results", Shell::Bash), [(r"D:\w\results\ckpt".into(), Written)]);
        assert_eq!(paths("Copy-Item a.bin -Destination D:\\b.bin", Shell::PowerShell), [(r"D:\b.bin".into(), Written)]);
        assert_eq!(paths("Write-Output x | Out-Null", Shell::PowerShell), []);
    }

    #[test]
    fn shared_caches() {
        let ps = || Shell::PowerShell;
        assert_eq!(caches("pip install -r requirements.txt", ps()), ["pip"]);
        assert_eq!(caches("python -m pip install torch", ps()), ["pip"]);
        assert_eq!(caches("uv pip install x; uv sync", Shell::Bash), ["uv", "uv"]);
        assert_eq!(caches("conda env create -f e.yml", Shell::Bash), ["conda"]);
        assert_eq!(caches("npm ci && npx tsc", Shell::Bash), ["npm", "npm"]);
        assert_eq!(caches("docker pull ubuntu:24.04", Shell::Bash), ["docker"]);
        assert_eq!(caches("wsl -e bash -lc 'git clone https://h/r'", ps()), ["wsl"]);
        assert_eq!(paths("wsl -e bash -lc 'git clone https://h/r'", ps()), [], "inside WSL, paths are not Windows paths");
        assert_eq!(caches("cargo build --release", ps()), ["cargo"]);
        assert_eq!(caches("git status; ls", Shell::Bash), Vec::<&str>::new());
        assert_eq!(paths("pip install -t vendor requests", ps()), [(r"D:\w\vendor".into(), How::Downloaded)]);
    }

    #[test]
    fn summaries_are_redacted() {
        let f = found("git clone https://u:tok@github.com/a/b.git", Shell::PowerShell);
        let Found::Path { summary, source, .. } = &f[0] else { panic!() };
        assert_eq!(summary, "git clone https://***@github.com/a/b.git");
        assert_eq!(source.as_deref(), Some("https://***@github.com/a/b.git"));
    }
}
