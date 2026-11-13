use crate::dialect::Dialect;
use crate::memory_store::MemoryStore;
use crate::types::Value;
use crate::{BatchStep, Session};
use eggdb_platform_core::time::Instant;
use std::time::Duration;

fn session(store: &MemoryStore) -> Session<'_> {
    Session::with_dialect(store, Dialect::SqlServer)
}

fn batch(session: &mut Session<'_>, text: &str) -> Vec<BatchStep> {
    let mut steps = Vec::new();
    session
        .execute_batch(text, &mut |step| {
            steps.push(step);
            true
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    steps
}

fn errors(steps: &[BatchStep]) -> Vec<Option<u32>> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().err())
        .map(crate::SqlError::sqlserver_number)
        .collect()
}

fn rows(steps: &[BatchStep]) -> Vec<Vec<Value>> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .filter(|result| result.columns.is_some())
        .flat_map(|result| result.rows.clone())
        .collect()
}

fn printed(steps: &[BatchStep]) -> Vec<String> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .flat_map(|result| {
            result
                .info_messages
                .iter()
                .map(|message| message.text.clone())
        })
        .collect()
}

fn succeeds(session: &mut Session<'_>, text: &str) -> Vec<BatchStep> {
    let steps = batch(session, text);
    assert_eq!(errors(&steps), Vec::<Option<u32>>::new(), "{text}");
    steps
}

fn fails_with(session: &mut Session<'_>, text: &str, number: u32) {
    let steps = batch(session, text);
    assert_eq!(errors(&steps), vec![Some(number)], "{text}");
}

fn refused(session: &mut Session<'_>, text: &str) -> Option<u32> {
    session
        .execute_batch(text, &mut |_| true)
        .expect_err(text)
        .sqlserver_number()
}

fn create_items(session: &mut Session<'_>) {
    succeeds(
        session,
        "CREATE TABLE items (id INT PRIMARY KEY, name NVARCHAR(20)); \
         INSERT INTO items VALUES (1, N'apple'), (2, N'pear'), (3, N'plum')",
    );
}

#[test]
fn a_cursor_loop_fetches_every_row_into_variables_until_the_fetch_status_ends() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    create_items(&mut session);
    let steps = succeeds(
        &mut session,
        "DECLARE @id INT, @name NVARCHAR(20); \
         DECLARE c CURSOR LOCAL FAST_FORWARD FOR SELECT id, name FROM items ORDER BY id; \
         OPEN c; \
         FETCH NEXT FROM c INTO @id, @name; \
         WHILE @@FETCH_STATUS = 0 \
         BEGIN \
             PRINT CONCAT(@id, N':', @name); \
             FETCH NEXT FROM c INTO @id, @name; \
         END; \
         PRINT @@FETCH_STATUS; \
         CLOSE c; \
         DEALLOCATE c;",
    );
    assert_eq!(printed(&steps), vec!["1:apple", "2:pear", "3:plum", "-1"]);
}

#[test]
fn a_scroll_cursor_moves_first_last_prior_absolute_and_relative() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    create_items(&mut session);
    let steps = succeeds(
        &mut session,
        "DECLARE @n INT = -2; \
         DECLARE c CURSOR SCROLL FOR SELECT id FROM items ORDER BY id; \
         OPEN c; \
         FETCH LAST FROM c; \
         FETCH PRIOR FROM c; \
         FETCH FIRST FROM c; \
         FETCH ABSOLUTE @n FROM c; \
         FETCH RELATIVE 1 FROM c; \
         FETCH RELATIVE 5 FROM c; \
         FETCH PRIOR FROM c; \
         CLOSE c; DEALLOCATE c;",
    );
    assert_eq!(
        rows(&steps),
        vec![
            vec![Value::Integer(3)],
            vec![Value::Integer(2)],
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
            vec![Value::Integer(3)],
        ]
    );
}

#[test]
fn a_forward_only_cursor_refuses_to_move_backwards() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    create_items(&mut session);
    fails_with(
        &mut session,
        "DECLARE c CURSOR FOR SELECT id FROM items; OPEN c; FETCH LAST FROM c",
        16911,
    );
}

#[test]
fn cursor_mistakes_raise_sql_servers_errors() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    create_items(&mut session);
    fails_with(&mut session, "OPEN missing", 16916);
    succeeds(
        &mut session,
        "DECLARE g CURSOR GLOBAL FOR SELECT id, name FROM items",
    );
    fails_with(
        &mut session,
        "DECLARE g CURSOR GLOBAL FOR SELECT id FROM items",
        16915,
    );
    fails_with(&mut session, "FETCH NEXT FROM g", 16917);
    fails_with(&mut session, "CLOSE g", 16917);
    succeeds(&mut session, "OPEN g");
    fails_with(&mut session, "OPEN g", 16905);
    fails_with(
        &mut session,
        "DECLARE @id INT; FETCH NEXT FROM g INTO @id",
        16924,
    );
    succeeds(&mut session, "CLOSE g; DEALLOCATE g");
    fails_with(&mut session, "DEALLOCATE g", 16916);
}

#[test]
fn cursor_status_reports_open_empty_closed_and_missing_cursors() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    create_items(&mut session);
    let steps = succeeds(
        &mut session,
        "DECLARE full_cursor CURSOR GLOBAL STATIC FOR SELECT id FROM items; \
         DECLARE empty_cursor CURSOR LOCAL STATIC FOR SELECT id FROM items WHERE id > 10; \
         PRINT CURSOR_STATUS('global', 'full_cursor'); \
         OPEN full_cursor; OPEN empty_cursor; \
         PRINT CURSOR_STATUS('global', 'full_cursor'); \
         PRINT CURSOR_STATUS('local', 'empty_cursor'); \
         PRINT @@CURSOR_ROWS; \
         PRINT CURSOR_STATUS('local', 'nothing');",
    );
    assert_eq!(printed(&steps), vec!["-1", "1", "0", "0", "-3"]);
    let after = succeeds(
        &mut session,
        "PRINT CURSOR_STATUS('local', 'empty_cursor'); PRINT CURSOR_STATUS('global', 'full_cursor')",
    );
    assert_eq!(printed(&after), vec!["-3", "1"]);
}

#[test]
fn a_local_cursor_ends_with_its_batch_or_procedure_and_a_global_one_stays() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    create_items(&mut session);
    succeeds(
        &mut session,
        "DECLARE l CURSOR LOCAL FOR SELECT id FROM items; OPEN l",
    );
    fails_with(&mut session, "OPEN l", 16916);
    succeeds(
        &mut session,
        "CREATE PROCEDURE dbo.first_item @id INT OUTPUT AS \
         DECLARE c CURSOR LOCAL FOR SELECT id FROM items ORDER BY id; \
         OPEN c; FETCH NEXT FROM c INTO @id; RETURN 0",
    );
    let steps = succeeds(
        &mut session,
        "DECLARE @id INT; EXEC dbo.first_item @id OUTPUT; EXEC dbo.first_item @id OUTPUT; PRINT @id",
    );
    assert_eq!(printed(&steps), vec!["1"]);
}

#[test]
fn goto_jumps_forward_and_backward_to_labels() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = succeeds(
        &mut session,
        "DECLARE @i INT = 0; \
         again: \
         SET @i += 1; \
         IF @i < 3 GOTO again; \
         PRINT @i; \
         GOTO done; \
         PRINT N'skipped'; \
         done: \
         PRINT N'end'",
    );
    assert_eq!(printed(&steps), vec!["3", "end"]);
}

#[test]
fn goto_refuses_unknown_and_duplicate_labels_and_jumps_into_try() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    assert_eq!(refused(&mut session, "GOTO nowhere"), Some(133));
    assert_eq!(
        refused(&mut session, "here: PRINT 1; here: PRINT 2"),
        Some(132)
    );
    assert_eq!(
        refused(
            &mut session,
            "GOTO inside; BEGIN TRY inside: PRINT 1 END TRY BEGIN CATCH END CATCH",
        ),
        Some(1026)
    );
    let steps = succeeds(
        &mut session,
        "BEGIN TRY GOTO out; PRINT N'no' END TRY BEGIN CATCH END CATCH; out: PRINT N'out'",
    );
    assert_eq!(printed(&steps), vec!["out"]);
}

#[test]
fn waitfor_delay_waits_and_refuses_a_malformed_time() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let started = Instant::now();
    succeeds(&mut session, "WAITFOR DELAY '00:00:00.200'; PRINT 1");
    assert!(started.elapsed() >= Duration::from_millis(200));
    fails_with(&mut session, "WAITFOR DELAY 'soon'", 148);
}

#[test]
fn a_cancel_stops_waitfor_and_a_goto_loop() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let signal = session.cancel_signal();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        signal.cancel();
    });
    let started = Instant::now();
    let steps = batch(&mut session, "WAITFOR DELAY '00:01:00'; PRINT N'never'");
    canceller.join().unwrap_or_default();
    assert!(started.elapsed() < Duration::from_secs(30));
    assert_eq!(errors(&steps).len(), 1);
    assert!(printed(&steps).is_empty());
    let steps = batch(&mut session, "spin: GOTO spin");
    assert_eq!(errors(&steps).len(), 1);
    session.cancel_signal().clear();
}

#[test]
fn select_assignment_reads_its_own_variable_row_by_row() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    create_items(&mut session);
    let steps = succeeds(
        &mut session,
        "DECLARE @list NVARCHAR(100) = N'', @total INT = 0, @csv NVARCHAR(100); \
         SELECT @list = @list + name + N';' FROM items ORDER BY id; \
         SELECT @total = @total + id FROM items; \
         SELECT @csv = COALESCE(@csv + N',', N'') + name FROM items ORDER BY id DESC; \
         PRINT @list; PRINT @total; PRINT @csv",
    );
    assert_eq!(
        printed(&steps),
        vec!["apple;pear;plum;", "6", "plum,pear,apple"]
    );
}

#[test]
fn cursor_variables_are_set_opened_fetched_and_freed() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    batch(&mut session, "CREATE TABLE marks (id int PRIMARY KEY, score int); INSERT INTO marks VALUES (1, 10), (2, 20), (3, 30)");
    let steps = batch(
        &mut session,
        "DECLARE @c CURSOR, @total int = 0, @score int;
         SET @c = CURSOR FORWARD_ONLY STATIC FOR SELECT score FROM marks ORDER BY id;
         OPEN @c;
         FETCH NEXT FROM @c INTO @score;
         WHILE @@FETCH_STATUS = 0
         BEGIN
             SET @total = @total + @score;
             FETCH NEXT FROM @c INTO @score;
         END;
         CLOSE @c;
         DEALLOCATE @c;
         SELECT @total AS total;",
    );
    assert!(errors(&steps).is_empty(), "{:?}", errors(&steps));
    assert_eq!(rows(&steps).last(), Some(&vec![Value::Integer(60)]));
}
