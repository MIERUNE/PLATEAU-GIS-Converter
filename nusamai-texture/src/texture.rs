use image::{DynamicImage, GenericImageView};
use std::path::{Path, PathBuf};

pub struct CroppedTexture {
    pub image_path: PathBuf,
    pub u: u32,
    pub v: u32,
    pub width: u32,
    pub height: u32,
}

impl CroppedTexture {
    pub fn new(uv_coords: &[(f32, f32)], image_path: &Path) -> Self {
        let image = image::open(image_path).expect("Failed to open image file");

        let (min_x, min_y, max_x, max_y) = uv_coords.iter().fold(
            (1.0_f32, 1.0_f32, 0.0_f32, 0.0_f32),
            |(min_x, min_y, max_x, max_y), (x, y)| {
                (min_x.min(*x), min_y.min(*y), max_x.max(*x), max_y.max(*y))
            },
        );

        let (width, height) = image.dimensions();

        let left = (min_x * width as f32) as u32;
        let top = (min_y * height as f32) as u32;
        let right = (max_x * width as f32) as u32;
        let bottom = (max_y * height as f32) as u32;

        let cropped_width = right - left;
        let cropped_height = bottom - top;

        CroppedTexture {
            image_path: image_path.to_path_buf(),
            u: left,
            v: top,
            width: cropped_width,
            height: cropped_height,
        }
    }

    pub fn crop(&self) -> DynamicImage {
        let image = image::open(&self.image_path).expect("Failed to open image file");
        let cropped_image = image
            .view(self.u, self.v, self.width, self.height)
            .to_image();
        DynamicImage::ImageRgba8(cropped_image)
    }
}