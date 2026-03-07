use super::{
    AttachmentPointer, attachment_save_name, inbound::format_attachment_metadata,
    rewrite_attachment_lines,
};

#[test]
fn attachment_save_names_are_unique_for_duplicate_unnamed_images() {
    assert_eq!(
        attachment_save_name(1_772_846_352_737, 1, "unnamed", Some("image/jpeg")),
        "1772846352737_001_unnamed.jpg"
    );
    assert_eq!(
        attachment_save_name(1_772_846_352_737, 2, "unnamed", Some("image/jpeg")),
        "1772846352737_002_unnamed.jpg"
    );
}

#[test]
fn rewrite_attachment_lines_preserves_distinct_paths_for_duplicate_metadata() {
    let attachment = AttachmentPointer {
        file_name: Some("unnamed".to_owned()),
        content_type: Some("image/jpeg".to_owned()),
        size: Some(321_409),
        ..Default::default()
    };
    let meta = format_attachment_metadata(&attachment);
    let content = format!("plans incoming\n{meta}\n{meta}\n{meta}");
    let originals = vec![meta.clone(), meta.clone(), meta.clone()];
    let replacements = vec![
        format!("{meta}\n[file saved: /tmp/1772846352737_001_unnamed.jpg]"),
        format!("{meta}\n[file saved: /tmp/1772846352737_002_unnamed.jpg]"),
        format!("{meta}\n[file saved: /tmp/1772846352737_003_unnamed.jpg]"),
    ];

    let rewritten = rewrite_attachment_lines(&content, &originals, &replacements);

    assert_eq!(rewritten.matches("[file saved:").count(), 3);
    assert!(rewritten.contains("/tmp/1772846352737_001_unnamed.jpg"));
    assert!(rewritten.contains("/tmp/1772846352737_002_unnamed.jpg"));
    assert!(rewritten.contains("/tmp/1772846352737_003_unnamed.jpg"));
    assert!(
        rewritten.find("001_unnamed.jpg").unwrap() < rewritten.find("002_unnamed.jpg").unwrap()
    );
    assert!(
        rewritten.find("002_unnamed.jpg").unwrap() < rewritten.find("003_unnamed.jpg").unwrap()
    );
}

#[test]
fn mixed_attachment_types_keep_unique_names_and_extensions() {
    let attachments = [
        AttachmentPointer {
            file_name: Some("unnamed".to_owned()),
            content_type: Some("image/jpeg".to_owned()),
            size: Some(343_911),
            ..Default::default()
        },
        AttachmentPointer {
            file_name: Some("unnamed".to_owned()),
            content_type: Some("application/pdf".to_owned()),
            size: Some(120_000),
            ..Default::default()
        },
        AttachmentPointer {
            file_name: Some("floor plan.docx".to_owned()),
            content_type: Some(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                    .to_owned(),
            ),
            size: Some(64_000),
            ..Default::default()
        },
        AttachmentPointer {
            file_name: Some("unnamed".to_owned()),
            content_type: Some("audio/ogg".to_owned()),
            size: Some(8_000),
            ..Default::default()
        },
        AttachmentPointer {
            file_name: Some("unnamed".to_owned()),
            content_type: Some("image/jpeg".to_owned()),
            size: Some(286_118),
            ..Default::default()
        },
    ];

    let originals = attachments
        .iter()
        .map(format_attachment_metadata)
        .collect::<Vec<_>>();
    let content = format!(
        "message\n{}\n{}\n{}\n{}\n{}",
        originals[0], originals[1], originals[2], originals[3], originals[4]
    );
    let replacements = attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| {
            let meta = format_attachment_metadata(attachment);
            let file_name = attachment.file_name.as_deref().unwrap_or("unnamed");
            let save_name = attachment_save_name(
                1_772_846_352_737,
                index + 1,
                file_name,
                attachment.content_type.as_deref(),
            );
            format!("{meta}\n[file saved: /tmp/{save_name}]")
        })
        .collect::<Vec<_>>();

    let rewritten = rewrite_attachment_lines(&content, &originals, &replacements);

    assert!(rewritten.contains("/tmp/1772846352737_001_unnamed.jpg"));
    assert!(rewritten.contains("/tmp/1772846352737_002_unnamed.pdf"));
    assert!(rewritten.contains("/tmp/1772846352737_003_floor_plan.docx"));
    assert!(rewritten.contains("/tmp/1772846352737_004_unnamed.ogg"));
    assert!(rewritten.contains("/tmp/1772846352737_005_unnamed.jpg"));
}
