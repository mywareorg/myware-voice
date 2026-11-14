//! The runtime, resolved the way this application installs it.
//!
//! `hippo-sherpa` loads a library at a path it is handed and refuses to search
//! for one, so the layout knowledge lives here: an explicit override, then beside
//! the executable, then its `lib/`, then the source tree for a development build.

pub use hippo_sherpa::asr::*;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

/// Load the runtime from the resolved path.
pub fn open() -> Result<std::sync::Arc<Sherpa>> {
    Sherpa::load(&resolve_library()?)
}

/// Where the runtime is for this user's installation.
///
/// The packaged layout comes before the source tree on purpose: a developer who
/// has both should exercise what users get, not a stale vendored copy.
pub fn resolve_library() -> Result<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    resolve_library_in(
        &|key| std::env::var_os(key),
        exe_dir.as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// The search order, with its inputs passed in so every layout can be tested on
/// any host:
///
/// 1. `HIPPO_VOICE_SHERPA_DIR`, an explicit override for a hand-placed runtime
/// 2. beside the executable, which is where the release folder puts it
/// 3. the executable's `lib/`
/// 4. `vendor/sherpa/lib` in the source tree, for development
pub fn resolve_library_in(
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    exe_dir: Option<&Path>,
    manifest_dir: &Path,
) -> Result<PathBuf> {
    let name = library_file();
    let mut candidates = Vec::new();

    if let Some(dir) = env("HIPPO_VOICE_SHERPA_DIR") {
        let dir = PathBuf::from(dir);
        candidates.push(dir.join("lib").join(name));
        candidates.push(dir.join(name));
    }
    if let Some(dir) = exe_dir {
        candidates.push(dir.join("lib").join(name));
        candidates.push(dir.join(name));
    }
    candidates.push(manifest_dir.join("vendor/sherpa/lib").join(name));

    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "sherpa-onnx runtime not found; run scripts/fetch-sherpa.sh \
                 or set HIPPO_VOICE_SHERPA_DIR"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;

    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("hippo-voice-sherpa-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn touch(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"not a real library").unwrap();
            path
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key| map.get(key).map(OsString::from)
    }

    #[test]
    fn the_override_wins_over_everything() {
        let tree = Tree::new("override");
        let name = library_file();
        let path = tree.touch(&format!("explicit/{name}"));
        let env = env_of(&[(
            "HIPPO_VOICE_SHERPA_DIR",
            tree.0.join("explicit").to_str().unwrap(),
        )]);
        assert_eq!(resolve_library_in(&env, None, &tree.0).unwrap(), path);
    }

    #[test]
    fn the_packaged_layout_beats_the_source_tree() {
        let tree = Tree::new("packaged");
        let name = library_file();
        let exe = tree.0.join("bin");
        let packaged = tree.touch(&format!("bin/lib/{name}"));
        tree.touch(&format!("vendor/sherpa/lib/{name}"));

        let env = env_of(&[]);
        assert_eq!(
            resolve_library_in(&env, Some(&exe), &tree.0).unwrap(),
            packaged
        );
    }

    #[test]
    fn the_source_tree_is_the_development_fallback() {
        let tree = Tree::new("vendor");
        let name = library_file();
        let vendored = tree.touch(&format!("vendor/sherpa/lib/{name}"));

        let env = env_of(&[]);
        assert_eq!(
            resolve_library_in(&env, Some(&tree.0.join("bin")), &tree.0).unwrap(),
            vendored
        );
    }

    #[test]
    fn nothing_found_says_what_to_run() {
        let tree = Tree::new("empty");
        let env = env_of(&[]);
        let error = resolve_library_in(&env, Some(&tree.0), &tree.0).unwrap_err();
        assert!(error.to_string().contains("fetch-sherpa"));
    }
}
