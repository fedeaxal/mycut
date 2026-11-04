/// Trait for face detection. Users implement this with their preferred face detector.
pub trait FaceDetector: Send + Sync {
    /// Returns 106 face landmarks as [x0, y0, x1, y1, ...] in [0,1] normalized coordinates.
    /// Returns an empty vec if no face is detected.
    fn detect(&self, rgba_data: &[u8], width: u32, height: u32) -> Vec<f32>;
}
