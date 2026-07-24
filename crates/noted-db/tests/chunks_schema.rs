use sqlx::Row;

async fn pool() -> noted_db::PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://noted:noted@localhost:5433/noted_test".into());
    let p = noted_db::connect(&url).await.unwrap();
    noted_db::migrate(&p).await.unwrap();
    p
}

#[tokio::test]
async fn chunks_and_embeddings_tables_exist() {
    let p = pool().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables
         WHERE table_name IN ('chunks', 'embeddings')",
    )
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn embedding_column_is_768_dimensional() {
    let p = pool().await;
    // A 768-dim vector inserts; a 3-dim one must be rejected by the column type.
    sqlx::query("INSERT INTO chunks (content_hash, text, token_estimate) VALUES ('h768', 't', 1)")
        .execute(&p)
        .await
        .unwrap();
    let v768 = format!("[{}]", vec!["0.1"; 768].join(","));
    sqlx::query(
        "INSERT INTO embeddings (content_hash, model_id, embedding) VALUES ($1, $2, $3::vector)",
    )
    .bind("h768")
    .bind("test")
    .bind(&v768)
    .execute(&p)
    .await
    .unwrap();

    sqlx::query("INSERT INTO chunks (content_hash, text, token_estimate) VALUES ('hbad', 't', 1)")
        .execute(&p)
        .await
        .unwrap();
    let wrong = sqlx::query(
        "INSERT INTO embeddings (content_hash, model_id, embedding)
         VALUES ('hbad', 'test', '[0.1,0.2,0.3]'::vector)",
    )
    .execute(&p)
    .await;
    assert!(
        wrong.is_err(),
        "a 3-dim vector must be rejected by a vector(768) column"
    );

    sqlx::query("DELETE FROM chunks WHERE content_hash IN ('h768','hbad')")
        .execute(&p)
        .await
        .unwrap();
}

#[tokio::test]
async fn embedding_requires_an_existing_chunk() {
    let p = pool().await;
    let v = format!("[{}]", vec!["0.1"; 768].join(","));
    let orphan = sqlx::query(
        "INSERT INTO embeddings (content_hash, model_id, embedding) VALUES ($1, $2, $3::vector)",
    )
    .bind("no-such-chunk")
    .bind("test")
    .bind(&v)
    .execute(&p)
    .await;
    assert!(orphan.is_err(), "embeddings must FK to chunks");
}
