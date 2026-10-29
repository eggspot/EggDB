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

int eggdb_scan_open(eggdb_txn *txn, const char *collection, const uint8_t *start,
                    size_t start_len, const uint8_t *end, size_t end_len, uint32_t flags,
                    eggdb_scan **scan);
int eggdb_scan_next(eggdb_txn *txn, eggdb_scan *scan, size_t max_entries,
                    const eggdb_entry **entries, size_t *count);
void eggdb_scan_free(eggdb_scan *scan);

int eggdb_sql_open(eggdb_db *db, const char *dialect, eggdb_sql **sql);
int eggdb_sql_execute(eggdb_sql *sql, const char *text, const eggdb_result **results,
                      size_t *count);
int eggdb_sql_close(eggdb_sql *sql);

#ifdef __cplusplus
}
#endif

#endif
