use super::ensure_media_extension;

#[test]
fn adds_jpg_for_jpeg_mime() {
    assert_eq!(
        ensure_media_extension("1234_unnamed", Some("image/jpeg")),
        "1234_unnamed.jpg"
    );
}

#[test]
fn adds_png_for_png_mime() {
    assert_eq!(
        ensure_media_extension("1234_unnamed", Some("image/png")),
        "1234_unnamed.png"
    );
}

#[test]
fn adds_gif_for_gif_mime() {
    assert_eq!(
        ensure_media_extension("1234_unnamed", Some("image/gif")),
        "1234_unnamed.gif"
    );
}

#[test]
fn adds_webp_for_webp_mime() {
    assert_eq!(
        ensure_media_extension("1234_unnamed", Some("image/webp")),
        "1234_unnamed.webp"
    );
}

#[test]
fn preserves_existing_extension() {
    assert_eq!(
        ensure_media_extension("1234_photo.png", Some("image/jpeg")),
        "1234_photo.png"
    );
}

#[test]
fn preserves_existing_jpg_extension() {
    assert_eq!(
        ensure_media_extension("1234_photo.jpg", Some("image/jpeg")),
        "1234_photo.jpg"
    );
}

#[test]
fn preserves_existing_jpeg_extension() {
    assert_eq!(
        ensure_media_extension("1234_photo.jpeg", Some("image/png")),
        "1234_photo.jpeg"
    );
}

#[test]
fn adds_pdf_for_pdf_mime() {
    assert_eq!(
        ensure_media_extension("1234_file", Some("application/pdf")),
        "1234_file.pdf"
    );
}

#[test]
fn no_op_for_no_content_type() {
    assert_eq!(ensure_media_extension("1234_file", None), "1234_file");
}

#[test]
fn case_insensitive_existing_extension() {
    assert_eq!(
        ensure_media_extension("1234_photo.PNG", Some("image/jpeg")),
        "1234_photo.PNG"
    );
}

#[test]
fn adds_m4a_for_audio_mp4() {
    assert_eq!(
        ensure_media_extension("1234_voice", Some("audio/mp4")),
        "1234_voice.m4a"
    );
}

#[test]
fn adds_aac_for_audio_aac() {
    assert_eq!(
        ensure_media_extension("1234_voice", Some("audio/aac")),
        "1234_voice.aac"
    );
}

#[test]
fn adds_ogg_for_audio_ogg() {
    assert_eq!(
        ensure_media_extension("1234_voice", Some("audio/ogg")),
        "1234_voice.ogg"
    );
}

#[test]
fn adds_mp4_for_video() {
    assert_eq!(
        ensure_media_extension("1234_video", Some("video/mp4")),
        "1234_video.mp4"
    );
}

#[test]
fn adds_mov_for_quicktime() {
    assert_eq!(
        ensure_media_extension("1234_video", Some("video/quicktime")),
        "1234_video.mov"
    );
}

#[test]
fn preserves_existing_audio_extension() {
    assert_eq!(
        ensure_media_extension("1234_voice.ogg", Some("audio/ogg")),
        "1234_voice.ogg"
    );
}

#[test]
fn no_op_for_unknown_mime() {
    assert_eq!(
        ensure_media_extension("1234_file", Some("application/octet-stream")),
        "1234_file"
    );
}
