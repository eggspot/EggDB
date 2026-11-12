#ifndef EGGDB_SQLITE3_H
#define EGGDB_SQLITE3_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct sqlite3 sqlite3;
typedef struct sqlite3_stmt sqlite3_stmt;
typedef int64_t sqlite3_int64;
typedef void (*sqlite3_destructor_type)(void *);

#define SQLITE_OK 0
#define SQLITE_ERROR 1
#define SQLITE_ABORT 4
#define SQLITE_BUSY 5
#define SQLITE_NOMEM 7
#define SQLITE_CANTOPEN 14
#define SQLITE_CONSTRAINT 19
#define SQLITE_MISUSE 21
#define SQLITE_RANGE 25
#define SQLITE_ROW 100
#define SQLITE_DONE 101

#define SQLITE_INTEGER 1
#define SQLITE_FLOAT 2
#define SQLITE_TEXT 3
#define SQLITE_BLOB 4
#define SQLITE_NULL 5

#define SQLITE_OPEN_READONLY 0x00000001
#define SQLITE_OPEN_READWRITE 0x00000002
#define SQLITE_OPEN_CREATE 0x00000004

#define SQLITE_STATIC ((sqlite3_destructor_type)0)
#define SQLITE_TRANSIENT ((sqlite3_destructor_type)-1)

const char *sqlite3_libversion(void);
int sqlite3_libversion_number(void);
int sqlite3_open(const char *filename, sqlite3 **db);
int sqlite3_open_v2(const char *filename, sqlite3 **db, int flags, const char *vfs);
int sqlite3_close(sqlite3 *db);
int sqlite3_close_v2(sqlite3 *db);
int sqlite3_exec(sqlite3 *db, const char *sql, int (*callback)(void *, int, char **, char **), void *argument, char **errmsg);
void sqlite3_free(void *pointer);
int sqlite3_prepare_v2(sqlite3 *db, const char *sql, int length, sqlite3_stmt **statement, const char **tail);
int sqlite3_bind_int(sqlite3_stmt *statement, int index, int value);
int sqlite3_bind_int64(sqlite3_stmt *statement, int index, sqlite3_int64 value);
int sqlite3_bind_double(sqlite3_stmt *statement, int index, double value);
int sqlite3_bind_null(sqlite3_stmt *statement, int index);
int sqlite3_bind_text(sqlite3_stmt *statement, int index, const char *text, int length, sqlite3_destructor_type destructor);
int sqlite3_bind_blob(sqlite3_stmt *statement, int index, const void *blob, int length, sqlite3_destructor_type destructor);
int sqlite3_bind_parameter_count(sqlite3_stmt *statement);
int sqlite3_step(sqlite3_stmt *statement);
int sqlite3_reset(sqlite3_stmt *statement);
int sqlite3_clear_bindings(sqlite3_stmt *statement);
int sqlite3_finalize(sqlite3_stmt *statement);
int sqlite3_column_count(sqlite3_stmt *statement);
const char *sqlite3_column_name(sqlite3_stmt *statement, int column);
int sqlite3_column_type(sqlite3_stmt *statement, int column);
int sqlite3_column_int(sqlite3_stmt *statement, int column);
sqlite3_int64 sqlite3_column_int64(sqlite3_stmt *statement, int column);
double sqlite3_column_double(sqlite3_stmt *statement, int column);
const unsigned char *sqlite3_column_text(sqlite3_stmt *statement, int column);
const void *sqlite3_column_blob(sqlite3_stmt *statement, int column);
int sqlite3_column_bytes(sqlite3_stmt *statement, int column);
const char *sqlite3_errmsg(sqlite3 *db);
int sqlite3_errcode(sqlite3 *db);
int sqlite3_changes(sqlite3 *db);
int sqlite3_total_changes(sqlite3 *db);
sqlite3_int64 sqlite3_last_insert_rowid(sqlite3 *db);

typedef struct sqlite3_context sqlite3_context;
typedef struct sqlite3_value sqlite3_value;

#define SQLITE_UTF8 1
#define SQLITE_DETERMINISTIC 0x000000800
#define SQLITE_TXN_NONE 0
#define SQLITE_TXN_READ 1
#define SQLITE_TXN_WRITE 2

int sqlite3_create_function(sqlite3 *db, const char *name, int arguments, int text_representation, void *user_data, void (*function)(sqlite3_context *, int, sqlite3_value **), void (*step)(sqlite3_context *, int, sqlite3_value **), void (*last)(sqlite3_context *));
int sqlite3_create_function_v2(sqlite3 *db, const char *name, int arguments, int text_representation, void *user_data, void (*function)(sqlite3_context *, int, sqlite3_value **), void (*step)(sqlite3_context *, int, sqlite3_value **), void (*last)(sqlite3_context *), void (*destroy)(void *));
int sqlite3_value_type(sqlite3_value *value);
int sqlite3_value_int(sqlite3_value *value);
sqlite3_int64 sqlite3_value_int64(sqlite3_value *value);
double sqlite3_value_double(sqlite3_value *value);
const unsigned char *sqlite3_value_text(sqlite3_value *value);
const void *sqlite3_value_blob(sqlite3_value *value);
int sqlite3_value_bytes(sqlite3_value *value);
void sqlite3_result_null(sqlite3_context *context);
void sqlite3_result_int(sqlite3_context *context, int value);
void sqlite3_result_int64(sqlite3_context *context, sqlite3_int64 value);
void sqlite3_result_double(sqlite3_context *context, double value);
void sqlite3_result_text(sqlite3_context *context, const char *text, int length, sqlite3_destructor_type destructor);
void sqlite3_result_blob(sqlite3_context *context, const void *blob, int length, sqlite3_destructor_type destructor);
void sqlite3_result_error(sqlite3_context *context, const char *message, int length);
void sqlite3_result_error_nomem(sqlite3_context *context);
void sqlite3_result_error_toobig(sqlite3_context *context);
void sqlite3_result_value(sqlite3_context *context, sqlite3_value *value);
void *sqlite3_user_data(sqlite3_context *context);
sqlite3 *sqlite3_context_db_handle(sqlite3_context *context);

const char *sqlite3_errstr(int code);
int sqlite3_extended_errcode(sqlite3 *db);
int sqlite3_extended_result_codes(sqlite3 *db, int on);
int sqlite3_get_autocommit(sqlite3 *db);
int sqlite3_txn_state(sqlite3 *db, const char *schema);
int sqlite3_busy_timeout(sqlite3 *db, int milliseconds);
void sqlite3_interrupt(sqlite3 *db);
int sqlite3_threadsafe(void);
const char *sqlite3_sourceid(void);
int sqlite3_complete(const char *sql);
const char *sqlite3_sql(sqlite3_stmt *statement);
int sqlite3_data_count(sqlite3_stmt *statement);
int sqlite3_stmt_readonly(sqlite3_stmt *statement);
int sqlite3_stmt_busy(sqlite3_stmt *statement);
sqlite3 *sqlite3_db_handle(sqlite3_stmt *statement);
const char *sqlite3_column_decltype(sqlite3_stmt *statement, int column);
const char *sqlite3_bind_parameter_name(sqlite3_stmt *statement, int index);
int sqlite3_bind_parameter_index(sqlite3_stmt *statement, const char *name);

#ifdef __cplusplus
}
#endif

#endif
