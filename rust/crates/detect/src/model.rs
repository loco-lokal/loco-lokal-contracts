//! The field detection model (`Templates::ImageToFields.model`): an ONNX model run by
//! ONNX Runtime through `ort`, loaded once per process.
//!
//! The model file is `ONNX_MODEL_PATH`, else `tmp/model.onnx` in the app root (where the
//! Docker image puts it for Rails), else `rust/vendor/onnx/model.onnx`. ONNX Runtime is
//! loaded at runtime from `ORT_DYLIB_PATH`, else `rust/vendor/onnx/lib`, else the
//! system's `libonnxruntime.so.1` (Alpine's package in the Docker image).
//! `rust/scripts/fetch-onnx.sh` downloads both into `rust/vendor/onnx`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ort::{
    environment::Environment,
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};

use crate::DetectError;

/// `INPUT_NAMES`: the name of the image input.
const INPUT_NAMES: [&str; 2] = ["images", "input"];

fn vendor_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/onnx")
}

/// The model file: `ONNX_MODEL_PATH`, `$APP_ROOT/tmp/model.onnx`, then the vendored one.
#[must_use]
pub fn model_path(app_root: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("ONNX_MODEL_PATH") {
        return PathBuf::from(path);
    }
    let rails = app_root.join("tmp/model.onnx");
    if rails.exists() {
        return rails;
    }
    vendor_dir().join("model.onnx")
}

/// The ONNX Runtime library: `ORT_DYLIB_PATH`, the vendored one, else the system's.
#[must_use]
pub fn library_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ORT_DYLIB_PATH") {
        return PathBuf::from(path);
    }
    let name = if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    };
    let vendored = vendor_dir().join("lib").join(name);
    if vendored.exists() {
        vendored
    } else if cfg!(target_os = "macos") {
        PathBuf::from(name)
    } else {
        PathBuf::from("libonnxruntime.so.1")
    }
}

/// Raw outputs of one run of the v2 model, for one image: `boxes` (`[n, 4]`, `x1 y1 x2
/// y2` in input pixels), `labels` and `scores`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outputs {
    pub boxes: Vec<[f32; 4]>,
    pub labels: Vec<i64>,
    pub scores: Vec<f32>,
}

/// A loaded model.
pub struct Model {
    session: Mutex<Session>,
    _environment: Environment,
    /// `resolution`: the image input's height (and width).
    pub resolution: usize,
    /// `model_v2?`: the model takes `orig_target_sizes` and returns `boxes`, `labels`
    /// and `scores`.
    pub v2: bool,
}

static SHARED: OnceLock<Result<Model, String>> = OnceLock::new();

impl Model {
    /// The process-wide model, loaded from [`model_path`] on first use.
    pub fn shared(app_root: &Path) -> Result<&'static Self, DetectError> {
        SHARED
            .get_or_init(|| {
                Self::load(&library_path(), &model_path(app_root)).map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| DetectError::Model(e.clone()))
    }

    /// `OnnxRuntime::Model.new(MODEL_PATH, inter_op_num_threads: 1, intra_op_num_threads:
    /// Etc.nprocessors, enable_mem_pattern: false, providers: ['CPUExecutionProvider'])`.
    pub fn load(library: &Path, model: &Path) -> Result<Self, DetectError> {
        let error = |e: &dyn std::fmt::Display| DetectError::Model(e.to_string());
        let environment = ort::init_from(library)
            .map_err(|e| error(&e))?
            .with_name("loco-detect")
            .build()
            .map_err(|e| error(&e))?;
        let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let session = Session::builder(&environment)
            .map_err(|e| error(&e))?
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(|e| error(&e))?
            .with_inter_threads(1)
            .map_err(|e| error(&e))?
            .with_intra_threads(threads)
            .map_err(|e| error(&e))?
            .with_memory_pattern(false)
            .map_err(|e| error(&e))?
            .commit_from_file(model)
            .map_err(|e| error(&format!("{}: {e}", model.display())))?;
        let input = session
            .inputs()
            .iter()
            .find(|i| INPUT_NAMES.contains(&i.name()))
            .ok_or_else(|| DetectError::Model("the model has no image input".into()))?;
        let resolution = input
            .dtype()
            .tensor_shape()
            .and_then(|shape| shape.get(2).copied())
            .and_then(|d| usize::try_from(d).ok())
            .ok_or_else(|| DetectError::Model("the model's input size is not fixed".into()))?;
        let v2 = session
            .inputs()
            .iter()
            .any(|i| i.name() == "orig_target_sizes");
        Ok(Self {
            session: Mutex::new(session),
            _environment: environment,
            resolution,
            v2,
        })
    }

    /// `model.predict({ 'images' => input, 'orig_target_sizes' => [[r, r]] })` for one
    /// `[3, r, r]` image.
    pub fn predict(&self, planar: Vec<f32>) -> Result<Outputs, DetectError> {
        let error = |e: ort::Error| DetectError::Model(e.to_string());
        let r = self.resolution;
        let size = i64::try_from(r).unwrap_or_default();
        let images = Tensor::from_array(([1usize, 3, r, r], planar)).map_err(error)?;
        let sizes = Tensor::from_array(([1usize, 2], vec![size, size])).map_err(error)?;
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outputs = session
            .run(ort::inputs!["images" => images, "orig_target_sizes" => sizes])
            .map_err(error)?;
        let (_, boxes) = outputs["boxes"]
            .try_extract_tensor::<f32>()
            .map_err(error)?;
        let (_, labels) = outputs["labels"]
            .try_extract_tensor::<i64>()
            .map_err(error)?;
        let (_, scores) = outputs["scores"]
            .try_extract_tensor::<f32>()
            .map_err(error)?;
        Ok(Outputs {
            boxes: boxes.as_chunks::<4>().0.to_vec(),
            labels: labels.to_vec(),
            scores: scores.to_vec(),
        })
    }
}
