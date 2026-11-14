#include <jni.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "eggdb.h"

static void throw_status(JNIEnv *env, int status, const char *fallback) {
    const char *message = eggdb_last_error();
    const char *state = eggdb_last_sqlstate();
    jclass failure = (*env)->FindClass(env, "app/eggspot/eggdb/EggDbNativeException");
    if (failure == NULL) {
        return;
    }
    jmethodID made = (*env)->GetMethodID(env, failure, "<init>", "(Ljava/lang/String;Ljava/lang/String;I)V");
    if (made == NULL) {
        return;
    }
    jstring text = (*env)->NewStringUTF(env, message != NULL && message[0] != '\0' ? message : fallback);
    jstring code = (*env)->NewStringUTF(env, state != NULL ? state : "");
    jobject thrown = (*env)->NewObject(env, failure, made, text, code, (jint)status);
    if (thrown != NULL) {
        (*env)->Throw(env, (jthrowable)thrown);
    }
}

static void throw_plain(JNIEnv *env, const char *message) {
    jclass failure = (*env)->FindClass(env, "app/eggspot/eggdb/EggDbNativeException");
    if (failure == NULL) {
        return;
    }
    jmethodID made = (*env)->GetMethodID(env, failure, "<init>", "(Ljava/lang/String;Ljava/lang/String;I)V");
    if (made == NULL) {
        return;
    }
    jstring text = (*env)->NewStringUTF(env, message);
    jstring code = (*env)->NewStringUTF(env, "HY000");
    jobject thrown = (*env)->NewObject(env, failure, made, text, code, (jint)EGGDB_ERR_INVALID_ARGUMENT);
    if (thrown != NULL) {
        (*env)->Throw(env, (jthrowable)thrown);
    }
}

static char *terminated(JNIEnv *env, jbyteArray bytes) {
    jsize length = (*env)->GetArrayLength(env, bytes);
    char *text = malloc((size_t)length + 1);
    if (text == NULL) {
        throw_plain(env, "out of memory");
        return NULL;
    }
    (*env)->GetByteArrayRegion(env, bytes, 0, length, (jbyte *)text);
    text[length] = '\0';
    if (memchr(text, '\0', (size_t)length) != NULL) {
        free(text);
        throw_plain(env, "the text holds a NUL character");
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

static jobjectArray byte_arrays(JNIEnv *env, jsize count) {
    jclass bytes = (*env)->FindClass(env, "[B");
    return bytes == NULL ? NULL : (*env)->NewObjectArray(env, count, bytes, NULL);
}

static jobjectArray flatten(JNIEnv *env, const eggdb_result *results, size_t count) {
    size_t total = 0;
    jsize at = 0;
    for (size_t index = 0; index < count; index++) {
        total += 3 + results[index].column_count + results[index].column_count * results[index].row_count;
    }
    jobjectArray out = byte_arrays(env, (jsize)total);
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

#define HANDLE(type, value) ((type *)(intptr_t)(value))

JNIEXPORT jlong JNICALL Java_app_eggspot_eggdb_JniCalls_nativeOpen(JNIEnv *env, jclass type, jbyteArray path, jboolean read_only) {
    (void)type;
    char *text = terminated(env, path);
    if (text == NULL) {
        return 0;
    }
    eggdb_options options;
    memset(&options, 0, sizeof options);
    options.flags = read_only ? EGGDB_OPEN_READ_ONLY : 0;
    eggdb_db *db = NULL;
    int status = eggdb_open(text, &options, &db);
    free(text);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the database could not be opened");
        return 0;
    }
    return (jlong)(intptr_t)db;
}

JNIEXPORT jlong JNICALL Java_app_eggspot_eggdb_JniCalls_nativeSqlOpen(JNIEnv *env, jclass type, jlong database, jbyteArray dialect) {
    (void)type;
    char *text = terminated(env, dialect);
    if (text == NULL) {
        return 0;
    }
    eggdb_sql *sql = NULL;
    int status = eggdb_sql_open(HANDLE(eggdb_db, database), text, &sql);
    free(text);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the SQL session could not be opened");
        return 0;
    }
    return (jlong)(intptr_t)sql;
}

JNIEXPORT jlong JNICALL Java_app_eggspot_eggdb_JniCalls_nativeConnect(JNIEnv *env, jclass type, jbyteArray url) {
    (void)type;
    char *text = terminated(env, url);
    if (text == NULL) {
        return 0;
    }
    eggdb_sql *sql = NULL;
    int status = eggdb_sql_connect(text, &sql);
    free(text);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the server could not be reached");
        return 0;
    }
    return (jlong)(intptr_t)sql;
}

JNIEXPORT void JNICALL Java_app_eggspot_eggdb_JniCalls_nativeSqlClose(JNIEnv *env, jclass type, jlong session) {
    (void)env;
    (void)type;
    eggdb_sql_close(HANDLE(eggdb_sql, session));
}

JNIEXPORT void JNICALL Java_app_eggspot_eggdb_JniCalls_nativeClose(JNIEnv *env, jclass type, jlong database) {
    (void)env;
    (void)type;
    eggdb_close(HANDLE(eggdb_db, database));
}

JNIEXPORT jobjectArray JNICALL Java_app_eggspot_eggdb_JniCalls_nativeExecute(JNIEnv *env, jclass type, jlong session, jbyteArray sql, jobjectArray parameters) {
    (void)type;
    char *text = terminated(env, sql);
    if (text == NULL) {
        return NULL;
    }
    jsize count = (*env)->GetArrayLength(env, parameters);
    eggdb_parameter *bound = calloc(count > 0 ? (size_t)count : 1, sizeof(eggdb_parameter));
    jbyteArray *arrays = calloc(count > 0 ? (size_t)count : 1, sizeof(jbyteArray));
    jbyte **data = calloc(count > 0 ? (size_t)count : 1, sizeof(jbyte *));
    if (bound == NULL || arrays == NULL || data == NULL) {
        free(text);
        free(bound);
        free(arrays);
        free(data);
        throw_plain(env, "out of memory");
        return NULL;
    }
    for (jsize index = 0; index < count; index++) {
        arrays[index] = (jbyteArray)(*env)->GetObjectArrayElement(env, parameters, index);
        if (arrays[index] != NULL) {
            data[index] = (*env)->GetByteArrayElements(env, arrays[index], NULL);
            bound[index].text = (const uint8_t *)data[index];
            bound[index].len = (size_t)(*env)->GetArrayLength(env, arrays[index]);
        }
    }
    const eggdb_result *results = NULL;
    size_t result_count = 0;
    int status = eggdb_sql_execute_with_parameters(HANDLE(eggdb_sql, session), text, bound, (size_t)count, &results, &result_count);
    for (jsize index = 0; index < count; index++) {
        if (arrays[index] != NULL) {
            (*env)->ReleaseByteArrayElements(env, arrays[index], data[index], JNI_ABORT);
            (*env)->DeleteLocalRef(env, arrays[index]);
        }
    }
    free(text);
    free(bound);
    free(arrays);
    free(data);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the statement failed");
        return NULL;
    }
    return flatten(env, results, result_count);
}

JNIEXPORT jlong JNICALL Java_app_eggspot_eggdb_JniCalls_nativePrepare(JNIEnv *env, jclass type, jlong session, jbyteArray sql) {
    (void)type;
    jsize length = (*env)->GetArrayLength(env, sql);
    jbyte *text = (*env)->GetByteArrayElements(env, sql, NULL);
    if (text == NULL) {
        return 0;
    }
    eggdb_stmt *stmt = NULL;
    int status = eggdb_sql_prepare(HANDLE(eggdb_sql, session), (const char *)text, (size_t)length, &stmt);
    (*env)->ReleaseByteArrayElements(env, sql, text, JNI_ABORT);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the statement could not be prepared");
        return 0;
    }
    return (jlong)(intptr_t)stmt;
}

JNIEXPORT jint JNICALL Java_app_eggspot_eggdb_JniCalls_nativeParameterCount(JNIEnv *env, jclass type, jlong statement) {
    (void)type;
    size_t count = 0;
    int status = eggdb_stmt_parameter_count(HANDLE(eggdb_stmt, statement), &count);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the parameters could not be counted");
        return 0;
    }
    return (jint)count;
}

enum { BINDING_BOOLEAN = 5 };

static int bind_one(JNIEnv *env, eggdb_stmt *stmt, size_t index, jint kind, jlong integer, jdouble real, jbyteArray bytes) {
    jsize length;
    jbyte *data;
    int status;
    switch (kind) {
    case EGGDB_NULL:
        return eggdb_stmt_bind_null(stmt, index);
    case BINDING_BOOLEAN:
        return eggdb_stmt_bind_bool(stmt, index, integer != 0);
    case EGGDB_INTEGER:
        return eggdb_stmt_bind_int64(stmt, index, (int64_t)integer);
    case EGGDB_REAL:
        return eggdb_stmt_bind_double(stmt, index, (double)real);
    default:
        if (bytes == NULL) {
            return eggdb_stmt_bind_null(stmt, index);
        }
        length = (*env)->GetArrayLength(env, bytes);
        data = (*env)->GetByteArrayElements(env, bytes, NULL);
        if (data == NULL) {
            return EGGDB_ERR_OUT_OF_MEMORY;
        }
        status = kind == EGGDB_TEXT
            ? eggdb_stmt_bind_text(stmt, index, (const char *)data, (size_t)length)
            : eggdb_stmt_bind_blob(stmt, index, (const uint8_t *)data, (size_t)length);
        (*env)->ReleaseByteArrayElements(env, bytes, data, JNI_ABORT);
        return status;
    }
}

JNIEXPORT void JNICALL Java_app_eggspot_eggdb_JniCalls_nativeBind(JNIEnv *env, jclass type, jlong statement, jintArray kinds, jlongArray integers, jdoubleArray reals, jobjectArray bytes) {
    (void)type;
    eggdb_stmt *stmt = HANDLE(eggdb_stmt, statement);
    jsize width = (*env)->GetArrayLength(env, kinds);
    jint *kind = (*env)->GetIntArrayElements(env, kinds, NULL);
    jlong *integer = (*env)->GetLongArrayElements(env, integers, NULL);
    jdouble *real = (*env)->GetDoubleArrayElements(env, reals, NULL);
    int status = kind == NULL || integer == NULL || real == NULL ? EGGDB_ERR_OUT_OF_MEMORY : eggdb_stmt_reset(stmt);
    if (status == EGGDB_OK) {
        status = eggdb_stmt_clear_bindings(stmt);
    }
    for (jsize column = 0; column < width && status == EGGDB_OK; column++) {
        jbyteArray value = (jbyteArray)(*env)->GetObjectArrayElement(env, bytes, column);
        status = bind_one(env, stmt, (size_t)column + 1, kind[column], integer[column], real[column], value);
        if (value != NULL) {
            (*env)->DeleteLocalRef(env, value);
        }
    }
    if (kind != NULL) (*env)->ReleaseIntArrayElements(env, kinds, kind, JNI_ABORT);
    if (integer != NULL) (*env)->ReleaseLongArrayElements(env, integers, integer, JNI_ABORT);
    if (real != NULL) (*env)->ReleaseDoubleArrayElements(env, reals, real, JNI_ABORT);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the parameters could not be bound");
    }
}

JNIEXPORT jobjectArray JNICALL Java_app_eggspot_eggdb_JniCalls_nativeRun(JNIEnv *env, jclass type, jlong statement) {
    (void)type;
    const eggdb_result *results = NULL;
    size_t count = 0;
    int status = eggdb_stmt_execute(HANDLE(eggdb_stmt, statement), &results, &count);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the statement failed");
        return NULL;
    }
    return flatten(env, results, count);
}

JNIEXPORT jobjectArray JNICALL Java_app_eggspot_eggdb_JniCalls_nativeColumns(JNIEnv *env, jclass type, jlong statement) {
    (void)type;
    eggdb_stmt *stmt = HANDLE(eggdb_stmt, statement);
    size_t count = 0;
    int status = eggdb_stmt_column_count(stmt, &count);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the columns could not be read");
        return NULL;
    }
    jobjectArray out = byte_arrays(env, (jsize)count);
    jsize at = 0;
    for (size_t column = 0; out != NULL && column < count; column++) {
        const char *name = NULL;
        status = eggdb_stmt_column_name(stmt, column, &name);
        if (status != EGGDB_OK) {
            throw_status(env, status, "the columns could not be read");
            return NULL;
        }
        if (put(env, out, &at, byte_array(env, name, strlen(name))) != 0) {
            return NULL;
        }
    }
    return out;
}

JNIEXPORT jobjectArray JNICALL Java_app_eggspot_eggdb_JniCalls_nativeStep(JNIEnv *env, jclass type, jlong statement) {
    (void)type;
    eggdb_stmt *stmt = HANDLE(eggdb_stmt, statement);
    int has_row = 0;
    int status = eggdb_stmt_step(stmt, &has_row);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the statement failed");
        return NULL;
    }
    if (!has_row) {
        return NULL;
    }
    const eggdb_value *values = NULL;
    size_t count = 0;
    status = eggdb_stmt_row(stmt, &values, &count);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the row could not be read");
        return NULL;
    }
    jobjectArray out = byte_arrays(env, (jsize)count);
    jsize at = 0;
    for (size_t column = 0; out != NULL && column < count; column++) {
        if (put(env, out, &at, cell(env, &values[column])) != 0) {
            return NULL;
        }
    }
    return out;
}

JNIEXPORT void JNICALL Java_app_eggspot_eggdb_JniCalls_nativeReset(JNIEnv *env, jclass type, jlong statement) {
    (void)env;
    (void)type;
    eggdb_stmt_reset(HANDLE(eggdb_stmt, statement));
}

JNIEXPORT void JNICALL Java_app_eggspot_eggdb_JniCalls_nativeFinalize(JNIEnv *env, jclass type, jlong statement) {
    (void)env;
    (void)type;
    eggdb_stmt_finalize(HANDLE(eggdb_stmt, statement));
}

static jlong begin_copy(JNIEnv *env, jlong session, jbyteArray sql, int out_of_session) {
    char *text = terminated(env, sql);
    if (text == NULL) {
        return 0;
    }
    eggdb_copy *copy = NULL;
    int status = out_of_session
        ? eggdb_sql_copy_out_begin(HANDLE(eggdb_sql, session), text, &copy)
        : eggdb_sql_copy_in_begin(HANDLE(eggdb_sql, session), text, &copy);
    free(text);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the COPY could not start");
        return 0;
    }
    return (jlong)(intptr_t)copy;
}

JNIEXPORT jlong JNICALL Java_app_eggspot_eggdb_JniCalls_nativeCopyInBegin(JNIEnv *env, jclass type, jlong session, jbyteArray sql) {
    (void)type;
    return begin_copy(env, session, sql, 0);
}

JNIEXPORT jlong JNICALL Java_app_eggspot_eggdb_JniCalls_nativeCopyOutBegin(JNIEnv *env, jclass type, jlong session, jbyteArray sql) {
    (void)type;
    return begin_copy(env, session, sql, 1);
}

JNIEXPORT void JNICALL Java_app_eggspot_eggdb_JniCalls_nativeCopyPut(JNIEnv *env, jclass type, jlong copy, jbyteArray data, jint length) {
    (void)type;
    jbyte *bytes = (*env)->GetByteArrayElements(env, data, NULL);
    if (bytes == NULL) {
        return;
    }
    int status = eggdb_copy_put(HANDLE(eggdb_copy, copy), (const uint8_t *)bytes, (size_t)length);
    (*env)->ReleaseByteArrayElements(env, data, bytes, JNI_ABORT);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the COPY data could not be sent");
    }
}

JNIEXPORT jbyteArray JNICALL Java_app_eggspot_eggdb_JniCalls_nativeCopyRead(JNIEnv *env, jclass type, jlong copy) {
    (void)type;
    const uint8_t *data = NULL;
    size_t length = 0;
    int status = eggdb_copy_read(HANDLE(eggdb_copy, copy), &data, &length);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the COPY data could not be read");
        return NULL;
    }
    return length == 0 ? NULL : byte_array(env, data, length);
}

JNIEXPORT jobjectArray JNICALL Java_app_eggspot_eggdb_JniCalls_nativeCopyEnd(JNIEnv *env, jclass type, jlong copy) {
    (void)type;
    const eggdb_result *results = NULL;
    size_t count = 0;
    int status = eggdb_copy_end(HANDLE(eggdb_copy, copy), &results, &count);
    if (status != EGGDB_OK) {
        throw_status(env, status, "the COPY failed");
        return NULL;
    }
    return flatten(env, results, count);
}

JNIEXPORT void JNICALL Java_app_eggspot_eggdb_JniCalls_nativeCopyFree(JNIEnv *env, jclass type, jlong copy) {
    (void)env;
    (void)type;
    eggdb_copy_free(HANDLE(eggdb_copy, copy));
}
