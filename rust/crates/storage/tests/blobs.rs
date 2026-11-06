use bytes::Bytes;
use loco_storage::{Attachment, ServiceConfig, Storage, StorageConfig, config::DISK};
use serde_json::json;
use sqlx::PgPool;

fn disk_storage(root: &std::path::Path) -> Storage {
    Storage::new(&StorageConfig {
        default_service: DISK.into(),
        services: vec![(
            DISK.into(),
            ServiceConfig::Disk {
                root: root.to_owned(),
            },
        )],
    })
    .unwrap()
}

#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn upload_attach_and_download(db: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let storage = disk_storage(dir.path());

    let blob = storage
        .create_and_upload(
            &db,
            Bytes::from_static(b"%PDF-1.7 test"),
            "contract.pdf",
            Some("application/pdf"),
            json!({ "pdf": { "number_of_pages": 1 } }),
        )
        .await
        .unwrap();

    assert_eq!(blob.service_name, "disk");
    assert_eq!(blob.byte_size, 13);
    assert_eq!(
        blob.metadata_json(),
        json!({ "identified": true, "pdf": { "number_of_pages": 1 } })
    );
    assert!(
        dir.path()
            .join(&blob.key[0..2])
            .join(&blob.key[2..4])
            .join(&blob.key)
            .exists()
    );

    let attachment = Attachment::create(&db, "Template", 1, "documents", &blob)
        .await
        .unwrap();
    let found = Attachment::for_record(&db, "Template", 1, "documents")
        .await
        .unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].uuid, attachment.uuid);
    assert_eq!(
        storage
            .download(&attachment.blob(&db).await.unwrap())
            .await
            .unwrap(),
        Bytes::from_static(b"%PDF-1.7 test")
    );
}
