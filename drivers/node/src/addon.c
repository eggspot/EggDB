#include <node_api.h>
#include <stdlib.h>
#include <string.h>

#include "eggdb.h"

typedef struct connection {
    eggdb_db *database;
    eggdb_sql *session;
} connection;

static napi_value throw_last(napi_env env, int status) {
    const char *message = eggdb_last_error();
    const char *sqlstate = eggdb_last_sqlstate();
    napi_value text;
    napi_value code;
    napi_value error;
    napi_create_string_utf8(env, message && *message ? message : "EggDB failed", NAPI_AUTO_LENGTH, &text);
    napi_create_string_utf8(env, sqlstate && *sqlstate ? sqlstate : "", NAPI_AUTO_LENGTH, &code);
    napi_create_error(env, code, text, &error);
    napi_value number;
    napi_create_int32(env, status, &number);
    napi_set_named_property(env, error, "status", number);
    napi_throw(env, error);
    return NULL;
}

static napi_value throw_message(napi_env env, const char *message) {
    napi_throw_error(env, NULL, message);
    return NULL;
}

static char *string_argument(napi_env env, napi_value value, size_t *written) {
    size_t length = 0;
    if (napi_get_value_string_utf8(env, value, NULL, 0, &length) != napi_ok) {
        return NULL;
    }
    char *text = malloc(length + 1);
    if (text == NULL) {
        return NULL;
    }
    if (napi_get_value_string_utf8(env, value, text, length + 1, &length) != napi_ok) {
        free(text);
        return NULL;
    }
    if (written != NULL) {
        *written = length;
    }
    return text;
}

static void finalize_connection(napi_env env, void *data, void *hint) {
    (void)env;
    (void)hint;
    connection *open = data;
    if (open->session != NULL) {
        eggdb_sql_close(open->session);
    }
    if (open->database != NULL) {
        eggdb_close(open->database);
    }
    free(open);
}

static napi_value open_connection(napi_env env, napi_callback_info info) {
    size_t count = 3;
    napi_value arguments[3];
    napi_get_cb_info(env, info, &count, arguments, NULL, NULL);
    if (count < 3) {
        return throw_message(env, "open(path, dialect, readOnly) needs three arguments");
    }
    char *path = string_argument(env, arguments[0], NULL);
    char *dialect = string_argument(env, arguments[1], NULL);
    bool read_only = false;
    napi_get_value_bool(env, arguments[2], &read_only);
    if (path == NULL || dialect == NULL) {
        free(path);
        free(dialect);
        return throw_message(env, "the path and the dialect must be strings");
    }
    connection *open = calloc(1, sizeof(connection));
    if (open == NULL) {
        free(path);
        free(dialect);
        return throw_message(env, "out of memory");
    }
    int status;
    if (strncmp(path, "postgresql://", 13) == 0 || strncmp(path, "postgres://", 11) == 0) {
        status = eggdb_sql_connect(path, &open->session);
    } else {
        eggdb_options options = {0};
        options.flags = read_only ? EGGDB_OPEN_READ_ONLY : 0;
        status = eggdb_open(path, &options, &open->database);
        if (status == EGGDB_OK) {
            status = eggdb_sql_open(open->database, dialect, &open->session);
        }
    }
    free(path);
    free(dialect);
    if (status != EGGDB_OK) {
        napi_value thrown = throw_last(env, status);
        finalize_connection(env, open, NULL);
        return thrown;
    }
    napi_value handle;
    napi_create_external(env, open, finalize_connection, NULL, &handle);
    return handle;
}

static connection *connection_of(napi_env env, napi_value handle) {
    void *data = NULL;
    if (napi_get_value_external(env, handle, &data) != napi_ok || data == NULL) {
        return NULL;
    }
    return data;
}

static napi_value close_connection(napi_env env, napi_callback_info info) {
    size_t count = 1;
    napi_value handle;
    napi_get_cb_info(env, info, &count, &handle, NULL, NULL);
    connection *open = count == 1 ? connection_of(env, handle) : NULL;
    if (open == NULL) {
        return throw_message(env, "close(handle) needs a connection handle");
    }
    if (open->session != NULL) {
        eggdb_sql_close(open->session);
        open->session = NULL;
    }
    if (open->database != NULL) {
        eggdb_close(open->database);
        open->database = NULL;
    }
    napi_value nothing;
    napi_get_undefined(env, &nothing);
    return nothing;
}

static napi_value converted(napi_env env, const eggdb_value *value) {
    napi_value out;
    switch (value->kind) {
        case EGGDB_INTEGER:
            if (value->integer > 9007199254740991LL || value->integer < -9007199254740991LL) {
                napi_create_bigint_int64(env, value->integer, &out);
            } else {
                napi_create_int64(env, value->integer, &out);
            }
            return out;
        case EGGDB_REAL:
            napi_create_double(env, value->real, &out);
            return out;
        case EGGDB_TEXT:
            napi_create_string_utf8(env, (const char *)value->data, value->len, &out);
            return out;
        case EGGDB_BLOB:
            napi_create_buffer_copy(env, value->len, value->data, NULL, &out);
            return out;
        default:
            napi_get_null(env, &out);
            return out;
    }
}

static napi_value gathered(napi_env env, const eggdb_result *results, size_t count) {
    napi_value list;
    napi_create_array_with_length(env, count, &list);
    for (size_t index = 0; index < count; index++) {
        const eggdb_result *result = &results[index];
        napi_value item;
        napi_value tag;
        napi_value columns;
        napi_value rows;
        napi_create_object(env, &item);
        napi_create_string_utf8(env, result->tag ? result->tag : "", NAPI_AUTO_LENGTH, &tag);
        napi_create_array_with_length(env, result->column_count, &columns);
        for (size_t column = 0; column < result->column_count; column++) {
            napi_value name;
            napi_create_string_utf8(env, result->column_names[column], NAPI_AUTO_LENGTH, &name);
            napi_set_element(env, columns, (uint32_t)column, name);
        }
        napi_create_array_with_length(env, result->row_count, &rows);
        for (size_t row = 0; row < result->row_count; row++) {
            napi_value cells;
            napi_create_array_with_length(env, result->column_count, &cells);
            for (size_t column = 0; column < result->column_count; column++) {
                napi_set_element(env, cells, (uint32_t)column, converted(env, eggdb_result_value(result, row, column)));
            }
            napi_set_element(env, rows, (uint32_t)row, cells);
        }
        napi_set_named_property(env, item, "tag", tag);
        napi_set_named_property(env, item, "columns", columns);
        napi_set_named_property(env, item, "rows", rows);
        napi_set_element(env, list, (uint32_t)index, item);
    }
    return list;
}

static void free_parameters(eggdb_parameter *parameters, uint32_t count) {
    for (uint32_t index = 0; index < count; index++) {
        free((void *)parameters[index].text);
    }
    free(parameters);
}

static napi_value execute(napi_env env, napi_callback_info info) {
    size_t count = 3;
    napi_value arguments[3];
    napi_get_cb_info(env, info, &count, arguments, NULL, NULL);
    connection *open = count == 3 ? connection_of(env, arguments[0]) : NULL;
    if (open == NULL || open->session == NULL) {
        return throw_message(env, "the connection is closed");
    }
    char *sql = string_argument(env, arguments[1], NULL);
    if (sql == NULL) {
        return throw_message(env, "the SQL must be a string");
    }
    uint32_t parameter_count = 0;
    napi_get_array_length(env, arguments[2], &parameter_count);
    eggdb_parameter *parameters = calloc(parameter_count ? parameter_count : 1, sizeof(eggdb_parameter));
    if (parameters == NULL) {
        free(sql);
        return throw_message(env, "out of memory");
    }
    for (uint32_t index = 0; index < parameter_count; index++) {
        napi_value element;
        napi_valuetype type;
        napi_get_element(env, arguments[2], index, &element);
        napi_typeof(env, element, &type);
        if (type == napi_null || type == napi_undefined) {
            continue;
        }
        size_t length = 0;
        char *text = string_argument(env, element, &length);
        if (text == NULL) {
            free(sql);
            free_parameters(parameters, parameter_count);
            return throw_message(env, "parameters reach the addon as strings or null");
        }
        parameters[index].text = (const uint8_t *)text;
        parameters[index].len = length;
    }
    const eggdb_result *results = NULL;
    size_t result_count = 0;
    int status = eggdb_sql_execute_with_parameters(open->session, sql, parameters, parameter_count, &results,
                                                   &result_count);
    free(sql);
    free_parameters(parameters, parameter_count);
    if (status != EGGDB_OK) {
        return throw_last(env, status);
    }
    return gathered(env, results, result_count);
}

typedef struct store {
    eggdb_db *database;
} store;

typedef struct transaction {
    eggdb_txn *txn;
} transaction;

static void finalize_store(napi_env env, void *data, void *hint) {
    (void)env;
    (void)hint;
    store *open = data;
    if (open->database != NULL) {
        eggdb_close(open->database);
    }
    free(open);
}

static void finalize_transaction(napi_env env, void *data, void *hint) {
    (void)env;
    (void)hint;
    transaction *open = data;
    if (open->txn != NULL) {
        eggdb_rollback(open->txn);
    }
    free(open);
}

static napi_value undefined_value(napi_env env) {
    napi_value nothing;
    napi_get_undefined(env, &nothing);
    return nothing;
}

static napi_value boolean_value(napi_env env, int flag) {
    napi_value out;
    napi_get_boolean(env, flag != 0, &out);
    return out;
}

static napi_value buffer_of(napi_env env, const uint8_t *data, size_t length) {
    napi_value out;
    void *copied = NULL;
    napi_create_buffer_copy(env, length, data, &copied, &out);
    return out;
}

static int bytes_argument(napi_env env, napi_value value, const uint8_t **data, size_t *length) {
    void *found = NULL;
    size_t size = 0;
    if (napi_get_buffer_info(env, value, &found, &size) != napi_ok) {
        return 0;
    }
    *data = found;
    *length = size;
    return 1;
}

static int is_nullish(napi_env env, napi_value value) {
    napi_valuetype type;
    napi_typeof(env, value, &type);
    return type == napi_null || type == napi_undefined;
}

static char *collection_argument(napi_env env, napi_value value, int *failed) {
    *failed = 0;
    if (is_nullish(env, value)) {
        return NULL;
    }
    char *name = string_argument(env, value, NULL);
    if (name == NULL) {
        *failed = 1;
    }
    return name;
}

static transaction *transaction_of(napi_env env, napi_value handle) {
    void *data = NULL;
    if (napi_get_value_external(env, handle, &data) != napi_ok || data == NULL) {
        return NULL;
    }
    transaction *open = data;
    return open->txn == NULL ? NULL : open;
}

static napi_value kv_open(napi_env env, napi_callback_info info) {
    size_t count = 2;
    napi_value arguments[2];
    napi_get_cb_info(env, info, &count, arguments, NULL, NULL);
    if (count < 2) {
        return throw_message(env, "kvOpen(path, readOnly) needs two arguments");
    }
    char *path = string_argument(env, arguments[0], NULL);
    if (path == NULL) {
        return throw_message(env, "the path must be a string");
    }
    bool read_only = false;
    napi_get_value_bool(env, arguments[1], &read_only);
    store *open = calloc(1, sizeof(store));
    if (open == NULL) {
        free(path);
        return throw_message(env, "out of memory");
    }
    eggdb_options options = {0};
    options.flags = read_only ? EGGDB_OPEN_READ_ONLY : 0;
    int status = eggdb_open(path, &options, &open->database);
    free(path);
    if (status != EGGDB_OK) {
        free(open);
        return throw_last(env, status);
    }
    napi_value handle;
    napi_create_external(env, open, finalize_store, NULL, &handle);
    return handle;
}

static napi_value kv_close(napi_env env, napi_callback_info info) {
    size_t count = 1;
    napi_value handle;
    napi_get_cb_info(env, info, &count, &handle, NULL, NULL);
    void *data = NULL;
    if (count != 1 || napi_get_value_external(env, handle, &data) != napi_ok || data == NULL) {
        return throw_message(env, "kvClose(handle) needs a store handle");
    }
    store *open = data;
    if (open->database != NULL) {
        eggdb_close(open->database);
        open->database = NULL;
    }
    return undefined_value(env);
}

static napi_value kv_begin(napi_env env, napi_callback_info info) {
    size_t count = 2;
    napi_value arguments[2];
    napi_get_cb_info(env, info, &count, arguments, NULL, NULL);
    void *data = NULL;
    if (count < 2 || napi_get_value_external(env, arguments[0], &data) != napi_ok || data == NULL) {
        return throw_message(env, "kvBegin(store, writing) needs a store handle");
    }
    store *open = data;
    if (open->database == NULL) {
        return throw_message(env, "the store is closed");
    }
    bool writing = false;
    napi_get_value_bool(env, arguments[1], &writing);
    transaction *begun = calloc(1, sizeof(transaction));
    if (begun == NULL) {
        return throw_message(env, "out of memory");
    }
    int status = writing ? eggdb_begin_write(open->database, &begun->txn) : eggdb_begin_read(open->database, &begun->txn);
    if (status != EGGDB_OK) {
        free(begun);
        return throw_last(env, status);
    }
    napi_value handle;
    napi_create_external(env, begun, finalize_transaction, NULL, &handle);
    return handle;
}

static napi_value kv_end(napi_env env, napi_callback_info info) {
    size_t count = 2;
    napi_value arguments[2];
    napi_get_cb_info(env, info, &count, arguments, NULL, NULL);
    transaction *open = count == 2 ? transaction_of(env, arguments[0]) : NULL;
    if (open == NULL) {
        return throw_message(env, "kvEnd(txn, commit) needs a live transaction");
    }
    bool commit = false;
    napi_get_value_bool(env, arguments[1], &commit);
    eggdb_txn *ending = open->txn;
    open->txn = NULL;
    if (!commit) {
        eggdb_rollback(ending);
        return undefined_value(env);
    }
    uint64_t stamp = 0;
    int status = eggdb_commit(ending, &stamp);
    if (status != EGGDB_OK) {
        return throw_last(env, status);
    }
    napi_value out;
    napi_create_bigint_uint64(env, stamp, &out);
    return out;
}

static napi_value kv_get(napi_env env, napi_callback_info info) {
    size_t count = 3;
    napi_value arguments[3];
    napi_get_cb_info(env, info, &count, arguments, NULL, NULL);
    transaction *open = count == 3 ? transaction_of(env, arguments[0]) : NULL;
    const uint8_t *key = NULL;
    size_t key_len = 0;
    if (open == NULL || !bytes_argument(env, arguments[2], &key, &key_len)) {
        return throw_message(env, "kvGet(txn, collection, key) needs a live transaction and a Buffer key");
    }
    int failed = 0;
    char *collection = collection_argument(env, arguments[1], &failed);
    if (failed) {
        return throw_message(env, "the collection must be a string or null");
    }
    uint8_t *value = NULL;
    size_t value_len = 0;
    int status = eggdb_get(open->txn, collection, key, key_len, &value, &value_len);
    free(collection);
    if (status != EGGDB_OK) {
        return throw_last(env, status);
    }
    if (value == NULL) {
        napi_value nothing;
        napi_get_null(env, &nothing);
        return nothing;
    }
    napi_value out = buffer_of(env, value, value_len);
    eggdb_free(value, value_len);
    return out;
}

static napi_value kv_put(napi_env env, napi_callback_info info) {
    size_t count = 5;
    napi_value arguments[5];
    napi_get_cb_info(env, info, &count, arguments, NULL, NULL);
    transaction *open = count == 5 ? transaction_of(env, arguments[0]) : NULL;
    const uint8_t *key = NULL;
    const uint8_t *value = NULL;
    size_t key_len = 0;
    size_t value_len = 0;
    if (open == NULL || !bytes_argument(env, arguments[2], &key, &key_len) || !bytes_argument(env, arguments[3], &value, &value_len)) {
        return throw_message(env, "kvPut(txn, collection, key, value, ttlMs) needs a live transaction and Buffers");
    }
    int failed = 0;
    char *collection = collection_argument(env, arguments[1], &failed);
    if (failed) {
        return throw_message(env, "the collection must be a string or null");
    }
    int status;
    if (is_nullish(env, arguments[4])) {
        status = eggdb_put(open->txn, collection, key, key_len, value, value_len);
    } else {
        int64_t ttl = 0;
        napi_get_value_int64(env, arguments[4], &ttl);
        status = eggdb_put_with_expiry(open->txn, collection, key, key_len, value, value_len, ttl > 0 ? (uint64_t)ttl : 0);
    }
    free(collection);
    if (status != EGGDB_OK) {
        return throw_last(env, status);
    }
    return undefined_value(env);
}

static napi_value kv_delete(napi_env env, napi_callback_info info) {
    size_t count = 3;
    napi_value arguments[3];
    napi_get_cb_info(env, info, &count, arguments, NULL, NULL);
    transaction *open = count == 3 ? transaction_of(env, arguments[0]) : NULL;
    const uint8_t *key = NULL;
    size_t key_len = 0;
    if (open == NULL || !bytes_argument(env, arguments[2], &key, &key_len)) {
        return throw_message(env, "kvDelete(txn, collection, key) needs a live transaction and a Buffer key");
    }
    int failed = 0;
    char *collection = collection_argument(env, arguments[1], &failed);
    if (failed) {
        return throw_message(env, "the collection must be a string or null");
    }
    int deleted = 0;
    int status = eggdb_delete(open->txn, collection, key, key_len, &deleted);
    free(collection);
    if (status != EGGDB_OK) {
        return throw_last(env, status);
    }
    return boolean_value(env, deleted);
}

static napi_value kv_scan(napi_env env, napi_callback_info info) {
    size_t count = 5;
    napi_value arguments[5];
    napi_get_cb_info(env, info, &count, arguments, NULL, NULL);
    transaction *open = count == 5 ? transaction_of(env, arguments[0]) : NULL;
    if (open == NULL) {
        return throw_message(env, "kvScan(txn, collection, start, end, flags) needs a live transaction");
    }
    const uint8_t *start = NULL;
    const uint8_t *end = NULL;
    size_t start_len = 0;
    size_t end_len = 0;
    if (!is_nullish(env, arguments[2]) && !bytes_argument(env, arguments[2], &start, &start_len)) {
        return throw_message(env, "start must be a Buffer or null");
    }
    if (!is_nullish(env, arguments[3]) && !bytes_argument(env, arguments[3], &end, &end_len)) {
        return throw_message(env, "end must be a Buffer or null");
    }
    uint32_t flags = 0;
    napi_get_value_uint32(env, arguments[4], &flags);
    int failed = 0;
    char *collection = collection_argument(env, arguments[1], &failed);
    if (failed) {
        return throw_message(env, "the collection must be a string or null");
    }
    eggdb_scan *scan = NULL;
    int status = eggdb_scan_open(open->txn, collection, start, start_len, end, end_len, flags, &scan);
    free(collection);
    if (status != EGGDB_OK) {
        return throw_last(env, status);
    }
    napi_value rows;
    napi_create_array(env, &rows);
    uint32_t taken = 0;
    for (;;) {
        const eggdb_entry *entries = NULL;
        size_t batch = 0;
        status = eggdb_scan_next(open->txn, scan, 0, &entries, &batch);
        if (status != EGGDB_OK) {
            eggdb_scan_free(scan);
            return throw_last(env, status);
        }
        if (batch == 0) {
            break;
        }
        for (size_t index = 0; index < batch; index++) {
            napi_value pair;
            napi_create_array_with_length(env, 2, &pair);
            napi_set_element(env, pair, 0, buffer_of(env, entries[index].key, entries[index].key_len));
            napi_set_element(env, pair, 1, buffer_of(env, entries[index].value, entries[index].value_len));
            napi_set_element(env, rows, taken++, pair);
        }
    }
    eggdb_scan_free(scan);
    return rows;
}

static napi_value kv_collection(napi_env env, napi_callback_info info) {
    size_t count = 4;
    napi_value arguments[4];
    napi_get_cb_info(env, info, &count, arguments, NULL, NULL);
    transaction *open = count >= 3 ? transaction_of(env, arguments[0]) : NULL;
    if (open == NULL) {
        return throw_message(env, "kvCollection(txn, action, name[, newName]) needs a live transaction");
    }
    char *action = string_argument(env, arguments[1], NULL);
    char *name = string_argument(env, arguments[2], NULL);
    if (action == NULL || name == NULL) {
        free(action);
        free(name);
        return throw_message(env, "the action and the name must be strings");
    }
    int flag = 0;
    int status;
    napi_value out = NULL;
    if (strcmp(action, "create") == 0) {
        status = eggdb_create_collection(open->txn, name, &flag);
        out = boolean_value(env, flag);
    } else if (strcmp(action, "drop") == 0) {
        status = eggdb_drop_collection(open->txn, name, &flag);
        out = boolean_value(env, flag);
    } else if (strcmp(action, "rename") == 0 && count == 4) {
        char *new_name = string_argument(env, arguments[3], NULL);
        status = new_name == NULL ? EGGDB_ERR_INVALID_ARGUMENT : eggdb_rename_collection(open->txn, name, new_name);
        free(new_name);
        out = undefined_value(env);
    } else if (strcmp(action, "stats") == 0) {
        eggdb_collection_stats stats = {0};
        status = eggdb_collection_stats(open->txn, name, &stats);
        napi_create_object(env, &out);
        napi_value number;
        napi_create_bigint_uint64(env, stats.entries, &number);
        napi_set_named_property(env, out, "entries", number);
        napi_create_bigint_uint64(env, stats.key_bytes, &number);
        napi_set_named_property(env, out, "keyBytes", number);
        napi_create_bigint_uint64(env, stats.value_bytes, &number);
        napi_set_named_property(env, out, "valueBytes", number);
    } else {
        free(action);
        free(name);
        return throw_message(env, "the action is create, drop, rename or stats");
    }
    free(action);
    free(name);
    if (status != EGGDB_OK) {
        return throw_last(env, status);
    }
    return out;
}

static napi_value kv_collections(napi_env env, napi_callback_info info) {
    size_t count = 1;
    napi_value handle;
    napi_get_cb_info(env, info, &count, &handle, NULL, NULL);
    transaction *open = count == 1 ? transaction_of(env, handle) : NULL;
    if (open == NULL) {
        return throw_message(env, "kvCollections(txn) needs a live transaction");
    }
    uint8_t *names = NULL;
    size_t names_len = 0;
    size_t listed = 0;
    int status = eggdb_collection_names(open->txn, &names, &names_len, &listed);
    if (status != EGGDB_OK) {
        return throw_last(env, status);
    }
    napi_value out;
    napi_create_array_with_length(env, listed, &out);
    size_t at = 0;
    for (size_t index = 0; index < listed && at < names_len; index++) {
        size_t length = strnlen((const char *)names + at, names_len - at);
        napi_value name;
        napi_create_string_utf8(env, (const char *)names + at, length, &name);
        napi_set_element(env, out, (uint32_t)index, name);
        at += length + 1;
    }
    if (names != NULL) {
        eggdb_free(names, names_len);
    }
    return out;
}

static napi_value init(napi_env env, napi_value exports) {
    napi_value function;
    napi_create_function(env, "open", NAPI_AUTO_LENGTH, open_connection, NULL, &function);
    napi_set_named_property(env, exports, "open", function);
    napi_create_function(env, "execute", NAPI_AUTO_LENGTH, execute, NULL, &function);
    napi_set_named_property(env, exports, "execute", function);
    napi_create_function(env, "close", NAPI_AUTO_LENGTH, close_connection, NULL, &function);
    napi_set_named_property(env, exports, "close", function);
    const struct {
        const char *name;
        napi_callback callback;
    } key_value[] = {
        {"kvOpen", kv_open},
        {"kvClose", kv_close},
        {"kvBegin", kv_begin},
        {"kvEnd", kv_end},
        {"kvGet", kv_get},
        {"kvPut", kv_put},
        {"kvDelete", kv_delete},
        {"kvScan", kv_scan},
        {"kvCollection", kv_collection},
        {"kvCollections", kv_collections},
    };
    for (size_t index = 0; index < sizeof key_value / sizeof key_value[0]; index++) {
        napi_create_function(env, key_value[index].name, NAPI_AUTO_LENGTH, key_value[index].callback, NULL, &function);
        napi_set_named_property(env, exports, key_value[index].name, function);
    }
    return exports;
}

NAPI_MODULE(eggdb, init)
