#ifndef EGGDB_H
#define EGGDB_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define EGGDB_OK 0
#define EGGDB_ERR_NOT_FOUND 1
#define EGGDB_ERR_PERMISSION_DENIED 2
#define EGGDB_ERR_LOCKED 3
#define EGGDB_ERR_READ_ONLY 4
#define EGGDB_ERR_NOT_A_DATABASE 5
#define EGGDB_ERR_INCOMPATIBLE 6
#define EGGDB_ERR_CORRUPT 7
#define EGGDB_ERR_IO 8
#define EGGDB_ERR_OUT_OF_MEMORY 9
#define EGGDB_ERR_TOO_LARGE 10
#define EGGDB_ERR_CACHE_FULL 11
#define EGGDB_ERR_WRITER_ACTIVE 12
#define EGGDB_ERR_SNAPSHOT_TOO_OLD 13
#define EGGDB_ERR_TRANSACTION_FAILED 14
#define EGGDB_ERR_NEEDS_REOPEN 15
#define EGGDB_ERR_DATABASE_FULL 16
#define EGGDB_ERR_INVALID_OPTION 17
#define EGGDB_ERR_COLLECTION_NOT_FOUND 18
#define EGGDB_ERR_INVALID_COLLECTION_NAME 19
#define EGGDB_ERR_RESERVED_KEY 20
#define EGGDB_ERR_ALREADY_EXISTS 21
#define EGGDB_ERR_NOT_AN_INTEGER 22
#define EGGDB_ERR_OTHER 23
#define EGGDB_ERR_SUSPENDED 24
#define EGGDB_ERR_INVALID_ARGUMENT 100
#define EGGDB_ERR_IN_USE 101
#define EGGDB_ERR_READ_TRANSACTION 102
#define EGGDB_ERR_SQL 103
#define EGGDB_ERR_INTERNAL 104

#define EGGDB_OPEN_READ_ONLY 1
#define EGGDB_OPEN_NO_CREATE 2

#define EGGDB_DURABILITY_FULL 0
#define EGGDB_DURABILITY_NORMAL 1
#define EGGDB_DURABILITY_OFF 2

#define EGGDB_POWER_MAINS 0
#define EGGDB_POWER_BATTERY 1
#define EGGDB_POWER_LOW_BATTERY 2

#define EGGDB_SCAN_REVERSE 1
#define EGGDB_SCAN_END_INCLUSIVE 2
#define EGGDB_SCAN_PREFIX 4

#define EGGDB_NULL 0
#define EGGDB_INTEGER 1
#define EGGDB_REAL 2
#define EGGDB_TEXT 3
#define EGGDB_BLOB 4

typedef struct eggdb_db eggdb_db;
typedef struct eggdb_txn eggdb_txn;
typedef struct eggdb_scan eggdb_scan;
typedef struct eggdb_sql eggdb_sql;
typedef struct eggdb_stmt eggdb_stmt;
typedef struct eggdb_copy eggdb_copy;

typedef struct eggdb_options {
    uint32_t flags;
    uint32_t durability;
    uint64_t cache_bytes;
} eggdb_options;

typedef struct eggdb_backup_report {
    uint64_t stamp;
    uint64_t entries;
    uint64_t key_bytes;
    uint64_t value_bytes;
    uint64_t page_count;
    uint64_t file_bytes;
} eggdb_backup_report;

typedef struct eggdb_compact_report {
    uint64_t stamp;
    uint64_t entries;
    uint64_t pages_before;
    uint64_t free_pages_before;
    uint64_t leaked_pages_before;
    uint64_t pages_after;
    uint64_t bytes_before;
    uint64_t bytes_after;
} eggdb_compact_report;

typedef struct eggdb_entry {
    const uint8_t *key;
    size_t key_len;
    const uint8_t *value;
    size_t value_len;
} eggdb_entry;

typedef struct eggdb_value {
    int32_t kind;
    int64_t integer;
    double real;
    const uint8_t *data;
    size_t len;
} eggdb_value;

typedef struct eggdb_parameter {
    const uint8_t *text;
    size_t len;
} eggdb_parameter;

typedef struct eggdb_result {
    const char *tag;
    size_t column_count;
    const char *const *column_names;
    size_t row_count;
    const eggdb_value *values;
} eggdb_result;

static inline const eggdb_value *eggdb_result_value(const eggdb_result *result, size_t row,
                                                    size_t column) {
    return &result->values[row * result->column_count + column];
}

const char *eggdb_version(void);
const char *eggdb_last_error(void);
const char *eggdb_last_sqlstate(void);
const char *eggdb_status_name(int status);
void eggdb_free(uint8_t *data, size_t len);

int eggdb_open(const char *path, const eggdb_options *options, eggdb_db **db);
int eggdb_close(eggdb_db *db);
int eggdb_backup(eggdb_db *db, const char *path, eggdb_backup_report *report);
int eggdb_compact(eggdb_db *db, eggdb_compact_report *report);
int eggdb_compact_file(const char *path, eggdb_compact_report *report);
int eggdb_remove_expired(eggdb_db *db, uint64_t *removed);
int eggdb_set_power_source(eggdb_db *db, uint32_t source);
int eggdb_suspend(eggdb_db *db);
int eggdb_resume(eggdb_db *db);

int eggdb_begin_read(eggdb_db *db, eggdb_txn **txn);
int eggdb_begin_write(eggdb_db *db, eggdb_txn **txn);
int eggdb_commit(eggdb_txn *txn, uint64_t *stamp);
int eggdb_rollback(eggdb_txn *txn);

int eggdb_get(eggdb_txn *txn, const char *collection, const uint8_t *key, size_t key_len,
              uint8_t **value, size_t *value_len);
int eggdb_put(eggdb_txn *txn, const char *collection, const uint8_t *key, size_t key_len,
              const uint8_t *value, size_t value_len);
int eggdb_put_with_expiry(eggdb_txn *txn, const char *collection, const uint8_t *key,
                          size_t key_len, const uint8_t *value, size_t value_len,
                          uint64_t ttl_ms);
int eggdb_delete(eggdb_txn *txn, const char *collection, const uint8_t *key, size_t key_len,
                 int *deleted);
int eggdb_set_expiry(eggdb_txn *txn, const char *collection, const uint8_t *key,
                     size_t key_len, uint64_t ttl_ms, int *found);
int eggdb_clear_expiry(eggdb_txn *txn, const char *collection, const uint8_t *key,
                       size_t key_len, int *cleared);
int eggdb_time_to_live(eggdb_txn *txn, const char *collection, const uint8_t *key,
                       size_t key_len, int64_t *ttl_ms);
int eggdb_increment(eggdb_txn *txn, const char *collection, const uint8_t *key,
                    size_t key_len, int64_t by, int64_t *count);
int eggdb_compare_and_set(eggdb_txn *txn, const char *collection, const uint8_t *key,
                          size_t key_len, const uint8_t *expected, size_t expected_len,
                          const uint8_t *new_value, size_t new_value_len, int *swapped);

int eggdb_create_collection(eggdb_txn *txn, const char *name, int *created);
int eggdb_drop_collection(eggdb_txn *txn, const char *name, int *dropped);
int eggdb_collection_names(eggdb_txn *txn, uint8_t **names, size_t *names_len, size_t *count);
int eggdb_rename_collection(eggdb_txn *txn, const char *name, const char *new_name);

typedef struct eggdb_collection_stats {
    uint64_t entries;
    uint64_t key_bytes;
    uint64_t value_bytes;
} eggdb_collection_stats;

int eggdb_collection_stats(eggdb_txn *txn, const char *name, eggdb_collection_stats *stats);

int eggdb_scan_open(eggdb_txn *txn, const char *collection, const uint8_t *start,
                    size_t start_len, const uint8_t *end, size_t end_len, uint32_t flags,
                    eggdb_scan **scan);
int eggdb_scan_next(eggdb_txn *txn, eggdb_scan *scan, size_t max_entries,
                    const eggdb_entry **entries, size_t *count);
void eggdb_scan_free(eggdb_scan *scan);

int eggdb_sql_open(eggdb_db *db, const char *dialect, eggdb_sql **sql);
int eggdb_sql_connect(const char *url, eggdb_sql **sql);
int eggdb_sql_execute(eggdb_sql *sql, const char *text, const eggdb_result **results,
                      size_t *count);
int eggdb_sql_execute_with_parameters(eggdb_sql *sql, const char *text,
                                      const eggdb_parameter *parameters, size_t parameter_count,
                                      const eggdb_result **results, size_t *count);
int eggdb_sql_copy_in(eggdb_sql *sql, const char *text, const uint8_t *data, size_t len,
                      const eggdb_result **results, size_t *count);
int eggdb_sql_cancel(eggdb_sql *sql);
int eggdb_sql_close(eggdb_sql *sql);

int eggdb_sql_prepare(eggdb_sql *sql, const char *text, size_t len, eggdb_stmt **stmt);
int eggdb_stmt_parameter_count(eggdb_stmt *stmt, size_t *count);
int eggdb_stmt_bind_null(eggdb_stmt *stmt, size_t index);
int eggdb_stmt_bind_int64(eggdb_stmt *stmt, size_t index, int64_t value);
int eggdb_stmt_bind_double(eggdb_stmt *stmt, size_t index, double value);
int eggdb_stmt_bind_bool(eggdb_stmt *stmt, size_t index, int value);
int eggdb_stmt_bind_text(eggdb_stmt *stmt, size_t index, const char *text, size_t len);
int eggdb_stmt_bind_blob(eggdb_stmt *stmt, size_t index, const uint8_t *data, size_t len);
int eggdb_stmt_clear_bindings(eggdb_stmt *stmt);
int eggdb_stmt_execute(eggdb_stmt *stmt, const eggdb_result **results, size_t *count);
int eggdb_stmt_reset(eggdb_stmt *stmt);
int eggdb_stmt_column_count(eggdb_stmt *stmt, size_t *count);
int eggdb_stmt_column_name(eggdb_stmt *stmt, size_t column, const char **name);
int eggdb_stmt_column_type(eggdb_stmt *stmt, size_t column, int32_t *kind);
int eggdb_stmt_step(eggdb_stmt *stmt, int *has_row);
int eggdb_stmt_row(eggdb_stmt *stmt, const eggdb_value **values, size_t *count);
int eggdb_stmt_value_kind(eggdb_stmt *stmt, size_t column, int32_t *kind);
int eggdb_stmt_value_int64(eggdb_stmt *stmt, size_t column, int64_t *value);
int eggdb_stmt_value_double(eggdb_stmt *stmt, size_t column, double *value);
int eggdb_stmt_value_text(eggdb_stmt *stmt, size_t column, const char **text, size_t *len);
int eggdb_stmt_value_blob(eggdb_stmt *stmt, size_t column, const uint8_t **data, size_t *len);
int eggdb_stmt_finalize(eggdb_stmt *stmt);

int eggdb_sql_copy_in_begin(eggdb_sql *sql, const char *text, eggdb_copy **copy);
int eggdb_sql_copy_out_begin(eggdb_sql *sql, const char *text, eggdb_copy **copy);
int eggdb_copy_put(eggdb_copy *copy, const uint8_t *data, size_t len);
int eggdb_copy_read(eggdb_copy *copy, const uint8_t **data, size_t *len);
int eggdb_copy_end(eggdb_copy *copy, const eggdb_result **results, size_t *count);
void eggdb_copy_free(eggdb_copy *copy);

#ifdef __cplusplus
}
#endif

#endif
