use std::time::Duration;

use loco_db::signing::{NewCompletedSubmitter, create_completed_submitter};
use sqlx::{Connection, PgConnection, PgPool};

fn completed(submitter_id: i64) -> NewCompletedSubmitter<'static> {
    NewCompletedSubmitter {
        submitter_id,
        submission_id: 7,
        account_id: 1,
        template_id: None,
        source: "api",
        sms_count: 0,
        verification_method: None,
        completed_at: chrono::Utc::now().naive_utc(),
    }
}

/// Two parties of a submission completing at once: the second insert waits for the first
/// party's uncommitted `is_first` row, fails on the unique index and is retried, as Rails
/// retries on `RecordNotUnique`.
#[sqlx::test(
    migrations = false,
    fixtures(path = "../../../schema", scripts("baseline"))
)]
async fn concurrent_completions_keep_both_rows(db: PgPool) {
    let mut conn = PgConnection::connect_with(&db.connect_options())
        .await
        .unwrap();
    let mut first = conn.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO completed_submitters
           (submitter_id, submission_id, account_id, source, sms_count, completed_at, is_first,
            created_at, updated_at)
         VALUES (1, 7, 1, 'api', 0, now(), true, now(), now())",
    )
    .execute(&mut *first)
    .await
    .unwrap();

    let second = tokio::spawn({
        let db = db.clone();
        async move { create_completed_submitter(&db, &completed(2)).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !second.is_finished(),
        "the second insert waits for the first row"
    );
    first.commit().await.unwrap();
    second.await.unwrap().unwrap();

    let rows = || async {
        sqlx::query_as::<_, (i64, Option<bool>)>(
            "SELECT submitter_id, is_first FROM completed_submitters ORDER BY submitter_id",
        )
        .fetch_all(&db)
        .await
        .unwrap()
    };
    assert_eq!(rows().await, [(1, Some(true)), (2, Some(false))]);

    // Completing again leaves the rows alone.
    create_completed_submitter(&db, &completed(2))
        .await
        .unwrap();
    assert_eq!(rows().await, [(1, Some(true)), (2, Some(false))]);
}
