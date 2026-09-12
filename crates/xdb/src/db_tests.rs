use super::*;
use serde_json::json;

fn database(dir: &tempfile::TempDir, name: &str) -> XdbDatabase {
    XdbDatabase::open(dir.path().join(name)).unwrap()
}

fn replicated_records(db: &mut XdbDatabase, collection: &str) -> Vec<Record> {
    let dir = tempfile::tempdir().unwrap();
    let mut peer = database(&dir, "peer.sqlite");
    peer.apply_remote_update(collection, &db.get_full_state(collection).unwrap())
        .unwrap();
    peer.get_collection(collection).unwrap()
}

#[test]
fn export_includes_uncheckpointed_writes_and_crdt_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir, "source.sqlite");
    db.conn
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    let (record, _) = db
        .create_record("notes", json!({"title": "Latest note"}))
        .unwrap();
    let exported = dir.path().join("export.sqlite");
    db.export_to_file(&exported).unwrap();
    let mut backup = XdbDatabase::open(exported).unwrap();
    assert_eq!(backup.get_record(&record.id).unwrap().data, record.data);
    assert_eq!(
        replicated_records(&mut backup, "notes")[0].data,
        record.data
    );
    // Export must not close or otherwise disrupt the live database.
    db.update_record(&record.id, json!({"title": "Still open"}))
        .unwrap();
    assert_eq!(backup.get_record(&record.id).unwrap().data, record.data);
}

#[test]
fn import_reads_live_wal_and_clears_cached_documents() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = database(&dir, "source.sqlite");
    source
        .conn
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    let (imported, _) = source
        .create_record("notes", json!({"title": "Imported"}))
        .unwrap();
    let mut target = database(&dir, "target.sqlite");
    let (old, _) = target
        .create_record("notes", json!({"title": "Old"}))
        .unwrap();
    target.replace_from_file(source.path()).unwrap();
    assert!(matches!(
        target.get_record(&old.id),
        Err(DbError::NotFound(_))
    ));
    assert_eq!(target.get_record(&imported.id).unwrap().data, imported.data);
    target
        .update_record(&imported.id, json!({"edited": true}))
        .unwrap();
    let synced = replicated_records(&mut target, "notes");
    assert_eq!(synced.len(), 1);
    assert_eq!(synced[0].data, json!({"title": "Imported", "edited": true}));
    assert_eq!(
        target
            .conn
            .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
}

#[test]
fn failed_import_preserves_existing_data_and_connection() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir, "target.sqlite");
    let (record, _) = db
        .create_record("notes", json!({"title": "Keep me"}))
        .unwrap();
    let missing = dir.path().join("missing.sqlite");
    let invalid = dir.path().join("invalid.sqlite");
    std::fs::write(&invalid, b"not a sqlite file").unwrap();
    let unrelated = dir.path().join("unrelated.sqlite");
    Connection::open(&unrelated)
        .unwrap()
        .execute_batch("CREATE TABLE unrelated (id INTEGER)")
        .unwrap();
    for path in [&missing, &invalid, &unrelated] {
        assert!(db.replace_from_file(path).is_err());
        assert_eq!(db.get_record(&record.id).unwrap().data, record.data);
    }
    assert!(!missing.exists());
    db.update_record(&record.id, json!({"still_working": true}))
        .unwrap();
    assert_eq!(replicated_records(&mut db, "notes").len(), 1);
}

#[test]
fn backup_rejects_same_path_and_uncommitted_transactions() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir, "target.sqlite");
    let path = db.path().clone();
    assert!(db.export_to_file(&path).is_err());
    assert!(db.replace_from_file(&path).is_err());
    db.with_transaction(|db| {
        assert!(db
            .export_to_file(&dir.path().join("export.sqlite"))
            .is_err());
        assert!(db
            .replace_from_file(&dir.path().join("missing.sqlite"))
            .is_err());
        assert!(db.reload().is_err());
        Ok(())
    })
    .unwrap();
    db.create_record("notes", json!({"title": "Still usable"}))
        .unwrap();
}

#[test]
fn outer_rollback_restores_database_and_sync_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir, "target.sqlite");
    let (original, _) = db
        .create_record("notes", json!({"title": "Original"}))
        .unwrap();
    let result: DbResult<()> = db.with_transaction(|db| {
        db.update_record(&original.id, json!({"title": "Must roll back"}))?;
        db.create_record("notes", json!({"title": "Never committed"}))?;
        Err(DbError::InvalidOperation("Cancel batch".into()))
    });
    assert!(result.is_err());
    assert_eq!(db.get_collection("notes").unwrap().len(), 1);
    let replicated = replicated_records(&mut db, "notes");
    assert_eq!(replicated.len(), 1);
    assert_eq!(replicated[0].data, original.data);
    db.create_record("notes", json!({"title": "Next write"}))
        .unwrap();
    assert_eq!(replicated_records(&mut db, "notes").len(), 2);
}

#[test]
fn caught_nested_failure_rolls_back_only_inner_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir, "target.sqlite");
    db.with_transaction(|db| {
        db.create_record("notes", json!({"title": "Before"}))?;
        let inner: DbResult<()> = db.with_transaction(|db| {
            db.create_record("notes", json!({"title": "Rejected"}))?;
            Err(DbError::InvalidOperation("Reject inner batch".into()))
        });
        assert!(inner.is_err());
        db.create_record("notes", json!({"title": "After"}))?;
        Ok(())
    })
    .unwrap();
    let records = replicated_records(&mut db, "notes");
    assert_eq!(records.len(), 2);
    assert!(records
        .iter()
        .all(|record| record.data["title"] != "Rejected"));
    assert_eq!(db.get_collection("notes").unwrap().len(), 2);
}

#[test]
fn panic_rolls_back_and_leaves_database_usable() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir, "target.sqlite");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: DbResult<()> = db.with_transaction(|db| {
            db.create_record("notes", json!({"title": "Not committed"}))?;
            panic!("cancel callback");
        });
    }));
    assert!(result.is_err());
    assert!(db.conn.is_autocommit());
    assert!(db.get_collection("notes").unwrap().is_empty());
    assert!(replicated_records(&mut db, "notes").is_empty());
    db.create_record("notes", json!({"title": "Next write"}))
        .unwrap();
}

#[test]
fn failed_commit_rolls_back_cached_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir, "target.sqlite");
    db.conn.execute_batch("PRAGMA foreign_keys = ON;
        CREATE TABLE parent (id INTEGER PRIMARY KEY);
        CREATE TABLE child (parent_id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);").unwrap();
    let result = db.with_transaction(|db| {
        db.create_record("notes", json!({"title": "Not committed"}))?;
        db.conn
            .execute("INSERT INTO child (parent_id) VALUES (42)", [])?;
        Ok(())
    });
    assert!(result.is_err());
    assert!(db.conn.is_autocommit());
    assert!(db.get_collection("notes").unwrap().is_empty());
    assert!(replicated_records(&mut db, "notes").is_empty());
}

#[test]
fn deletion_metadata_matches_between_local_and_synced_records() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir, "source.sqlite");
    let (record, _) = db
        .create_record("notes", json!({"title": "Archived"}))
        .unwrap();
    db.delete_record(&record.id).unwrap();
    let mut peer = database(&dir, "peer.sqlite");
    peer.apply_remote_update("notes", &db.get_full_state("notes").unwrap())
        .unwrap();
    let local = serde_json::to_value(db.get_record(&record.id).unwrap()).unwrap();
    let remote = serde_json::to_value(peer.get_record(&record.id).unwrap()).unwrap();
    assert_eq!(local, remote);
    assert!(peer.get_collection("notes").unwrap().is_empty());
}

#[test]
fn corrupt_saved_crdt_is_reported_without_overwriting_data() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir, "target.sqlite");
    let (original, _) = db
        .create_record("notes", json!({"title": "Original"}))
        .unwrap();
    db.conn
        .execute(
            "UPDATE crdt_state SET doc_state = ?1 WHERE collection = 'notes'",
            params![vec![255_u8]],
        )
        .unwrap();
    db.reload().unwrap();
    assert!(matches!(
        db.create_record("notes", json!({"title": "Not committed"})),
        Err(DbError::Crdt(_))
    ));
    assert_eq!(db.get_collection("notes").unwrap().len(), 1);
    assert_eq!(db.get_record(&original.id).unwrap().data, original.data);
}

#[test]
fn statistics_include_database_pages_still_in_wal() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir, "target.sqlite");
    db.conn
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    let before = db.get_stats().unwrap().db_size_bytes;
    db.create_record("notes", json!({"content": "a".repeat(100_000)}))
        .unwrap();
    assert!(db.get_stats().unwrap().db_size_bytes > before);
    assert!(db.get_stats().unwrap().db_size_bytes > std::fs::metadata(db.path()).unwrap().len());
}

#[test]
fn corrupt_record_data_is_reported_instead_of_null_or_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = database(&dir, "target.sqlite");
    let (record, _) = db
        .create_record("notes", json!({"title": "Original"}))
        .unwrap();
    db.conn
        .execute(
            "UPDATE records SET data = 'invalid json' WHERE id = ?1",
            params![record.id],
        )
        .unwrap();
    assert!(matches!(db.get_record(&record.id), Err(DbError::Sqlite(_))));
    assert!(matches!(
        db.get_collection("notes"),
        Err(DbError::Sqlite(_))
    ));
    assert!(matches!(
        db.get_record("missing"),
        Err(DbError::NotFound(_))
    ));
    assert!(db
        .update_record(&record.id, json!({"title": "Must not overwrite"}))
        .is_err());
}

#[test]
fn import_rejects_damaged_records_or_sync_state_before_replacing_target() {
    let dir = tempfile::tempdir().unwrap();
    let mut target = database(&dir, "target.sqlite");
    let (original, _) = target
        .create_record("notes", json!({"title": "Keep me"}))
        .unwrap();
    for (index, damage) in [
        "UPDATE records SET data = 'invalid json'",
        "UPDATE crdt_state SET doc_state = X'FF'",
        "UPDATE crdt_state SET state_vector = X'FF'",
    ]
    .iter()
    .enumerate()
    {
        let mut source = database(&dir, &format!("damaged-{index}.sqlite"));
        source
            .create_record("notes", json!({"title": "Damaged source"}))
            .unwrap();
        source.conn.execute_batch(damage).unwrap();
        assert!(target.replace_from_file(source.path()).is_err());
        assert_eq!(target.get_record(&original.id).unwrap().data, original.data);
        assert_eq!(
            replicated_records(&mut target, "notes")[0].data,
            original.data
        );
    }
}
