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

#ifdef __cplusplus
}
#endif

#endif
