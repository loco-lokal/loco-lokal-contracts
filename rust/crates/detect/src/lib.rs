//! Field detection with the ONNX model: replaces `lib/templates/detect_fields.rb` and
//! `lib/templates/image_to_fields.rb`.
//!
//! [`model::Model`] runs the model through ONNX Runtime; [`image`] prepares its input
//! as libvips does in Rails; [`inference`] maps the detections back to the page; and
//! [`fields`] links them with the page text (`loco_pdf::pages::detect_page`) and types
//! them by their labels.

pub mod fields;
pub mod image;
pub mod inference;
pub mod model;

pub use fields::{PageNodes, detect_image, detect_pdf_page, render_size};
pub use model::Model;

#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    /// The model or ONNX Runtime failed to load or run.
    #[error("field detection model: {0}")]
    Model(String),
}
