#include "eggdb.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int checks_passed = 0;

static void fail(const char *what, int line) {
    fprintf(stderr, "api_check.c:%d: %s failed: %s\n", line, what, eggdb_last_error());
    exit(1);
}

#define CHECK(condition)                \
    do {                                \
        if (!(condition)) {             \
            fail(#condition, __LINE__); \
        }                               \
        checks_passed++;                \
    } while (0)

#define CHECK_OK(call) CHECK((call) == EGGDB_OK)

static const uint8_t *bytes(const char *text) {
    return (const uint8_t *)text;
}

static void join_path(char *out, size_t size, const char *directory, const char *name) {
    int written = snprintf(out, size, "%s/%s", directory, name);
    if (written < 0 || (size_t)written >= size) {
        fprintf(stderr, "api_check.c: the path %s/%s is too long\n", directory, name);
        exit(2);
    }
}

static void check_keys_and_values(eggdb_db *db) {
    eggdb_txn *txn = NULL;
    CHECK_OK(eggdb_begin_write(db, &txn));
    CHECK_OK(eggdb_put(txn, NULL, bytes("greeting"), 8, bytes("hello"), 5));
    CHECK_OK(eggdb_put(txn, NULL, bytes("empty"), 5, bytes(""), 0));
    for (int index = 0; index < 100; index++) {
        char key[16];
        int length = snprintf(key, sizeof key, "n%03d", index);
        CHECK_OK(eggdb_put(txn, NULL, bytes(key), (size_t)length, bytes("v"), 1));
    }
    uint64_t stamp = 0;
    CHECK_OK(eggdb_commit(txn, &stamp));
    CHECK(stamp > 0);

    CHECK_OK(eggdb_begin_read(db, &txn));
    uint8_t *value = NULL;
    size_t value_len = 0;
    CHECK_OK(eggdb_get(txn, NULL, bytes("greeting"), 8, &value, &value_len));
    CHECK(value != NULL && value_len == 5 && memcmp(value, "hello", 5) == 0);
    eggdb_free(value, value_len);
    CHECK_OK(eggdb_get(txn, NULL, bytes("empty"), 5, &value, &value_len));
    CHECK(value != NULL && value_len == 0);
    eggdb_free(value, value_len);
    CHECK_OK(eggdb_get(txn, NULL, bytes("missing"), 7, &value, &value_len));
    CHECK(value == NULL && value_len == 0);
    CHECK(eggdb_put(txn, NULL, bytes("k"), 1, bytes("v"), 1) == EGGDB_ERR_READ_TRANSACTION);
    CHECK(strstr(eggdb_last_error(), "eggdb_begin_write") != NULL);

    eggdb_scan *scan = NULL;
    CHECK_OK(eggdb_scan_open(txn, NULL, bytes("n"), 1, NULL, 0, EGGDB_SCAN_PREFIX, &scan));
    size_t seen = 0;
    char previous[16] = "";
    for (;;) {
        const eggdb_entry *entries = NULL;
        size_t count = 0;
        CHECK_OK(eggdb_scan_next(txn, scan, 7, &entries, &count));
        if (count == 0) {
            break;
        }
        CHECK(count <= 7);
        for (size_t index = 0; index < count; index++) {
            char key[16] = "";
            CHECK(entries[index].key_len < sizeof key);
            memcpy(key, entries[index].key, entries[index].key_len);
            CHECK(strcmp(previous, key) < 0);
            memcpy(previous, key, sizeof key);
            CHECK(entries[index].value_len == 1 && entries[index].value[0] == 'v');
        }
        seen += count;
    }
    CHECK(seen == 100);
    CHECK(strcmp(previous, "n099") == 0);
    eggdb_scan_free(scan);

    CHECK_OK(eggdb_scan_open(txn, NULL, NULL, 0, NULL, 0, EGGDB_SCAN_REVERSE, &scan));
    const eggdb_entry *entries = NULL;
    size_t count = 0;
    CHECK_OK(eggdb_scan_next(txn, scan, 1, &entries, &count));
    CHECK(count == 1 && entries[0].key_len == 4 && memcmp(entries[0].key, "n099", 4) == 0);
    eggdb_scan_free(scan);
    CHECK_OK(eggdb_rollback(txn));
}

static void check_collections_expiry_and_counters(eggdb_db *db) {
    eggdb_txn *txn = NULL;
    CHECK_OK(eggdb_begin_write(db, &txn));
    int created = -1;
    CHECK_OK(eggdb_create_collection(txn, "users", &created));
    CHECK(created == 1);
    CHECK_OK(eggdb_put(txn, "users", bytes("ada"), 3, bytes("Ada Lovelace"), 12));
    CHECK(eggdb_put(txn, "nobody", bytes("x"), 1, bytes("y"), 1) ==
          EGGDB_ERR_COLLECTION_NOT_FOUND);
    CHECK_OK(eggdb_put_with_expiry(txn, NULL, bytes("session"), 7, bytes("token"), 5, 60000));
    int64_t counted = 0;
    CHECK_OK(eggdb_increment(txn, NULL, bytes("visits"), 6, 41, &counted));
    CHECK_OK(eggdb_increment(txn, NULL, bytes("visits"), 6, 1, &counted));
    CHECK(counted == 42);
    CHECK(eggdb_increment(txn, NULL, bytes("session"), 7, 1, NULL) == EGGDB_ERR_NOT_AN_INTEGER);
    int swapped = -1;
    CHECK_OK(eggdb_compare_and_set(txn, NULL, bytes("lock"), 4, NULL, 0, bytes("me"), 2,
                                   &swapped));
    CHECK(swapped == 1);
    CHECK_OK(eggdb_compare_and_set(txn, NULL, bytes("lock"), 4, NULL, 0, bytes("you"), 3,
                                   &swapped));
    CHECK(swapped == 0);
    CHECK_OK(eggdb_commit(txn, NULL));

    CHECK_OK(eggdb_begin_read(db, &txn));
    int64_t ttl_ms = 0;
    CHECK_OK(eggdb_time_to_live(txn, NULL, bytes("session"), 7, &ttl_ms));
    CHECK(ttl_ms > 0 && ttl_ms <= 60000);
    CHECK_OK(eggdb_time_to_live(txn, NULL, bytes("visits"), 6, &ttl_ms));
    CHECK(ttl_ms == -1);
    uint8_t *names = NULL;
    size_t names_len = 0;
    size_t name_count = 0;
    CHECK_OK(eggdb_collection_names(txn, &names, &names_len, &name_count));
    CHECK(name_count == 1 && names_len == 6 && memcmp(names, "users", 6) == 0);
    eggdb_free(names, names_len);
    uint8_t *value = NULL;
    size_t value_len = 0;
    CHECK_OK(eggdb_get(txn, "users", bytes("ada"), 3, &value, &value_len));
    CHECK(value_len == 12 && memcmp(value, "Ada Lovelace", 12) == 0);
    eggdb_free(value, value_len);
    CHECK_OK(eggdb_get(txn, NULL, bytes("ada"), 3, &value, &value_len));
    CHECK(value == NULL);
    CHECK_OK(eggdb_rollback(txn));
}

static const eggdb_result *run_sql(eggdb_sql *sql, const char *text, size_t expected_results) {
    const eggdb_result *results = NULL;
    size_t count = 0;
    CHECK_OK(eggdb_sql_execute(sql, text, &results, &count));
    CHECK(count == expected_results);
    return results;
}

static void check_sql(eggdb_db *db) {
    eggdb_sql *postgres = NULL;
    CHECK_OK(eggdb_sql_open(db, "postgresql", &postgres));
    run_sql(postgres,
            "CREATE TABLE items (id integer PRIMARY KEY, name text, price double precision); "
            "INSERT INTO items VALUES (1, 'tea', 2.5), (2, 'cake', NULL)",
            2);
    eggdb_parameter parameters[2] = {
        {bytes("3"), 1},
        {bytes("O'Brien's pie"), 13},
    };
    const eggdb_result *results = NULL;
    size_t count = 0;
    CHECK_OK(eggdb_sql_execute_with_parameters(
        postgres, "INSERT INTO items (id, name) VALUES ($1, $2)", parameters, 2, &results,
        &count));
    CHECK(count == 1 && strcmp(results[0].tag, "INSERT 0 1") == 0);

    eggdb_sql *sqlite = NULL;
    CHECK_OK(eggdb_sql_open(db, "sqlite", &sqlite));
    results = run_sql(sqlite, "SELECT id, name, price FROM items ORDER BY id", 1);
    const eggdb_result *rows = &results[0];
    CHECK(rows->column_count == 3 && rows->row_count == 3);
    CHECK(strcmp(rows->column_names[1], "name") == 0);
    const eggdb_value *id = eggdb_result_value(rows, 0, 0);
    CHECK(id->kind == EGGDB_INTEGER && id->integer == 1);
    const eggdb_value *price = eggdb_result_value(rows, 0, 2);
    CHECK(price->kind == EGGDB_REAL && price->real == 2.5);
    const eggdb_value *missing = eggdb_result_value(rows, 1, 2);
    CHECK(missing->kind == EGGDB_NULL && missing->data == NULL);
    const eggdb_value *name = eggdb_result_value(rows, 2, 1);
    CHECK(name->kind == EGGDB_TEXT && name->len == 13 &&
          memcmp(name->data, "O'Brien's pie", 13) == 0);
    CHECK(name->data[name->len] == '\0');

    CHECK(eggdb_sql_execute(sqlite, "SELECT * FROM no_such_table", &results, &count) ==
          EGGDB_ERR_SQL);
    CHECK(results == NULL && count == 0);
    CHECK(strlen(eggdb_last_sqlstate()) == 5);
    CHECK(strstr(eggdb_last_error(), "no_such_table") != NULL);

    CHECK(eggdb_close(db) == EGGDB_ERR_IN_USE);
    CHECK_OK(eggdb_sql_close(sqlite));
    CHECK_OK(eggdb_sql_close(postgres));
}

static void check_prepared_statements_and_copy(eggdb_db *db) {
    eggdb_sql *sql = NULL;
    CHECK_OK(eggdb_sql_open(db, "postgresql", &sql));
    run_sql(sql, "CREATE TABLE readings (id bigint, sensor text, value double precision, "
                 "ok boolean, raw bytea)",
            1);

    const char *insert_text = "INSERT INTO readings VALUES ($1, $2, $3, $4, $5)";
    eggdb_stmt *insert = NULL;
    CHECK_OK(eggdb_sql_prepare(sql, insert_text, strlen(insert_text), &insert));
    size_t parameter_count = 0;
    CHECK_OK(eggdb_stmt_parameter_count(insert, &parameter_count));
    CHECK(parameter_count == 5);
    const uint8_t raw[3] = {0, 0x7f, 0xff};
    const eggdb_result *results = NULL;
    size_t count = 0;
    for (int64_t id = 1; id <= 3; id++) {
        CHECK_OK(eggdb_stmt_bind_int64(insert, 1, id));
        CHECK_OK(eggdb_stmt_bind_text(insert, 2, "north", 5));
        CHECK_OK(eggdb_stmt_bind_double(insert, 3, (double)id / 4.0));
        CHECK_OK(eggdb_stmt_bind_bool(insert, 4, id != 2));
        CHECK_OK(eggdb_stmt_bind_blob(insert, 5, raw, sizeof raw));
        CHECK_OK(eggdb_stmt_execute(insert, &results, &count));
        CHECK(count == 1 && strcmp(results[0].tag, "INSERT 0 1") == 0);
    }
    CHECK(eggdb_stmt_bind_int64(insert, 6, 0) == EGGDB_ERR_INVALID_ARGUMENT);
    CHECK(eggdb_sql_close(sql) == EGGDB_ERR_IN_USE);
    CHECK_OK(eggdb_stmt_finalize(insert));

    const char *select_text = "SELECT id, value, ok, raw FROM readings WHERE id = $1";
    eggdb_stmt *select = NULL;
    CHECK_OK(eggdb_sql_prepare(sql, select_text, strlen(select_text), &select));
    size_t column_count = 0;
    CHECK_OK(eggdb_stmt_column_count(select, &column_count));
    CHECK(column_count == 4);
    const char *column_name = NULL;
    CHECK_OK(eggdb_stmt_column_name(select, 1, &column_name));
    CHECK(strcmp(column_name, "value") == 0);
    int32_t kind = EGGDB_NULL;
    CHECK_OK(eggdb_stmt_column_type(select, 3, &kind));
    CHECK(kind == EGGDB_BLOB);
    CHECK_OK(eggdb_stmt_bind_int64(select, 1, 2));
    CHECK_OK(eggdb_stmt_execute(select, &results, &count));
    CHECK(count == 1 && results[0].row_count == 1);
    CHECK(eggdb_result_value(&results[0], 0, 1)->real == 0.5);
    CHECK(eggdb_result_value(&results[0], 0, 2)->integer == 0);
    const eggdb_value *bytes_back = eggdb_result_value(&results[0], 0, 3);
    CHECK(bytes_back->kind == EGGDB_BLOB && bytes_back->len == 3 &&
          memcmp(bytes_back->data, raw, 3) == 0);
    CHECK_OK(eggdb_stmt_clear_bindings(select));
    CHECK_OK(eggdb_stmt_execute(select, &results, &count));
    CHECK(count == 1 && results[0].row_count == 0);
    CHECK_OK(eggdb_stmt_reset(select));
    CHECK_OK(eggdb_stmt_finalize(select));

    const char *rows = "10\tsouth\t1.5\tt\t\\\\x00\n11\twest\t\\N\tf\t\\N\n";
    CHECK_OK(eggdb_sql_copy_in(sql, "COPY readings FROM STDIN", bytes(rows), strlen(rows),
                               &results, &count));
    CHECK(count == 1 && strcmp(results[0].tag, "COPY 2") == 0);
    results = run_sql(sql, "SELECT count(*) FROM readings", 1);
    CHECK(eggdb_result_value(&results[0], 0, 0)->integer == 5);

    CHECK_OK(eggdb_sql_cancel(sql));
    run_sql(sql, "SELECT 1", 1);
    CHECK(eggdb_sql_prepare(sql, "SELECT 1; SELECT 2", 18, &select) == EGGDB_ERR_SQL);
    CHECK(select == NULL);
    CHECK_OK(eggdb_stmt_finalize(NULL));
    CHECK_OK(eggdb_sql_close(sql));
}

static void check_stepping_and_streaming_copy(eggdb_db *db) {
    eggdb_sql *sql = NULL;
    CHECK_OK(eggdb_sql_open(db, "postgresql", &sql));
    run_sql(sql, "CREATE TABLE lines (id bigint, word text, weight double precision)", 1);

    eggdb_copy *copy = NULL;
    CHECK_OK(eggdb_sql_copy_in_begin(sql, "COPY lines FROM STDIN", &copy));
    const char *data = "1\tone\t0.5\n2\ttwo\t\\N\n3\tthr";
    CHECK_OK(eggdb_copy_put(copy, bytes(data), strlen(data)));
    CHECK_OK(eggdb_copy_put(copy, bytes("ee\t1.5\n"), 7));
    const eggdb_result *results = NULL;
    size_t count = 0;
    CHECK(eggdb_sql_execute(sql, "SELECT 1", &results, &count) == EGGDB_ERR_IN_USE);
    CHECK_OK(eggdb_copy_end(copy, &results, &count));
    CHECK(count == 1 && strcmp(results[0].tag, "COPY 3") == 0);
    CHECK(eggdb_copy_end(copy, &results, &count) == EGGDB_ERR_INVALID_ARGUMENT);
    eggdb_copy_free(copy);

    const char *text = "SELECT id, word, weight FROM lines ORDER BY id";
    eggdb_stmt *select = NULL;
    CHECK_OK(eggdb_sql_prepare(sql, text, strlen(text), &select));
    int has_row = 0;
    int64_t total = 0;
    size_t rows = 0;
    for (;;) {
        CHECK_OK(eggdb_stmt_step(select, &has_row));
        if (!has_row) {
            break;
        }
        const eggdb_value *values = NULL;
        size_t columns = 0;
        CHECK_OK(eggdb_stmt_row(select, &values, &columns));
        CHECK(columns == 3 && values[0].kind == EGGDB_INTEGER);
        int64_t id = 0;
        CHECK_OK(eggdb_stmt_value_int64(select, 0, &id));
        total += id;
        const char *word = NULL;
        size_t word_len = 0;
        CHECK_OK(eggdb_stmt_value_text(select, 1, &word, &word_len));
        CHECK(word != NULL && word[word_len] == '\0');
        int32_t kind = EGGDB_NULL;
        CHECK_OK(eggdb_stmt_value_kind(select, 2, &kind));
        if (kind == EGGDB_REAL) {
            double weight = 0;
            CHECK_OK(eggdb_stmt_value_double(select, 2, &weight));
            CHECK(weight == 0.5 || weight == 1.5);
        } else {
            double weight = 0;
            CHECK(kind == EGGDB_NULL && id == 2);
            CHECK(eggdb_stmt_value_double(select, 2, &weight) == EGGDB_ERR_INVALID_ARGUMENT);
        }
        const uint8_t *raw = NULL;
        size_t raw_len = 0;
        CHECK_OK(eggdb_stmt_value_blob(select, 1, &raw, &raw_len));
        CHECK(raw_len == word_len);
        rows++;
    }
    CHECK(rows == 3 && total == 6);
    CHECK_OK(eggdb_stmt_step(select, &has_row));
    CHECK(has_row == 0);
    CHECK_OK(eggdb_stmt_reset(select));
    CHECK_OK(eggdb_stmt_step(select, &has_row));
    CHECK(has_row == 1);
    CHECK_OK(eggdb_stmt_finalize(select));

    CHECK_OK(eggdb_sql_copy_out_begin(sql, "COPY lines (id, word) TO STDOUT", &copy));
    char out[64] = {0};
    size_t used = 0;
    for (;;) {
        const uint8_t *piece = NULL;
        size_t piece_len = 0;
        CHECK_OK(eggdb_copy_read(copy, &piece, &piece_len));
        if (piece == NULL) {
            break;
        }
        CHECK(used + piece_len < sizeof out);
        memcpy(out + used, piece, piece_len);
        used += piece_len;
    }
    CHECK(strcmp(out, "1\tone\n2\ttwo\n3\tthree\n") == 0);
    CHECK_OK(eggdb_copy_end(copy, &results, &count));
    CHECK(count == 1 && strcmp(results[0].tag, "COPY 3") == 0);
    eggdb_copy_free(copy);
    eggdb_copy_free(NULL);
    CHECK_OK(eggdb_sql_close(sql));
}

static void check_backup_and_compaction(eggdb_db *db, const char *work) {
    char copy[4096];
    join_path(copy, sizeof copy, work, "api-check-copy.db");
    eggdb_backup_report backup;
    CHECK_OK(eggdb_backup(db, copy, &backup));
    CHECK(backup.entries > 100 && backup.file_bytes > 0);
    CHECK(eggdb_backup(db, copy, NULL) == EGGDB_ERR_ALREADY_EXISTS);
    eggdb_compact_report compacted;
    CHECK_OK(eggdb_compact_file(copy, &compacted));
    CHECK(compacted.entries == backup.entries);

    eggdb_options options = {EGGDB_OPEN_READ_ONLY, EGGDB_DURABILITY_FULL, 0};
    eggdb_db *reader = NULL;
    CHECK_OK(eggdb_open(copy, &options, &reader));
    eggdb_txn *txn = NULL;
    CHECK_OK(eggdb_begin_read(reader, &txn));
    uint8_t *value = NULL;
    size_t value_len = 0;
    CHECK_OK(eggdb_get(txn, NULL, bytes("greeting"), 8, &value, &value_len));
    CHECK(value_len == 5 && memcmp(value, "hello", 5) == 0);
    eggdb_free(value, value_len);
    CHECK_OK(eggdb_rollback(txn));
    CHECK(eggdb_begin_write(reader, &txn) == EGGDB_ERR_READ_ONLY);
    CHECK(txn == NULL);
    CHECK_OK(eggdb_close(reader));
}

static void check_misuse(const char *work) {
    char missing[4096];
    join_path(missing, sizeof missing, work, "api-check-missing.db");
    eggdb_options options = {EGGDB_OPEN_NO_CREATE, EGGDB_DURABILITY_FULL, 0};
    eggdb_db *db = (eggdb_db *)&options;
    CHECK(eggdb_open(missing, &options, &db) == EGGDB_ERR_NOT_FOUND);
    CHECK(db == NULL);
    CHECK(strcmp(eggdb_status_name(EGGDB_ERR_NOT_FOUND), "EGGDB_ERR_NOT_FOUND") == 0);
    CHECK(eggdb_open(NULL, NULL, &db) == EGGDB_ERR_INVALID_ARGUMENT);
    CHECK(eggdb_commit(NULL, NULL) == EGGDB_ERR_INVALID_ARGUMENT);
    CHECK(eggdb_rollback(NULL) == EGGDB_OK);
    CHECK(eggdb_close(NULL) == EGGDB_OK);
    eggdb_scan_free(NULL);
    eggdb_free(NULL, 0);
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: api_check <work directory>\n");
        return 2;
    }
    const char *work = argv[1];
    char path[4096];
    join_path(path, sizeof path, work, "api-check.db");

    CHECK(strlen(eggdb_version()) > 0);
    eggdb_db *db = NULL;
    CHECK_OK(eggdb_open(path, NULL, &db));
    check_keys_and_values(db);
    check_collections_expiry_and_counters(db);
    check_sql(db);
    check_prepared_statements_and_copy(db);
    check_stepping_and_streaming_copy(db);
    check_backup_and_compaction(db, work);
    eggdb_compact_report compacted;
    CHECK_OK(eggdb_compact(db, &compacted));
    CHECK(compacted.pages_after <= compacted.pages_before);
    check_misuse(work);
    printf("api_check: %d checks passed with EggDB %s\n", checks_passed, eggdb_version());
    return 0;
}
