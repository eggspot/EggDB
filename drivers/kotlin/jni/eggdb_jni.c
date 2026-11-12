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

static void throw_message(JNIEnv *env, const char *message) {
    jclass failure = (*env)->FindClass(env, "app/eggspot/eggdb/EggDBException");
    if (failure != NULL) {
        (*env)->ThrowNew(env, failure, message);
    }
}

static int check_status(JNIEnv *env, int status, const char *fallback) {
    if (status == EGGDB_OK) {
        return 0;
    }
    throw_failure(env, fallback);
    return -1;
}

typedef struct eggdb_jni_bytes {
    uint8_t *data;
    size_t len;
    int missing;
} eggdb_jni_bytes;

static int copied_bytes(JNIEnv *env, jbyteArray array, eggdb_jni_bytes *out) {
    out->data = NULL;
    out->len = 0;
    out->missing = array == NULL;
    if (array == NULL) {
        return 0;
    }
    jsize length = (*env)->GetArrayLength(env, array);
    out->data = malloc((size_t)length + 1);
    if (out->data == NULL) {
        throw_message(env, "memory ran out");
        return -1;
    }
    (*env)->GetByteArrayRegion(env, array, 0, length, (jbyte *)out->data);
    out->len = (size_t)length;
    return 0;
}

static int optional_name(JNIEnv *env, jbyteArray name, char **out) {
    *out = NULL;
    if (name == NULL) {
        return 0;
    }
    *out = terminated(env, name);
    if (*out == NULL) {
        throw_message(env, "the collection name holds a NUL character, or memory ran out");
        return -1;
    }
    return 0;
}

static eggdb_txn *txn_of(jlong handle) {
    return (eggdb_txn *)(intptr_t)handle;
}

JNIEXPORT jlong JNICALL Java_app_eggspot_eggdb_KeyValue_nativeOpen(JNIEnv *env, jclass type, jbyteArray path, jboolean read_only) {
    (void)type;
    char *path_text = terminated(env, path);
    if (path_text == NULL) {
        throw_message(env, "the path holds a NUL character, or memory ran out");
        return 0;
    }
    eggdb_options options;
    memset(&options, 0, sizeof options);
    if (read_only) {
        options.flags = EGGDB_OPEN_READ_ONLY;
    }
    eggdb_db *db = NULL;
    int status = eggdb_open(path_text, &options, &db);
    free(path_text);
    if (check_status(env, status, "EggDB could not open the database") != 0) {
        return 0;
    }
    return (jlong)(intptr_t)db;
}

JNIEXPORT void JNICALL Java_app_eggspot_eggdb_KeyValue_nativeClose(JNIEnv *env, jclass type, jlong db) {
    (void)type;
    check_status(env, eggdb_close((eggdb_db *)(intptr_t)db), "EggDB could not close the database");
}

JNIEXPORT jlong JNICALL Java_app_eggspot_eggdb_KeyValue_nativeBegin(JNIEnv *env, jclass type, jlong db, jboolean writing) {
    (void)type;
    eggdb_txn *txn = NULL;
    int status = writing ? eggdb_begin_write((eggdb_db *)(intptr_t)db, &txn) : eggdb_begin_read((eggdb_db *)(intptr_t)db, &txn);
    if (check_status(env, status, "EggDB could not begin a transaction") != 0) {
        return 0;
    }
    return (jlong)(intptr_t)txn;
}

JNIEXPORT jlong JNICALL Java_app_eggspot_eggdb_Transaction_nativeCommit(JNIEnv *env, jclass type, jlong txn) {
    (void)type;
    uint64_t stamp = 0;
    if (check_status(env, eggdb_commit(txn_of(txn), &stamp), "the commit failed") != 0) {
        return 0;
    }
    return (jlong)stamp;
}

JNIEXPORT void JNICALL Java_app_eggspot_eggdb_Transaction_nativeRollback(JNIEnv *env, jclass type, jlong txn) {
    (void)type;
    check_status(env, eggdb_rollback(txn_of(txn)), "the rollback failed");
}

JNIEXPORT jbyteArray JNICALL Java_app_eggspot_eggdb_Transaction_nativeGet(JNIEnv *env, jclass type, jlong txn, jbyteArray collection, jbyteArray key) {
    (void)type;
    char *name = NULL;
    eggdb_jni_bytes key_bytes;
    jbyteArray found = NULL;
    if (optional_name(env, collection, &name) != 0) {
        return NULL;
    }
    if (copied_bytes(env, key, &key_bytes) == 0) {
        uint8_t *value = NULL;
        size_t value_len = 0;
        if (check_status(env, eggdb_get(txn_of(txn), name, key_bytes.data, key_bytes.len, &value, &value_len), "the read failed") == 0 && value != NULL) {
            found = byte_array(env, value, value_len);
            eggdb_free(value, value_len);
        }
        free(key_bytes.data);
    }
    free(name);
    return found;
}

JNIEXPORT void JNICALL Java_app_eggspot_eggdb_Transaction_nativePut(JNIEnv *env, jclass type, jlong txn, jbyteArray collection, jbyteArray key, jbyteArray value, jlong ttl_ms) {
    (void)type;
    char *name = NULL;
    eggdb_jni_bytes key_bytes;
    eggdb_jni_bytes value_bytes;
    if (optional_name(env, collection, &name) != 0) {
        return;
    }
    if (copied_bytes(env, key, &key_bytes) == 0) {
        if (copied_bytes(env, value, &value_bytes) == 0) {
            int status = ttl_ms > 0
                ? eggdb_put_with_expiry(txn_of(txn), name, key_bytes.data, key_bytes.len, value_bytes.data, value_bytes.len, (uint64_t)ttl_ms)
                : eggdb_put(txn_of(txn), name, key_bytes.data, key_bytes.len, value_bytes.data, value_bytes.len);
            check_status(env, status, "the write failed");
            free(value_bytes.data);
        }
        free(key_bytes.data);
    }
    free(name);
}

JNIEXPORT jboolean JNICALL Java_app_eggspot_eggdb_Transaction_nativeDelete(JNIEnv *env, jclass type, jlong txn, jbyteArray collection, jbyteArray key) {
    (void)type;
    char *name = NULL;
    eggdb_jni_bytes key_bytes;
    int deleted = 0;
    if (optional_name(env, collection, &name) != 0) {
        return JNI_FALSE;
    }
    if (copied_bytes(env, key, &key_bytes) == 0) {
        check_status(env, eggdb_delete(txn_of(txn), name, key_bytes.data, key_bytes.len, &deleted), "the delete failed");
        free(key_bytes.data);
    }
    free(name);
    return deleted ? JNI_TRUE : JNI_FALSE;
}

static jobjectArray scanned(JNIEnv *env, eggdb_txn *txn, eggdb_scan *scan, jint limit) {
    size_t capacity = 64;
    size_t used = 0;
    jobjectArray out = NULL;
    jsize at = 0;
    uint8_t **copies = malloc(capacity * sizeof *copies);
    size_t *lengths = malloc(capacity * sizeof *lengths);
    if (copies == NULL || lengths == NULL) {
        free(copies);
        free(lengths);
        throw_message(env, "memory ran out");
        return NULL;
    }
    for (;;) {
        const eggdb_entry *entries = NULL;
        size_t count = 0;
        if (check_status(env, eggdb_scan_next(txn, scan, 0, &entries, &count), "the scan failed") != 0) {
            goto failed;
        }
        if (count == 0) {
            break;
        }
        for (size_t index = 0; index < count && (limit <= 0 || used / 2 < (size_t)limit); index++) {
            if (used + 2 > capacity) {
                capacity *= 2;
                uint8_t **grown_copies = realloc(copies, capacity * sizeof *copies);
                if (grown_copies == NULL) {
                    goto out_of_memory;
                }
                copies = grown_copies;
                size_t *grown_lengths = realloc(lengths, capacity * sizeof *lengths);
                if (grown_lengths == NULL) {
                    goto out_of_memory;
                }
                lengths = grown_lengths;
            }
            const eggdb_entry *entry = &entries[index];
            copies[used] = malloc(entry->key_len + 1);
            copies[used + 1] = malloc(entry->value_len + 1);
            if (copies[used] == NULL || copies[used + 1] == NULL) {
                free(copies[used]);
                free(copies[used + 1]);
                goto out_of_memory;
            }
            memcpy(copies[used], entry->key, entry->key_len);
            memcpy(copies[used + 1], entry->value, entry->value_len);
            lengths[used] = entry->key_len;
            lengths[used + 1] = entry->value_len;
            used += 2;
        }
        if (limit > 0 && used / 2 >= (size_t)limit) {
            break;
        }
    }
    out = (*env)->NewObjectArray(env, (jsize)used, (*env)->FindClass(env, "[B"), NULL);
    for (size_t index = 0; out != NULL && index < used; index++) {
        if (put(env, out, &at, byte_array(env, copies[index], lengths[index])) != 0) {
            out = NULL;
        }
    }
    goto done;
out_of_memory:
    throw_message(env, "memory ran out");
failed:
    out = NULL;
done:
    for (size_t index = 0; index < used; index++) {
        free(copies[index]);
    }
    free(copies);
    free(lengths);
    return out;
}

JNIEXPORT jobjectArray JNICALL Java_app_eggspot_eggdb_Transaction_nativeScan(JNIEnv *env, jclass type, jlong txn, jbyteArray collection, jbyteArray start, jbyteArray end, jint flags, jint limit) {
    (void)type;
    char *name = NULL;
    eggdb_jni_bytes start_bytes;
    eggdb_jni_bytes end_bytes;
    jobjectArray out = NULL;
    if (optional_name(env, collection, &name) != 0) {
        return NULL;
    }
    if (copied_bytes(env, start, &start_bytes) == 0) {
        if (copied_bytes(env, end, &end_bytes) == 0) {
            eggdb_scan *scan = NULL;
            int status = eggdb_scan_open(txn_of(txn), name, start_bytes.missing ? NULL : start_bytes.data, start_bytes.len,
                end_bytes.missing ? NULL : end_bytes.data, end_bytes.len, (uint32_t)flags, &scan);
            if (check_status(env, status, "the scan failed") == 0) {
                out = scanned(env, txn_of(txn), scan, limit);
                eggdb_scan_free(scan);
            }
            free(end_bytes.data);
        }
        free(start_bytes.data);
    }
    free(name);
    return out;
}

static jboolean collection_change(JNIEnv *env, jlong txn, jbyteArray collection, int creating) {
    char *name = terminated(env, collection);
    int changed = 0;
    if (name == NULL) {
        throw_message(env, "the collection name holds a NUL character, or memory ran out");
        return JNI_FALSE;
    }
    int status = creating ? eggdb_create_collection(txn_of(txn), name, &changed) : eggdb_drop_collection(txn_of(txn), name, &changed);
    free(name);
    check_status(env, status, "the collection could not be changed");
    return changed ? JNI_TRUE : JNI_FALSE;
}

JNIEXPORT jboolean JNICALL Java_app_eggspot_eggdb_Transaction_nativeCreateCollection(JNIEnv *env, jclass type, jlong txn, jbyteArray name) {
    (void)type;
    return collection_change(env, txn, name, 1);
}

JNIEXPORT jboolean JNICALL Java_app_eggspot_eggdb_Transaction_nativeDropCollection(JNIEnv *env, jclass type, jlong txn, jbyteArray name) {
    (void)type;
    return collection_change(env, txn, name, 0);
}

JNIEXPORT void JNICALL Java_app_eggspot_eggdb_Transaction_nativeRenameCollection(JNIEnv *env, jclass type, jlong txn, jbyteArray name, jbyteArray new_name) {
    (void)type;
    char *old_text = terminated(env, name);
    char *new_text = terminated(env, new_name);
    if (old_text == NULL || new_text == NULL) {
        throw_message(env, "a collection name holds a NUL character, or memory ran out");
    } else {
        check_status(env, eggdb_rename_collection(txn_of(txn), old_text, new_text), "the collection could not be renamed");
    }
    free(old_text);
    free(new_text);
}

JNIEXPORT jobjectArray JNICALL Java_app_eggspot_eggdb_Transaction_nativeCollections(JNIEnv *env, jclass type, jlong txn) {
    (void)type;
    uint8_t *names = NULL;
    size_t names_len = 0;
    size_t count = 0;
    jobjectArray out = NULL;
    jsize at = 0;
    if (check_status(env, eggdb_collection_names(txn_of(txn), &names, &names_len, &count), "the collections could not be listed") != 0) {
        return NULL;
    }
    out = (*env)->NewObjectArray(env, (jsize)count, (*env)->FindClass(env, "[B"), NULL);
    size_t offset = 0;
    for (size_t index = 0; out != NULL && index < count && offset <= names_len; index++) {
        const uint8_t *found = names_len > offset ? memchr(names + offset, '\0', names_len - offset) : NULL;
        size_t length = found != NULL ? (size_t)(found - (names + offset)) : names_len - offset;
        if (put(env, out, &at, byte_array(env, names + offset, length)) != 0) {
            out = NULL;
        }
        offset += length + 1;
    }
    eggdb_free(names, names_len);
    return out;
}

JNIEXPORT jlongArray JNICALL Java_app_eggspot_eggdb_Transaction_nativeStats(JNIEnv *env, jclass type, jlong txn, jbyteArray name) {
    (void)type;
    char *text = terminated(env, name);
    eggdb_collection_stats stats;
    jlongArray out = NULL;
    if (text == NULL) {
        throw_message(env, "the collection name holds a NUL character, or memory ran out");
        return NULL;
    }
    memset(&stats, 0, sizeof stats);
    int status = eggdb_collection_stats(txn_of(txn), text, &stats);
    free(text);
    if (check_status(env, status, "the collection could not be counted") != 0) {
        return NULL;
    }
    jlong values[3] = {(jlong)stats.entries, (jlong)stats.key_bytes, (jlong)stats.value_bytes};
    out = (*env)->NewLongArray(env, 3);
    if (out != NULL) {
        (*env)->SetLongArrayRegion(env, out, 0, 3, values);
    }
    return out;
}
