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
    let name = library_file();

    let mut candidates = Vec::new();
    if let Some(dir) = std::env::var_os("HIPPO_VOICE_SHERPA_DIR") {
        let dir = PathBuf::from(dir);
        candidates.push(dir.join("lib").join(name));
        candidates.push(dir.join(name));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("lib").join(name));
            candidates.push(dir.join(name));
        }
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("vendor/sherpa/lib")
            .join(name),
    );

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

impl Sherpa {
    pub fn load(path: &Path) -> Result<Self> {
        // SAFETY: `Library` keeps the code mapped for as long as `fns` lives,
        // and every symbol is required for the recognizer to work at all.
        let lib =
            unsafe { Library::new(path) }.with_context(|| format!("load {}", path.display()))?;

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
            Ok(Self {
                _lib: lib,
                fns,
                path: path.to_path_buf(),
            })
        }
    }

    /// Load from the resolved location.
    pub fn open() -> Result<Self> {
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
    pub fn recognizer(&self, model_dir: &Path, threads: i32) -> Result<Recognizer<'_>> {
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
            sherpa: self,
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
pub struct Recognizer<'a> {
    sherpa: &'a Sherpa,
    handle: *const SherpaOnnxOfflineRecognizer,
}

// The handle is only ever used under `&self` and sherpa's recognizer is safe to
// share across threads for decoding distinct streams.
unsafe impl Send for Recognizer<'_> {}

impl Drop for Recognizer<'_> {
    fn drop(&mut self) {
        unsafe { (self.sherpa.fns.destroy_recognizer)(self.handle) }
    }
}

impl Recognizer<'_> {
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
