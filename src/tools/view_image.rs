//! `view_image`: give the model a picture from disk.
//!
//! The other tools answer in text, and text about an image is not the image. A screenshot,
//! a rendered chart, a UI mock — the model has to see it to say anything useful, so this
//! result carries the pixels themselves.
//!
//! It reads the file the same way a `read` of it would, and reports the same failure: the
//! model learns the path was wrong, not that "the image failed", which it could not act on.

use std::path::Path;

use crate::llm::ToolSpec;
use crate::tools::ToolOutput;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "view_image".into(),
        description: "查看磁盘上的图片（PNG / JPEG / GIF / WebP）。用在需要真正看见画面的时候：\
                      截图、渲染结果、界面设计。返回图片本身，不是对图片的描述。"
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "图片路径（相对当前工作目录或绝对路径）" }
            },
            "required": ["path"]
        }),
    }
}

pub async fn execute(arguments: &serde_json::Value, cwd: &Path) -> Result<ToolOutput, String> {
    let path = crate::tools::required_str(arguments, "path")?;
    let resolved = crate::auth::policy::resolve_tool_path(path, cwd);
    let image = crate::image_input::read_image_path(&resolved).map_err(|err| err.to_string())?;
    // One line of text with the picture: the result is never empty, and the label says what
    // the model is looking at, which it cannot tell from the pixels alone.
    Ok(ToolOutput {
        content: image.label(),
        display: crate::tools::Display::File {
            verb: crate::tools::verb_for("view_image"),
            path: resolved.display().to_string(),
        },
        is_error: false,
        duration: None,
        images: vec![image.block()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::block;

    fn png_file(dir: &Path, name: &str, width: u32, height: u32) -> String {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&vec![0u8; (width * height * 4) as usize])
            .unwrap();
        writer.finish().unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn the_picture_itself_is_the_result() {
        let dir = crate::tools::temp_dir("view-image");
        let path = png_file(&dir, "shot.png", 12, 7);
        let out = block(execute(&serde_json::json!({"path": path}), &dir)).unwrap();
        assert!(!out.is_error);
        assert_eq!(out.images.len(), 1, "the picture travels with the result");
        assert!(matches!(out.images[0], crate::llm::Block::Image { .. }));
        // The text still says what it is: pixels alone do not name their own dimensions in a
        // way the model can quote.
        assert!(out.content.contains("12×7"), "{}", out.content);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_file_reports_the_path_rather_than_a_generic_failure() {
        let dir = crate::tools::temp_dir("view-image-missing");
        let error = block(execute(
            &serde_json::json!({"path": dir.join("nope.png").to_string_lossy()}),
            &dir,
        ))
        .unwrap_err();
        // The model can act on "no such file"; it cannot act on "image failed".
        assert!(
            error.contains("读不了") || error.contains("No such file"),
            "{error}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_that_is_not_an_image_says_which_formats_are_taken() {
        let dir = crate::tools::temp_dir("view-image-text");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "just text").unwrap();
        let error = block(execute(
            &serde_json::json!({"path": path.to_string_lossy()}),
            &dir,
        ))
        .unwrap_err();
        assert!(error.contains("PNG") && error.contains("JPEG"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
