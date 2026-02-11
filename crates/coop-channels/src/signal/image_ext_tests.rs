use super::ensure_image_extension;

#[test]
fn adds_jpg_for_jpeg_mime() {
    assert_eq!(
        ensure_image_extension("1234_unnamed", Some("image/jpeg")),
        "1234_unnamed.jpg"
    );
}

#[test]
fn adds_png_for_png_mime() {
    assert_eq!(
        ensure_image_extension("1234_unnamed", Some("image/png")),
        "1234_unnamed.png"
    );
}

#[test]
fn adds_gif_for_gif_mime() {
    assert_eq!(
        ensure_image_extension("1234_unnamed", Some("image/gif")),
        "1234_unnamed.gif"
    );
}

#[test]
fn adds_webp_for_webp_mime() {
    assert_eq!(
        ensure_image_extension("1234_unnamed", Some("image/webp")),
        "1234_unnamed.webp"
    );
}

#[test]
fn preserves_existing_extension() {
    assert_eq!(
        ensure_image_extension("1234_photo.png", Some("image/jpeg")),
        "1234_photo.png"
    );
}

#[test]
fn preserves_existing_jpg_extension() {
    assert_eq!(
        ensure_image_extension("1234_photo.jpg", Some("image/jpeg")),
        "1234_photo.jpg"
    );
}

#[test]
fn preserves_existing_jpeg_extension() {
    assert_eq!(
        ensure_image_extension("1234_photo.jpeg", Some("image/png")),
        "1234_photo.jpeg"
    );
}

#[test]
fn no_op_for_non_image_mime() {
    assert_eq!(
        ensure_image_extension("1234_file", Some("application/pdf")),
        "1234_file"
    );
}

#[test]
fn no_op_for_no_content_type() {
    assert_eq!(ensure_image_extension("1234_file", None), "1234_file");
}

#[test]
fn case_insensitive_existing_extension() {
    assert_eq!(
        ensure_image_extension("1234_photo.PNG", Some("image/jpeg")),
        "1234_photo.PNG"
    );
}
