#include <jni.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "eggdb.h"

typedef struct eggdb_jni_handle {
    eggdb_db *db;
    eggdb_sql *sql;
} eggdb_jni_handle;

static void throw_failure(JNIEnv *env, const char *fallback) {
    const char *message = eggdb_last_error();
    const char *state = eggdb_last_sqlstate();
    char text[4096];
    if (state != NULL && state[0] != '\0') {
        snprintf(text, sizeof text, "%s (SQLSTATE %s)", message != NULL ? message : fallback, state);
    } else {
        snprintf(text, sizeof text, "%s", message != NULL ? message : fallback);
    }
    jclass failure = (*env)->FindClass(env, "app/eggspot/eggdb/EggDBException");
    if (failure != NULL) {
        (*env)->ThrowNew(env, failure, text);
    }
}

static char *terminated(JNIEnv *env, jbyteArray bytes) {
    jsize length = (*env)->GetArrayLength(env, bytes);
    char *text = malloc((size_t)length + 1);
    if (text == NULL) {
        return NULL;
    }
    (*env)->GetByteArrayRegion(env, bytes, 0, length, (jbyte *)text);
    text[length] = '\0';
    if (memchr(text, '\0', (size_t)length) != NULL) {
        free(text);
        return NULL;
    }
    return text;
}

static jbyteArray byte_array(JNIEnv *env, const void *data, size_t length) {
    jbyteArray array = (*env)->NewByteArray(env, (jsize)length);
    if (array != NULL && length > 0) {
        (*env)->SetByteArrayRegion(env, array, 0, (jsize)length, (const jbyte *)data);
    }
    return array;
}

static jbyteArray cell(JNIEnv *env, const eggdb_value *value) {
    char number[64];
    unsigned char *buffer;
    size_t length;
    jbyteArray array;
    switch (value->kind) {
    case EGGDB_NULL:
        return NULL;
    case EGGDB_INTEGER:
        length = (size_t)snprintf(number + 1, sizeof number - 1, "%lld", (long long)value->integer) + 1;
        number[0] = (char)EGGDB_INTEGER;
        return byte_array(env, number, length);
    case EGGDB_REAL:
        length = (size_t)snprintf(number + 1, sizeof number - 1, "%.17g", value->real) + 1;
        number[0] = (char)EGGDB_REAL;
        return byte_array(env, number, length);
    default:
        buffer = malloc(value->len + 1);
        if (buffer == NULL) {
            return NULL;
        }
        buffer[0] = (unsigned char)value->kind;
        if (value->len > 0) {
            memcpy(buffer + 1, value->data, value->len);
        }
        array = byte_array(env, buffer, value->len + 1);
        free(buffer);
        return array;
    }
}

static int put(JNIEnv *env, jobjectArray out, jsize *at, jbyteArray element) {
    (*env)->SetObjectArrayElement(env, out, (*at)++, element);
    if (element != NULL) {
        (*env)->DeleteLocalRef(env, element);
    }
    return (*env)->ExceptionCheck(env) ? -1 : 0;
}

static jbyteArray decimal(JNIEnv *env, size_t number) {
    char text[32];
    int length = snprintf(text, sizeof text, "%zu", number);
    return byte_array(env, text, (size_t)length);
}

JNIEXPORT jlong JNICALL Java_app_eggspot_eggdb_EggDB_nativeOpen(JNIEnv *env, jclass type, jbyteArray path, jbyteArray dialect) {
    (void)type;
    char *path_text = terminated(env, path);
    char *dialect_text = terminated(env, dialect);
    eggdb_jni_handle *handle = calloc(1, sizeof *handle);
    jlong opened = 0;
    if (path_text == NULL || dialect_text == NULL || handle == NULL) {
        jclass failure = (*env)->FindClass(env, "app/eggspot/eggdb/EggDBException");
        if (failure != NULL) {
            (*env)->ThrowNew(env, failure, "the path or dialect holds a NUL character, or memory ran out");
        }
    } else {
        eggdb_options options;
        memset(&options, 0, sizeof options);
        if (eggdb_open(path_text, &options, &handle->db) != EGGDB_OK) {
            throw_failure(env, "EggDB could not open the database");
        } else if (eggdb_sql_open(handle->db, dialect_text, &handle->sql) != EGGDB_OK) {
            throw_failure(env, "EggDB could not start a SQL session");
            eggdb_close(handle->db);
        } else {
            opened = (jlong)(intptr_t)handle;
            handle = NULL;
        }
    }
    free(handle);
    free(path_text);
    free(dialect_text);
    return opened;
}

JNIEXPORT jobjectArray JNICALL Java_app_eggspot_eggdb_EggDB_nativeExecute(JNIEnv *env, jclass type, jlong opened, jbyteArray sql) {
    (void)type;
    eggdb_jni_handle *handle = (eggdb_jni_handle *)(intptr_t)opened;
    char *text = terminated(env, sql);
    const eggdb_result *results = NULL;
    size_t count = 0;
    size_t total = 0;
    jobjectArray out = NULL;
    jsize at = 0;
    if (handle == NULL || text == NULL) {
        jclass failure = (*env)->FindClass(env, "app/eggspot/eggdb/EggDBException");
        if (failure != NULL) {
            (*env)->ThrowNew(env, failure, handle == NULL ? "the database is closed" : "the SQL holds a NUL character");
        }
        free(text);
        return NULL;
    }
    if (eggdb_sql_execute(handle->sql, text, &results, &count) != EGGDB_OK) {
        free(text);
        throw_failure(env, "the statement failed");
        return NULL;
    }
    free(text);
    for (size_t index = 0; index < count; index++) {
        total += 3 + results[index].column_count + results[index].column_count * results[index].row_count;
    }
    out = (*env)->NewObjectArray(env, (jsize)total, (*env)->FindClass(env, "[B"), NULL);
    if (out == NULL) {
        return NULL;
    }
    for (size_t index = 0; index < count; index++) {
        const eggdb_result *result = &results[index];
        if (put(env, out, &at, byte_array(env, result->tag, strlen(result->tag))) != 0 ||
            put(env, out, &at, decimal(env, result->column_count)) != 0 ||
            put(env, out, &at, decimal(env, result->row_count)) != 0) {
            return NULL;
        }
        for (size_t column = 0; column < result->column_count; column++) {
            const char *name = result->column_names[column];
            if (put(env, out, &at, byte_array(env, name, strlen(name))) != 0) {
                return NULL;
            }
        }
        for (size_t row = 0; row < result->row_count; row++) {
            for (size_t column = 0; column < result->column_count; column++) {
                if (put(env, out, &at, cell(env, eggdb_result_value(result, row, column))) != 0) {
                    return NULL;
                }
            }
        }
    }
    return out;
}

JNIEXPORT void JNICALL Java_app_eggspot_eggdb_EggDB_nativeClose(JNIEnv *env, jclass type, jlong opened) {
    (void)env;
    (void)type;
    eggdb_jni_handle *handle = (eggdb_jni_handle *)(intptr_t)opened;
    if (handle == NULL) {
        return;
    }
    eggdb_sql_close(handle->sql);
    eggdb_close(handle->db);
    free(handle);
}
