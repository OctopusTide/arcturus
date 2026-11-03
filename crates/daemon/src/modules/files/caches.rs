//! Shared caches: places many conversations fill without one of them owning the files
//! (package and model caches, container and WSL disks). Conversations only record that they
//! used one; the cache's size is measured as a whole.

use std::path::{Path, PathBuf};

use super::paths::{PathEnv, is_within, key};

/// Every cache name a command can be recognised as using (see `downloads`).
pub const NAMES: &[&str] = &["huggingface", "torch", "pip", "uv", "conda", "npm", "docker", "wsl", "cargo"];

/// Cache folders (or disk files) that exist, as `(cache, path)`.
pub fn locations(env: &PathEnv) -> Vec<(&'static str, PathBuf)> {
    let home = PathBuf::from(&env.home);
    let var = |k: &str| env.vars.get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let local = var("LOCALAPPDATA").unwrap_or_else(|| home.join(r"AppData\Local"));
    let roaming = var("APPDATA").unwrap_or_else(|| home.join(r"AppData\Roaming"));
    let xdg = var("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache"));
    let mut out: Vec<(&'static str, PathBuf)> = Vec::new();
    out.push(("huggingface", var("HF_HOME").unwrap_or_else(|| xdg.join("huggingface"))));
    if let Some(hub) = var("HF_HUB_CACHE") {
        out.push(("huggingface", hub));
    }
    out.push(("torch", var("TORCH_HOME").unwrap_or_else(|| xdg.join("torch"))));
    out.push(("pip", var("PIP_CACHE_DIR").unwrap_or_else(|| local.join(r"pip\Cache"))));
    out.push(("uv", var("UV_CACHE_DIR").unwrap_or_else(|| local.join(r"uv\cache"))));
    match env.vars.get("CONDA_PKGS_DIRS").filter(|v| !v.is_empty()) {
        Some(dirs) => out.extend(dirs.split(';').filter(|d| !d.is_empty()).map(|d| ("conda", PathBuf::from(d)))),
        None => {
            for base in ["miniconda3", "anaconda3", "miniforge3", "mambaforge", ".conda"] {
                out.push(("conda", home.join(base).join("pkgs")));
            }
            out.push(("conda", local.join(r"conda\conda\pkgs")));
        }
    }
    out.push(("npm", var("NPM_CONFIG_CACHE").unwrap_or_else(|| local.join("npm-cache"))));
    out.push(("npm", roaming.join("npm-cache")));
    out.push(("npm", local.join(r"pnpm\store")));
    out.push(("npm", local.join(r"Yarn\Cache")));
    out.push(("npm", home.join(r".bun\install\cache")));
    let docker: Vec<PathBuf> = agents::env::glob(&local, &["Docker", "wsl", "*", "*.vhdx"]);
    out.extend(docker.iter().cloned().map(|p| ("docker", p)));
    for p in agents::env::glob(&local, &["Packages", "*", "LocalState", "ext4.vhdx"]).into_iter().chain(agents::env::glob(&local, &["wsl", "*", "ext4.vhdx"])) {
        if !docker.contains(&p) {
            out.push(("wsl", p));
        }
    }
    let cargo = var("CARGO_HOME").unwrap_or_else(|| home.join(".cargo"));
    out.push(("cargo", cargo.join("registry")));
    out.push(("cargo", cargo.join("git")));
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|(_, p)| p.exists() && seen.insert(key(&p.to_string_lossy())));
    out
}

/// The cache a path lies in, if any (a download into a cache only counts as using it).
pub fn containing(path: &str, locations: &[(&'static str, PathBuf)]) -> Option<&'static str> {
    locations.iter().find(|(_, p)| is_within(path, &p.to_string_lossy())).map(|(c, _)| *c)
}

/// The process's own cache variables, for `PathEnv` (tests leave them out).
pub fn process_vars() -> Vec<(String, String)> {
    ["HF_HOME", "HF_HUB_CACHE", "TORCH_HOME", "XDG_CACHE_HOME", "PIP_CACHE_DIR", "UV_CACHE_DIR", "CONDA_PKGS_DIRS", "NPM_CONFIG_CACHE", "CARGO_HOME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| ((*k).to_owned(), v)))
        .collect()
}

pub fn display(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_existing_caches_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        for p in [r".cache\huggingface\hub", r"AppData\Local\pip\Cache", r"AppData\Local\Docker\wsl\disk", r"AppData\Local\Packages\Ubuntu_x\LocalState", r".cargo\registry"] {
            std::fs::create_dir_all(home.join(p)).unwrap();
        }
        std::fs::write(home.join(r"AppData\Local\Docker\wsl\disk\docker_data.vhdx"), b"").unwrap();
        std::fs::write(home.join(r"AppData\Local\Packages\Ubuntu_x\LocalState\ext4.vhdx"), b"").unwrap();
        let env = PathEnv::from_agents_env(&agents::Env::with_home(home));
        let found: Vec<&str> = locations(&env).iter().map(|(c, _)| *c).collect();
        assert_eq!(found, ["huggingface", "pip", "docker", "wsl", "cargo"]);
        let hf = display(&home.join(r".cache\huggingface\hub\models--x"));
        assert_eq!(containing(&hf, &locations(&env)), Some("huggingface"));
        assert_eq!(containing(&display(&home.join("other")), &locations(&env)), None);
    }
}
