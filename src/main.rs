#![windows_subsystem = "windows"]

use eframe::egui;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::thread;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Photo Selector & Manager",
        options,
        Box::new(|_cc| Box::new(PhotoApp::default())),
    )
}

struct PhotoApp {
    source_dir: Option<PathBuf>,
    target_dir: Option<PathBuf>,
    files: Vec<PathBuf>,

    // Search Cache (Prevents fuzzy-matching 60 times a second)
    search_query: String,
    last_search_query: String,
    filtered_files: Vec<(PathBuf, i64)>,
    matcher: SkimMatcherV2,

    // Asynchronous Image Loading State
    selected_path: Option<PathBuf>,
    loaded_texture: Option<egui::TextureHandle>,
    image_rx: Option<Receiver<(PathBuf, Result<egui::ColorImage, String>)>>,
    is_loading: bool,
    feedback_msg: String,
}

impl Default for PhotoApp {
    fn default() -> Self {
        Self {
            source_dir: None,
            target_dir: None,
            files: Vec::new(),
            search_query: String::new(),
            last_search_query: String::new(),
            filtered_files: Vec::new(),
            matcher: SkimMatcherV2::default(),
            selected_path: None,
            loaded_texture: None,
            image_rx: None,
            is_loading: false,
            feedback_msg: String::new(),
        }
    }
}

impl PhotoApp {
    fn load_directory(&mut self, path: PathBuf) {
        self.files.clear();
        if let Ok(entries) = fs::read_dir(&path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() {
                    if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                        let ext = ext.to_lowercase();
                        if ["png", "jpg", "jpeg", "cr2", "cr3", "raf", "nef", "arw", "dng"]
                            .contains(&ext.as_str())
                        {
                            self.files.push(p);
                        }
                    }
                }
            }
        }
        self.source_dir = Some(path);
        self.selected_path = None;
        self.loaded_texture = None;

        // Force an update to the search cache
        self.update_search_cache();
    }

    fn update_search_cache(&mut self) {
        self.filtered_files.clear();
        for p in &self.files {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if self.search_query.is_empty() {
                    self.filtered_files.push((p.clone(), 0));
                } else if let Some(score) = self.matcher.fuzzy_match(name, &self.search_query) {
                    self.filtered_files.push((p.clone(), score));
                }
            }
        }
        self.filtered_files.sort_by(|a, b| b.1.cmp(&a.1));
        self.last_search_query = self.search_query.clone();
    }

    /// Spawns a background thread to prevent UI freezing
    fn load_image_async(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.is_loading = true;
        self.loaded_texture = None;
        self.feedback_msg.clear();

        let (tx, rx) = channel();
        self.image_rx = Some(rx);
        let ctx_clone = ctx.clone();

        thread::spawn(move || {
            let result = Self::decode_image_to_color_image(&path);
            let _ = tx.send((path, result));
            // Force the UI to wake up and redraw immediately when done
            ctx_clone.request_repaint();
        });
    }

    /// Process heavy RAW/JPEG data offline
    fn decode_image_to_color_image(path: &Path) -> Result<egui::ColorImage, String> {
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
        let is_raw = ["cr2", "cr3", "raf", "nef", "arw", "dng"].contains(&ext.as_str());

        let rgba_image = if is_raw {
            Self::process_raw_sensor_data(path).ok_or("Failed to process RAW data")?
        } else {
            image::open(path)
                .map_err(|e| format!("Image error: {}", e))?
                .into_rgba8()
        };

        let size = [rgba_image.width() as usize, rgba_image.height() as usize];
        let pixels = rgba_image.as_flat_samples();
        Ok(egui::ColorImage::from_rgba_unmultiplied(size, pixels.as_slice()))
    }

    /// Optimized memory allocation for RAW decoding
    fn process_raw_sensor_data(path: &Path) -> Option<image::RgbaImage> {
        // Read file bytes first
        let data = std::fs::read(path).ok()?;

        // Open the raw image buffer
        let mut raw_image = rsraw::RawImage::open(&data).ok()?;

        // Unpack raw sensor data
        raw_image.unpack().ok()?;

        // Process into 8-bit RGB (demosaicing, white balance, etc.)
        // The const generic '8' corresponds to rsraw::BIT_DEPTH_8
        let processed = raw_image.process::<8>().ok()?;

        let width = processed.width();
        let height = processed.height();

        // MEMORY OPTIMIZATION:
        // Pre-allocate and initialize with 255 (Alpha), avoiding `.push()` calls.
        let mut rgba_data = vec![255u8; (width * height * 4) as usize];

        // ProcessedImage implements Deref to a byte slice for 8-bit depth
        for (i, chunk) in processed.chunks_exact(3).enumerate() {
            let base = i * 4;
            rgba_data[base] = chunk[0];     // R
            rgba_data[base + 1] = chunk[1]; // G
            rgba_data[base + 2] = chunk[2]; // B
            // Alpha is already 255
        }

        image::RgbaImage::from_raw(width, height, rgba_data)
    }

    fn perform_file_op(&mut self, move_file: bool) {
        if let (Some(src), Some(target_dir)) = (&self.selected_path, &self.target_dir) {
            if let Some(filename) = src.file_name() {
                let dest = target_dir.join(filename);
                if move_file {
                    match fs::rename(src, &dest) {
                        Ok(_) => {
                            self.feedback_msg = "File moved successfully.".to_string();
                            self.files.retain(|p| p != src);
                            self.update_search_cache();
                            self.selected_path = None;
                            self.loaded_texture = None;
                        }
                        Err(e) => self.feedback_msg = format!("Move failed: {}", e),
                    }
                } else {
                    match fs::copy(src, &dest) {
                        Ok(_) => self.feedback_msg = "File copied successfully.".to_string(),
                        Err(e) => self.feedback_msg = format!("Copy failed: {}", e),
                    }
                }
            }
        }
    }
}

impl eframe::App for PhotoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {

        // 1. Check if the background thread has finished loading the image
        if let Some(rx) = &self.image_rx {
            if let Ok((path, result)) = rx.try_recv() {
                // Ensure the user hasn't clicked a different image while this was loading
                if Some(path) == self.selected_path {
                    match result {
                        Ok(color_image) => {
                            self.loaded_texture = Some(ctx.load_texture(
                                "current_image",
                                color_image,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                        Err(e) => {
                            self.feedback_msg = e;
                        }
                    }
                    self.is_loading = false;
                }
            }
        }

        // 2. Check if search query changed, update cache if necessary
        if self.search_query != self.last_search_query {
            self.update_search_cache();
        }

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                egui::widgets::global_dark_light_mode_switch(ui);
                ui.separator();

                if ui.button("Select Source Directory").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.load_directory(path);
                    }
                }
                if let Some(dir) = &self.source_dir {
                    ui.label(format!("Source: {}", dir.display()));
                }
            });

            ui.horizontal(|ui| {
                if ui.button("Select Target Directory").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.target_dir = Some(path);
                    }
                }
                if let Some(dir) = &self.target_dir {
                    ui.label(format!("Target: {}", dir.display()));
                }
            });

            ui.horizontal(|ui| {
                ui.label("Search/Match:");
                ui.text_edit_singleline(&mut self.search_query);
            });

            if !self.feedback_msg.is_empty() {
                ui.colored_label(egui::Color32::YELLOW, &self.feedback_msg);
            }
        });

        egui::SidePanel::left("left_panel").show(ctx, |ui| {
            ui.heading("Files");

            let mut clicked_path: Option<PathBuf> = None;

            egui::ScrollArea::vertical().show(ui, |ui| {
                for (path, score) in &self.filtered_files {
                    let filename = path.file_name().unwrap_or_default().to_string_lossy();
                    let display_text = if self.search_query.is_empty() {
                        filename.to_string()
                    } else {
                        format!("{} (Score: {})", filename, score)
                    };

                    let is_selected = Some(path.clone()) == self.selected_path;
                    if ui.selectable_label(is_selected, display_text).clicked() {
                        clicked_path = Some(path.clone());
                    }
                }
            });

            if let Some(path) = clicked_path {
                self.selected_path = Some(path.clone());
                self.load_image_async(ctx, path); // Use the new Async loader
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.is_loading {
                ui.centered_and_justified(|ui| {
                    ui.spinner(); // Shows a smooth loading animation!
                    ui.heading("Processing Image...");
                });
            } else if let Some(texture) = &self.loaded_texture {
                let available_size = ui.available_size() - egui::vec2(0.0, 50.0);
                let img_size = texture.size_vec2();
                let scale = (available_size.x / img_size.x).min(available_size.y / img_size.y).min(1.0);

                ui.image((texture.id(), img_size * scale));

                ui.horizontal(|ui| {
                    if self.target_dir.is_some() {
                        if ui.button("Copy to Target").clicked() {
                            self.perform_file_op(false);
                        }
                        if ui.button("Move to Target").clicked() {
                            self.perform_file_op(true);
                        }
                    } else {
                        ui.label("Select a Target Directory to enable Copy/Move.");
                    }
                });
            } else if self.selected_path.is_some() {
                ui.centered_and_justified(|ui| {
                    ui.label("Loading image or format not natively displayable...");
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Select an image from the left panel.");
                });
            }
        });
    }
}