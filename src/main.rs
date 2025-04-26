use actix_files::NamedFile;
use actix_multipart::Multipart;
use actix_web::{
    get, middleware, post, web, App, Error, HttpResponse, HttpServer, Result,
};
use futures::{StreamExt, TryStreamExt};
use image::DynamicImage;
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::fs;
use uuid::Uuid;

// Create a struct to hold our application state
struct AppState {
    upload_dir: String,
}

#[derive(Serialize, Deserialize)]
struct ImageProcessingResponse {
    success: bool,
    message: String,
    original_filename: String,
    processed_filename: Option<String>,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    // Create upload directory if it doesn't exist
    let upload_dir = "uploads";
    fs::create_dir_all(upload_dir)?;
    
    // Also create a directory for processed images
    let processed_dir = "processed";
    fs::create_dir_all(processed_dir)?;

    let app_state = web::Data::new(AppState {
        upload_dir: upload_dir.to_string(),
    });

    HttpServer::new(move || {
        App::new()
            .wrap(middleware::Logger::default())
            .app_data(app_state.clone())
            .service(index)
            .service(upload_image)
            .service(process_image)
            .service(actix_files::Files::new("/uploads", "uploads"))
            .service(actix_files::Files::new("/processed", "processed"))
            .service(actix_files::Files::new("/static", "static"))
    })
    .bind("127.0.0.1:8080")?
    .run()
    .await
}

#[get("/")]
async fn index() -> Result<NamedFile> {
    Ok(NamedFile::open("static/index.html")?)
}

#[post("/upload")]
async fn upload_image(mut payload: Multipart, data: web::Data<AppState>) -> Result<HttpResponse, Error> {
    let mut filename = String::new();

    // Iterate over multipart items
    while let Ok(Some(mut field)) = payload.try_next().await {
        let content_disposition = field.content_disposition();
        
        if let Some(name) = content_disposition.get_name() {
            if name == "image" {
                // Generate a unique filename
                let original_name = content_disposition.get_filename().unwrap_or("unknown").to_string();
                let safe_name = sanitize_filename::sanitize(&original_name);
                let uuid = Uuid::new_v4();
                filename = format!("{}-{}", uuid, safe_name);
                
                // Create the filepath here but don't store it in a variable that will be borrowed
                let filepath = format!("{}/{}", data.upload_dir, &filename);
                
                // Create a file to save the image - use move to take ownership of filepath
                let mut file = web::block(move || std::fs::File::create(&filepath)).await??;
                
                // Write the image data to the file
                while let Some(chunk) = field.next().await {
                    let data = chunk?;
                    file = web::block(move || file.write_all(&data).map(|_| file)).await??;
                }
            }
        }
    }

    if filename.is_empty() {
        return Ok(HttpResponse::BadRequest().json(ImageProcessingResponse {
            success: false,
            message: "No image uploaded".to_string(),
            original_filename: "".to_string(),
            processed_filename: None,
        }));
    }

    Ok(HttpResponse::Ok().json(ImageProcessingResponse {
        success: true,
        message: "Image uploaded successfully".to_string(),
        original_filename: filename,
        processed_filename: None,
    }))
}

#[derive(Deserialize)]
struct ProcessImageParams {
    filename: String,
    operation: String,
    params: Option<Vec<String>>,
}

#[post("/process")]
async fn process_image(
    params: web::Json<ProcessImageParams>,
    data: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let filename = &params.filename;
    let operation = &params.operation;
    
    let input_path = format!("{}/{}", data.upload_dir, filename);
    
    // Load the image
    let img = match image::open(&input_path) {
        Ok(img) => img,
        Err(e) => {
            error!("Failed to open image: {}", e);
            return Ok(HttpResponse::InternalServerError().json(ImageProcessingResponse {
                success: false,
                message: format!("Failed to open image: {}", e),
                original_filename: filename.clone(),
                processed_filename: None,
            }));
        }
    };

    info!("Processing image {} with operation {}", filename, operation);

    // Process the image based on the operation
    let processed_img = match operation.as_str() {
        "resize" => {
            let width = params.params.as_ref().and_then(|p| p.get(0))
                .and_then(|w| w.parse::<u32>().ok())
                .unwrap_or(100);
            let height = params.params.as_ref().and_then(|p| p.get(1))
                .and_then(|h| h.parse::<u32>().ok())
                .unwrap_or(100);
            resize_image(&img, width, height)
        }
        "grayscale" => grayscale_image(&img),
        "blur" => {
            let sigma = params.params.as_ref().and_then(|p| p.get(0))
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(1.0);
            blur_image(&img, sigma)
        }
        "brighten" => {
            let value = params.params.as_ref().and_then(|p| p.get(0))
                .and_then(|v| v.parse::<i32>().ok())
                .unwrap_or(10);
            brighten_image(&img, value)
        }
        "rotate" => {
            let degrees = params.params.as_ref().and_then(|p| p.get(0))
                .and_then(|d| d.parse::<i32>().ok())
                .unwrap_or(90);
            rotate_image(&img, degrees)
        }
        "flip" => {
            // FIX: Use a let binding to extend the string's lifetime
            let default_direction = "horizontal".to_string();
            let direction = params.params.as_ref().and_then(|p| p.get(0))
                .unwrap_or(&default_direction);
            flip_image(&img, direction)
        }
        _ => {
            return Ok(HttpResponse::BadRequest().json(ImageProcessingResponse {
                success: false,
                message: format!("Unknown operation: {}", operation),
                original_filename: filename.clone(),
                processed_filename: None,
            }));
        }
    };

    // Create an output filename
    let input_path = Path::new(&input_path);
    let stem = input_path.file_stem().unwrap_or_default().to_str().unwrap_or("output");
    let extension = input_path.extension().unwrap_or_default().to_str().unwrap_or("png");
    let output_filename = format!("{}_{}_{}.{}", 
        stem, 
        operation,
        params.params.as_ref().map_or("default".to_string(), |p| p.join("_")),
        extension
    );
    let output_path = format!("processed/{}", output_filename);

    // Save the processed image
    match processed_img.save(&output_path) {
        Ok(_) => {
            info!("Saved processed image to: {}", output_path);
            Ok(HttpResponse::Ok().json(ImageProcessingResponse {
                success: true,
                message: format!("Image processed with {} operation", operation),
                original_filename: filename.clone(),
                processed_filename: Some(output_filename),
            }))
        },
        Err(e) => {
            error!("Failed to save image: {}", e);
            Ok(HttpResponse::InternalServerError().json(ImageProcessingResponse {
                success: false,
                message: format!("Failed to save processed image: {}", e),
                original_filename: filename.clone(),
                processed_filename: None,
            }))
        }
    }
}

// Image processing functions
fn resize_image(img: &DynamicImage, width: u32, height: u32) -> DynamicImage {
    img.resize(width, height, image::imageops::FilterType::Lanczos3)
}

fn grayscale_image(img: &DynamicImage) -> DynamicImage {
    img.grayscale()
}

fn blur_image(img: &DynamicImage, sigma: f32) -> DynamicImage {
    img.blur(sigma)
}

fn brighten_image(img: &DynamicImage, value: i32) -> DynamicImage {
    img.brighten(value)
}

fn rotate_image(img: &DynamicImage, degrees: i32) -> DynamicImage {
    match degrees {
        90 => img.rotate90(),
        180 => img.rotate180(),
        270 => img.rotate270(),
        _ => {
            error!("Rotation is limited to 90, 180, or 270 degrees. Using 90.");
            img.rotate90()
        }
    }
}

fn flip_image(img: &DynamicImage, direction: &str) -> DynamicImage {
    match direction {
        "horizontal" => img.fliph(),
        "vertical" => img.flipv(),
        _ => {
            error!("Unknown flip direction. Using horizontal.");
            img.fliph()
        }
    }
}