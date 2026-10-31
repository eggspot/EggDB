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

static napi_value init(napi_env env, napi_value exports) {
    napi_value function;
    napi_create_function(env, "open", NAPI_AUTO_LENGTH, open_connection, NULL, &function);
    napi_set_named_property(env, exports, "open", function);
    napi_create_function(env, "execute", NAPI_AUTO_LENGTH, execute, NULL, &function);
    napi_set_named_property(env, exports, "execute", function);
    napi_create_function(env, "close", NAPI_AUTO_LENGTH, close_connection, NULL, &function);
    napi_set_named_property(env, exports, "close", function);
    return exports;
}

NAPI_MODULE(eggdb, init)
