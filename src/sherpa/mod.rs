//! The sherpa-onnx C API, loaded at run time rather than linked.
//!
//! Nothing here is a build dependency: `cargo build` works with no native
//! artifacts present. [`Sherpa::load`] finds the two shared objects and
//! [`Sherpa::recognizer`] creates an offline transducer recognizer.
//!
//! The binding is pinned to sherpa-onnx 1.13.8 in `sherpa.lock`. The struct
//! layouts below this version differ, so the `size_of` assertions in this module
//! are not decoration: a mismatched library is undefined behaviour, and
//! `doctor` reports the version it actually loaded.

mod bindings;

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use libloading::Library;

pub use bindings::*;

/// Feature dimension Parakeet's NeMo frontend expects.
const FEATURE_DIM: i32 = 80;
/// The model type string that makes sherpa treat the joiner as a TDT joiner.
const NEMO_TRANSDUCER: &str = "nemo_transducer";

type GetVersion = unsafe extern "C" fn() -> *const std::os::raw::c_char;
type CreateRecognizer = unsafe extern "C" fn(
    *const SherpaOnnxOfflineRecognizerConfig,
) -> *const SherpaOnnxOfflineRecognizer;
type DestroyRecognizer = unsafe extern "C" fn(*const SherpaOnnxOfflineRecognizer);
type CreateStream =
    unsafe extern "C" fn(*const SherpaOnnxOfflineRecognizer) -> *const SherpaOnnxOfflineStream;
type DestroyStream = unsafe extern "C" fn(*const SherpaOnnxOfflineStream);
type AcceptWaveform = unsafe extern "C" fn(*const SherpaOnnxOfflineStream, i32, *const f32, i32);
type DecodeStream =
    unsafe extern "C" fn(*const SherpaOnnxOfflineRecognizer, *const SherpaOnnxOfflineStream);
type GetResult = unsafe extern "C" fn(
    *const SherpaOnnxOfflineStream,
) -> *const SherpaOnnxOfflineRecognizerResult;
type DestroyResult = unsafe extern "C" fn(*const SherpaOnnxOfflineRecognizerResult);

struct Fns {
    get_version: GetVersion,
    create_recognizer: CreateRecognizer,
    destroy_recognizer: DestroyRecognizer,
    create_stream: CreateStream,
    destroy_stream: DestroyStream,
    accept_waveform: AcceptWaveform,
    decode_stream: DecodeStream,
    get_result: GetResult,
    destroy_result: DestroyResult,
}

/// The loaded library and its function table.
pub struct Sherpa {
    // Declared before `fns` so it outlives the pointers taken from it.
    _lib: Library,
    fns: Fns,
    path: PathBuf,
}

fn library_file() -> &'static str {
    if cfg!(windows) {
        "sherpa-onnx-c-api.dll"
    } else {
        "libsherpa-onnx-c-api.so"
    }
}

/// Find the runtime, in the same order `doctor` reports.
///
/// 1. `HIPPO_VOICE_SHERPA_DIR`
/// 2. `lib/` beside the executable
/// 3. `vendor/sherpa/lib` relative to the source tree, for development
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
///
/// The packaged layout comes before the source tree on purpose: a developer who
/// has both should exercise what users get, not a stale vendored copy.
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

/// Open the shared object.
///
/// `Library::new` passes no flags to `LoadLibraryExW`, so Windows resolves the
/// DLL's imports from the *application* directory and `PATH` - never from the
/// directory of the DLL that imports it. Linux is saved by `$ORIGIN`; Windows
/// needs `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR`, or `sherpa-onnx-c-api.dll` loads and
/// then fails on `onnxruntime.dll`.
fn open_library(path: &Path) -> Result<Library> {
    #[cfg(windows)]
    {
        use libloading::os::windows::{
            Library as WindowsLibrary, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        };

        let flags = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;
        // SAFETY: the returned library is held in `Sherpa` for as long as any
        // symbol taken from it is used.
        let library = unsafe { WindowsLibrary::load_with_flags(path, flags) }
            .with_context(|| dependency_hint(path))?;
        Ok(Library::from(library))
    }

    #[cfg(not(windows))]
    {
        // SAFETY: as above.
        unsafe { Library::new(path) }.with_context(|| dependency_hint(path))
    }
}

fn dependency_hint(path: &Path) -> String {
    let hint = format!(
        "load {} - its dependencies must sit beside it (onnxruntime.dll on Windows)",
        path.display()
    );
    #[cfg(windows)]
    {
        // sherpa-onnx-c-api.dll and onnxruntime.dll are C++ and import
        // vcruntime140/msvcp140; without them the load fails with "the specified
        // module could not be found", which reads like a missing sherpa DLL.
        return format!(
            "{hint}. If the DLL is present, install the Microsoft Visual C++ \
             Redistributable (x64): the runtime needs vcruntime140 and msvcp140"
        );
    }
    #[cfg(not(windows))]
    {
        hint
    }
}

impl Sherpa {
    pub fn load(path: &Path) -> Result<Arc<Self>> {
        // SAFETY: `Library` keeps the code mapped for as long as `fns` lives,
        // and every symbol is required for the recognizer to work at all.
        let lib = open_library(path)?;

        unsafe {
            macro_rules! sym {
                ($name:literal, $ty:ty) => {{
                    let symbol: libloading::Symbol<$ty> = lib
                        .get($name)
                        .with_context(|| format!("resolve {}", String::from_utf8_lossy($name)))?;
                    *symbol
                }};
            }
            let fns = Fns {
                get_version: sym!(b"SherpaOnnxGetVersionStr\0", GetVersion),
                create_recognizer: sym!(b"SherpaOnnxCreateOfflineRecognizer\0", CreateRecognizer),
                destroy_recognizer: sym!(
                    b"SherpaOnnxDestroyOfflineRecognizer\0",
                    DestroyRecognizer
                ),
                create_stream: sym!(b"SherpaOnnxCreateOfflineStream\0", CreateStream),
                destroy_stream: sym!(b"SherpaOnnxDestroyOfflineStream\0", DestroyStream),
                accept_waveform: sym!(b"SherpaOnnxAcceptWaveformOffline\0", AcceptWaveform),
                decode_stream: sym!(b"SherpaOnnxDecodeOfflineStream\0", DecodeStream),
                get_result: sym!(b"SherpaOnnxGetOfflineStreamResult\0", GetResult),
                destroy_result: sym!(b"SherpaOnnxDestroyOfflineRecognizerResult\0", DestroyResult),
            };
            Ok(Arc::new(Self {
                _lib: lib,
                fns,
                path: path.to_path_buf(),
            }))
        }
    }

    /// Load from the resolved location.
    pub fn open() -> Result<Arc<Self>> {
        let path = resolve_library()?;
        Self::load(&path)
    }

    pub fn version(&self) -> String {
        unsafe {
            let ptr = (self.fns.get_version)();
            if ptr.is_null() {
                return "unknown".to_string();
            }
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create an offline transducer recognizer for a Parakeet model directory.
    pub fn recognizer(self: &Arc<Self>, model_dir: &Path, threads: i32) -> Result<Recognizer> {
        let encoder = model_dir.join("encoder.int8.onnx");
        let decoder = model_dir.join("decoder.int8.onnx");
        let joiner = model_dir.join("joiner.int8.onnx");
        let tokens = model_dir.join("tokens.txt");
        for file in [&encoder, &decoder, &joiner, &tokens] {
            if !file.is_file() {
                bail!("missing model file {}", file.display());
            }
        }

        let encoder = cpath(&encoder);
        let decoder = cpath(&decoder);
        let joiner = cpath(&joiner);
        let tokens = cpath(&tokens);
        let model_type = CString::new(NEMO_TRANSDUCER).expect("static string");
        let decoding = CString::new("greedy_search").expect("static string");
        let provider = CString::new("cpu").expect("static string");
        // These two are non-empty in sherpa's own defaults. A null here reaches
        // `std::string` in the C++ layer and crashes inside the create call.
        let modeling_unit = CString::new("cjkchar").expect("static string");
        let bpe_vocab = CString::new("").expect("static string");

        // SAFETY: a C config is all-zero initialisation followed by field
        // writes; the pointers it holds stay alive until the create call
        // returns, which is all sherpa requires (it copies them).
        let mut config: SherpaOnnxOfflineRecognizerConfig = unsafe { std::mem::zeroed() };
        config.feat_config.sample_rate = crate::audio::SAMPLE_RATE as i32;
        config.feat_config.feature_dim = FEATURE_DIM;
        config.model_config.transducer.encoder = encoder.as_ptr();
        config.model_config.transducer.decoder = decoder.as_ptr();
        config.model_config.transducer.joiner = joiner.as_ptr();
        config.model_config.tokens = tokens.as_ptr();
        config.model_config.model_type = model_type.as_ptr();
        config.model_config.provider = provider.as_ptr();
        config.model_config.num_threads = threads.max(1);
        config.model_config.modeling_unit = modeling_unit.as_ptr();
        config.model_config.bpe_vocab = bpe_vocab.as_ptr();
        config.decoding_method = decoding.as_ptr();
        config.max_active_paths = 4;

        let handle = unsafe { (self.fns.create_recognizer)(&config) };
        if handle.is_null() {
            bail!(
                "sherpa-onnx could not create a recognizer for {}",
                model_dir.display()
            );
        }
        Ok(Recognizer {
            sherpa: Arc::clone(self),
            handle,
        })
    }
}

fn cpath(path: &Path) -> CString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        CString::new(path.as_os_str().as_bytes()).expect("path has no interior nul")
    }
    #[cfg(not(unix))]
    {
        CString::new(path.to_string_lossy().as_bytes()).expect("path has no interior nul")
    }
}

/// One recognizer. Dropping it frees the native handle.
///
/// It owns an `Arc<Sherpa>` rather than borrowing one: a borrow would point at
/// the stack slot the `Sherpa` was built in, and moving that value into a tuple
/// or a thread would leave the recognizer holding a dangling reference.
pub struct Recognizer {
    sherpa: Arc<Sherpa>,
    handle: *const SherpaOnnxOfflineRecognizer,
}

// The handle is only used under `&self`, and sherpa's recognizer is safe to
// share across threads for decoding distinct streams.
unsafe impl Send for Recognizer {}

impl Drop for Recognizer {
    fn drop(&mut self) {
        unsafe { (self.sherpa.fns.destroy_recognizer)(self.handle) }
    }
}

impl Recognizer {
    /// Decode one clip of 16 kHz mono f32.
    pub fn transcribe(&self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let len = i32::try_from(samples.len()).context("clip longer than i32 samples")?;
        let fns = &self.sherpa.fns;

        // SAFETY: every handle is created and destroyed within this function,
        // and the sample slice outlives the accept call.
        unsafe {
            let stream = (fns.create_stream)(self.handle);
            if stream.is_null() {
                bail!("sherpa-onnx could not create a stream");
            }
            (fns.accept_waveform)(
                stream,
                crate::audio::SAMPLE_RATE as i32,
                samples.as_ptr(),
                len,
            );
            (fns.decode_stream)(self.handle, stream);

            let result = (fns.get_result)(stream);
            let text = if result.is_null() {
                String::new()
            } else {
                let ptr = (*result).text;
                let text = if ptr.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(ptr).to_string_lossy().trim().to_string()
                };
                (fns.destroy_result)(result);
                text
            };
            (fns.destroy_stream)(stream);
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    /// Sizes measured from the pinned header with `cc -I include`. If these
    /// change, the vendored library and these bindings are no longer the same
    /// ABI and every decode would be undefined behaviour.
    #[test]
    fn the_bindings_match_the_pinned_abi() {
        assert_eq!(size_of::<SherpaOnnxFeatureConfig>(), 8);
        assert_eq!(size_of::<SherpaOnnxOfflineTransducerModelConfig>(), 24);
        assert_eq!(size_of::<SherpaOnnxOfflineModelConfig>(), 504);
        assert_eq!(size_of::<SherpaOnnxOfflineRecognizerConfig>(), 608);
        assert_eq!(size_of::<SherpaOnnxOfflineRecognizerResult>(), 128);
    }

    /// Needs the runtime and the model on disk; run it by hand.
    #[test]
    #[ignore]
    fn the_vendored_runtime_loads() {
        let sherpa = Sherpa::open().expect("load sherpa");
        assert!(!sherpa.version().is_empty());
    }
}

#[cfg(test)]
mod resolution {
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

        /// Create a file at `relative`: that is the layout the loader looks for.
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

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn the_release_layout_beside_the_executable_is_found() {
        let tree = Tree::new("release");
        let expected = tree.touch(&format!("app/{}", library_file()));

        let found = resolve_library_in(&no_env, Some(&tree.0.join("app")), &tree.0.join("src"))
            .expect("resolve");
        assert_eq!(found, expected);
    }

    #[test]
    fn a_lib_subdirectory_beside_the_executable_also_works() {
        let tree = Tree::new("lib");
        let expected = tree.touch(&format!("app/lib/{}", library_file()));

        let found = resolve_library_in(&no_env, Some(&tree.0.join("app")), &tree.0.join("src"))
            .expect("resolve");
        assert_eq!(found, expected);
    }

    #[test]
    fn the_source_tree_is_the_development_fallback() {
        let tree = Tree::new("dev");
        let expected = tree.touch(&format!("src/vendor/sherpa/lib/{}", library_file()));

        let found = resolve_library_in(
            &no_env,
            Some(&tree.0.join("target/debug")),
            &tree.0.join("src"),
        )
        .expect("resolve");
        assert_eq!(found, expected);
    }

    #[test]
    fn an_explicit_override_wins_over_everything_else() {
        let tree = Tree::new("override");
        tree.touch(&format!("app/{}", library_file()));
        tree.touch(&format!("src/vendor/sherpa/lib/{}", library_file()));
        let expected = tree.touch(&format!("elsewhere/lib/{}", library_file()));

        let env = |key: &str| {
            (key == "HIPPO_VOICE_SHERPA_DIR").then(|| OsString::from(tree.0.join("elsewhere")))
        };
        let found = resolve_library_in(&env, Some(&tree.0.join("app")), &tree.0.join("src"))
            .expect("resolve");
        assert_eq!(found, expected);
    }

    #[test]
    fn nothing_anywhere_says_how_to_fix_it() {
        let tree = Tree::new("missing");
        let error = resolve_library_in(&no_env, Some(&tree.0.join("app")), &tree.0.join("src"))
            .expect_err("should not resolve");
        let text = error.to_string();
        assert!(text.contains("fetch-sherpa"), "{text}");
        assert!(text.contains("HIPPO_VOICE_SHERPA_DIR"), "{text}");
    }
}
