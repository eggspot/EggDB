#define PY_SSIZE_T_CLEAN
#include <Python.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
#else
#include <dlfcn.h>
#endif

#include "eggdb.h"

typedef struct calls {
    const char *(*last_error)(void);
    const char *(*last_sqlstate)(void);
    int (*execute)(eggdb_sql *, const char *, const eggdb_parameter *, size_t, const eggdb_result **, size_t *);
    int (*prepare)(eggdb_sql *, const char *, size_t, eggdb_stmt **);
    int (*parameter_count)(eggdb_stmt *, size_t *);
    int (*bind_null)(eggdb_stmt *, size_t);
    int (*bind_int64)(eggdb_stmt *, size_t, int64_t);
    int (*bind_double)(eggdb_stmt *, size_t, double);
    int (*bind_bool)(eggdb_stmt *, size_t, int);
    int (*bind_text)(eggdb_stmt *, size_t, const char *, size_t);
    int (*bind_blob)(eggdb_stmt *, size_t, const uint8_t *, size_t);
    int (*clear_bindings)(eggdb_stmt *);
    int (*stmt_execute)(eggdb_stmt *, const eggdb_result **, size_t *);
    int (*reset)(eggdb_stmt *);
    int (*column_count)(eggdb_stmt *, size_t *);
    int (*column_name)(eggdb_stmt *, size_t, const char **);
    int (*step)(eggdb_stmt *, int *);
    int (*row)(eggdb_stmt *, const eggdb_value **, size_t *);
    int (*finalize)(eggdb_stmt *);
    int (*copy_in_begin)(eggdb_sql *, const char *, eggdb_copy **);
    int (*copy_out_begin)(eggdb_sql *, const char *, eggdb_copy **);
    int (*copy_put)(eggdb_copy *, const uint8_t *, size_t);
    int (*copy_read)(eggdb_copy *, const uint8_t **, size_t *);
    int (*copy_end)(eggdb_copy *, const eggdb_result **, size_t *);
    void (*copy_free)(eggdb_copy *);
} calls;

static calls api;
static int api_loaded = 0;
static PyObject *error_factory = NULL;

static void *symbol(void *library, const char *name) {
#ifdef _WIN32
    return (void *)GetProcAddress((HMODULE)library, name);
#else
    return dlsym(library, name);
#endif
}

#define LOAD(field, name)                                                                     \
    do {                                                                                      \
        void *found = symbol(library, name);                                                  \
        memcpy(&api.field, &found, sizeof found);                                             \
        if (found == NULL) {                                                                  \
            PyErr_Format(PyExc_ImportError, "the EggDB C library has no %s", name);           \
            return NULL;                                                                      \
        }                                                                                     \
    } while (0)

static PyObject *load(PyObject *module, PyObject *args) {
    (void)module;
    const char *path;
    PyObject *factory;
    if (!PyArg_ParseTuple(args, "sO", &path, &factory)) {
        return NULL;
    }
#ifdef _WIN32
    void *library = (void *)LoadLibraryA(path);
#else
    void *library = dlopen(path, RTLD_NOW | RTLD_GLOBAL);
#endif
    if (library == NULL) {
        PyErr_Format(PyExc_ImportError, "cannot open the EggDB C library at %s", path);
        return NULL;
    }
    LOAD(last_error, "eggdb_last_error");
    LOAD(last_sqlstate, "eggdb_last_sqlstate");
    LOAD(execute, "eggdb_sql_execute_with_parameters");
    LOAD(prepare, "eggdb_sql_prepare");
    LOAD(parameter_count, "eggdb_stmt_parameter_count");
    LOAD(bind_null, "eggdb_stmt_bind_null");
    LOAD(bind_int64, "eggdb_stmt_bind_int64");
    LOAD(bind_double, "eggdb_stmt_bind_double");
    LOAD(bind_bool, "eggdb_stmt_bind_bool");
    LOAD(bind_text, "eggdb_stmt_bind_text");
    LOAD(bind_blob, "eggdb_stmt_bind_blob");
    LOAD(clear_bindings, "eggdb_stmt_clear_bindings");
    LOAD(stmt_execute, "eggdb_stmt_execute");
    LOAD(reset, "eggdb_stmt_reset");
    LOAD(column_count, "eggdb_stmt_column_count");
    LOAD(column_name, "eggdb_stmt_column_name");
    LOAD(step, "eggdb_stmt_step");
    LOAD(row, "eggdb_stmt_row");
    LOAD(finalize, "eggdb_stmt_finalize");
    LOAD(copy_in_begin, "eggdb_sql_copy_in_begin");
    LOAD(copy_out_begin, "eggdb_sql_copy_out_begin");
    LOAD(copy_put, "eggdb_copy_put");
    LOAD(copy_read, "eggdb_copy_read");
    LOAD(copy_end, "eggdb_copy_end");
    LOAD(copy_free, "eggdb_copy_free");
    Py_INCREF(factory);
    Py_XDECREF(error_factory);
    error_factory = factory;
    api_loaded = 1;
    Py_RETURN_NONE;
}

static PyObject *raise_status(int status) {
    const char *message = api.last_error();
    const char *state = api.last_sqlstate();
    PyObject *raised = PyObject_CallFunction(error_factory, "iss", status, message ? message : "", state ? state : "");
    if (raised != NULL) {
        PyErr_SetObject((PyObject *)Py_TYPE(raised), raised);
        Py_DECREF(raised);
    }
    return NULL;
}

static int ready(void) {
    if (!api_loaded) {
        PyErr_SetString(PyExc_RuntimeError, "eggdb._native.load has not been called");
        return 0;
    }
    return 1;
}

static void *address_of(PyObject *value) {
    return PyLong_AsVoidPtr(value);
}

static PyObject *converted(const eggdb_value *value) {
    switch (value->kind) {
    case EGGDB_INTEGER:
        return PyLong_FromLongLong((long long)value->integer);
    case EGGDB_REAL:
        return PyFloat_FromDouble(value->real);
    case EGGDB_TEXT:
        return PyUnicode_DecodeUTF8((const char *)value->data, (Py_ssize_t)value->len, "replace");
    case EGGDB_BLOB:
        return PyBytes_FromStringAndSize((const char *)value->data, (Py_ssize_t)value->len);
    default:
        Py_RETURN_NONE;
    }
}

static PyObject *row_tuple(const eggdb_value *values, size_t count) {
    PyObject *row = PyTuple_New((Py_ssize_t)count);
    if (row == NULL) {
        return NULL;
    }
    for (size_t column = 0; column < count; column++) {
        PyObject *cell = converted(&values[column]);
        if (cell == NULL) {
            Py_DECREF(row);
            return NULL;
        }
        PyTuple_SET_ITEM(row, (Py_ssize_t)column, cell);
    }
    return row;
}

static PyObject *gathered(const eggdb_result *results, size_t count) {
    PyObject *list = PyList_New((Py_ssize_t)count);
    if (list == NULL) {
        return NULL;
    }
    for (size_t index = 0; index < count; index++) {
        const eggdb_result *result = &results[index];
        PyObject *names = PyList_New((Py_ssize_t)result->column_count);
        PyObject *rows = PyList_New((Py_ssize_t)result->row_count);
        if (names == NULL || rows == NULL) {
            Py_XDECREF(names);
            Py_XDECREF(rows);
            Py_DECREF(list);
            return NULL;
        }
        for (size_t column = 0; column < result->column_count; column++) {
            PyObject *name = PyUnicode_FromString(result->column_names[column]);
            if (name == NULL) {
                Py_DECREF(names);
                Py_DECREF(rows);
                Py_DECREF(list);
                return NULL;
            }
            PyList_SET_ITEM(names, (Py_ssize_t)column, name);
        }
        for (size_t row = 0; row < result->row_count; row++) {
            PyObject *cells = row_tuple(&result->values[row * result->column_count], result->column_count);
            if (cells == NULL) {
                Py_DECREF(names);
                Py_DECREF(rows);
                Py_DECREF(list);
                return NULL;
            }
            PyList_SET_ITEM(rows, (Py_ssize_t)row, cells);
        }
        PyObject *item = Py_BuildValue("(sNN)", result->tag ? result->tag : "", names, rows);
        if (item == NULL) {
            Py_DECREF(list);
            return NULL;
        }
        PyList_SET_ITEM(list, (Py_ssize_t)index, item);
    }
    return list;
}

static PyObject *execute(PyObject *module, PyObject *args) {
    (void)module;
    PyObject *session;
    const char *sql;
    PyObject *parameters;
    if (!ready() || !PyArg_ParseTuple(args, "OsO!", &session, &sql, &PyList_Type, &parameters)) {
        return NULL;
    }
    Py_ssize_t count = PyList_GET_SIZE(parameters);
    eggdb_parameter *bound = PyMem_Calloc(count > 0 ? (size_t)count : 1, sizeof(eggdb_parameter));
    if (bound == NULL) {
        return PyErr_NoMemory();
    }
    for (Py_ssize_t index = 0; index < count; index++) {
        PyObject *text = PyList_GET_ITEM(parameters, index);
        if (text == Py_None) {
            continue;
        }
        if (!PyBytes_Check(text)) {
            PyMem_Free(bound);
            PyErr_SetString(PyExc_TypeError, "parameters reach the native execute as bytes or None");
            return NULL;
        }
        bound[index].text = (const uint8_t *)PyBytes_AS_STRING(text);
        bound[index].len = (size_t)PyBytes_GET_SIZE(text);
    }
    const eggdb_result *results = NULL;
    size_t result_count = 0;
    eggdb_sql *handle = address_of(session);
    int status;
    Py_BEGIN_ALLOW_THREADS
    status = api.execute(handle, sql, bound, (size_t)count, &results, &result_count);
    Py_END_ALLOW_THREADS
    PyMem_Free(bound);
    if (status != EGGDB_OK) {
        return raise_status(status);
    }
    return gathered(results, result_count);
}

typedef struct {
    PyObject_HEAD
    eggdb_stmt *stmt;
    size_t parameter_count;
    int stepping;
} Statement;

static PyTypeObject StatementType;

static int bind_value(eggdb_stmt *stmt, size_t index, PyObject *value) {
    if (value == Py_None) {
        return api.bind_null(stmt, index);
    }
    if (PyBool_Check(value)) {
        return api.bind_bool(stmt, index, value == Py_True);
    }
    if (PyLong_Check(value)) {
        int overflow = 0;
        long long number = PyLong_AsLongLongAndOverflow(value, &overflow);
        if (overflow == 0 && !(number == -1 && PyErr_Occurred())) {
            return api.bind_int64(stmt, index, (int64_t)number);
        }
        PyErr_Clear();
    }
    if (PyFloat_Check(value)) {
        return api.bind_double(stmt, index, PyFloat_AS_DOUBLE(value));
    }
    if (PyObject_CheckBuffer(value) && !PyUnicode_Check(value)) {
        Py_buffer view;
        if (PyObject_GetBuffer(value, &view, PyBUF_SIMPLE) != 0) {
            return -1;
        }
        int status = api.bind_blob(stmt, index, view.buf, (size_t)view.len);
        PyBuffer_Release(&view);
        return status;
    }
    PyObject *text = PyUnicode_Check(value) ? (Py_INCREF(value), value) : PyObject_Str(value);
    if (text == NULL) {
        return -1;
    }
    Py_ssize_t length = 0;
    const char *utf8 = PyUnicode_AsUTF8AndSize(text, &length);
    int status = utf8 == NULL ? -1 : api.bind_text(stmt, index, utf8, (size_t)length);
    Py_DECREF(text);
    return status;
}

static int bind_row(Statement *self, PyObject *parameters) {
    int status = api.reset(self->stmt);
    self->stepping = 0;
    if (status == EGGDB_OK) {
        status = api.clear_bindings(self->stmt);
    }
    if (status != EGGDB_OK) {
        raise_status(status);
        return -1;
    }
    PyObject *sequence = PySequence_Fast(parameters, "parameters must be a sequence");
    if (sequence == NULL) {
        return -1;
    }
    Py_ssize_t count = PySequence_Fast_GET_SIZE(sequence);
    if ((size_t)count > self->parameter_count) {
        Py_DECREF(sequence);
        PyErr_Format(PyExc_ValueError, "%zd parameters given for %zu placeholders", count, self->parameter_count);
        return -1;
    }
    for (Py_ssize_t index = 0; index < count; index++) {
        status = bind_value(self->stmt, (size_t)index + 1, PySequence_Fast_GET_ITEM(sequence, index));
        if (status != EGGDB_OK) {
            Py_DECREF(sequence);
            if (status > 0) {
                raise_status(status);
            }
            return -1;
        }
    }
    Py_DECREF(sequence);
    return 0;
}

static PyObject *Statement_new(PyTypeObject *type, PyObject *args, PyObject *kwargs) {
    (void)kwargs;
    PyObject *session;
    const char *sql;
    Py_ssize_t length;
    if (!ready() || !PyArg_ParseTuple(args, "Os#", &session, &sql, &length)) {
        return NULL;
    }
    Statement *self = (Statement *)type->tp_alloc(type, 0);
    if (self == NULL) {
        return NULL;
    }
    int status = api.prepare(address_of(session), sql, (size_t)length, &self->stmt);
    if (status == EGGDB_OK) {
        status = api.parameter_count(self->stmt, &self->parameter_count);
    }
    if (status != EGGDB_OK) {
        Py_DECREF(self);
        return raise_status(status);
    }
    return (PyObject *)self;
}

static void Statement_dealloc(Statement *self) {
    if (self->stmt != NULL) {
        api.finalize(self->stmt);
    }
    Py_TYPE(self)->tp_free((PyObject *)self);
}

static int live(Statement *self) {
    if (self->stmt == NULL) {
        PyErr_SetString(PyExc_ValueError, "the statement is closed");
        return 0;
    }
    return 1;
}

static PyObject *Statement_execute(Statement *self, PyObject *args) {
    PyObject *parameters = NULL;
    if (!live(self) || !PyArg_ParseTuple(args, "|O", &parameters)) {
        return NULL;
    }
    PyObject *empty = NULL;
    if (parameters == NULL) {
        empty = PyTuple_New(0);
        parameters = empty;
    }
    int bound = bind_row(self, parameters);
    Py_XDECREF(empty);
    if (bound != 0) {
        return NULL;
    }
    const eggdb_result *results = NULL;
    size_t count = 0;
    int status;
    Py_BEGIN_ALLOW_THREADS
    status = api.stmt_execute(self->stmt, &results, &count);
    Py_END_ALLOW_THREADS
    if (status != EGGDB_OK) {
        return raise_status(status);
    }
    return gathered(results, count);
}

static long long changed_rows(const eggdb_result *results, size_t count) {
    long long total = 0;
    for (size_t index = 0; index < count; index++) {
        const char *tag = results[index].tag;
        const char *last = tag ? strrchr(tag, ' ') : NULL;
        if (last != NULL && (strncmp(tag, "INSERT", 6) == 0 || strncmp(tag, "UPDATE", 6) == 0 ||
                             strncmp(tag, "DELETE", 6) == 0 || strncmp(tag, "MERGE", 5) == 0)) {
            total += strtoll(last + 1, NULL, 10);
        }
    }
    return total;
}

static PyObject *Statement_executemany(Statement *self, PyObject *args) {
    PyObject *rows;
    if (!live(self) || !PyArg_ParseTuple(args, "O", &rows)) {
        return NULL;
    }
    PyObject *iterator = PyObject_GetIter(rows);
    if (iterator == NULL) {
        return NULL;
    }
    long long total = 0;
    PyObject *row;
    while ((row = PyIter_Next(iterator)) != NULL) {
        int bound = bind_row(self, row);
        Py_DECREF(row);
        if (bound != 0) {
            Py_DECREF(iterator);
            return NULL;
        }
        const eggdb_result *results = NULL;
        size_t count = 0;
        int status = api.stmt_execute(self->stmt, &results, &count);
        if (status != EGGDB_OK) {
            Py_DECREF(iterator);
            return raise_status(status);
        }
        total += changed_rows(results, count);
    }
    Py_DECREF(iterator);
    if (PyErr_Occurred()) {
        return NULL;
    }
    return PyLong_FromLongLong(total);
}

typedef struct {
    Py_buffer view;
    char format;
    int held;
} column_buffer;

static int bind_buffer_cell(eggdb_stmt *stmt, size_t index, const column_buffer *column, Py_ssize_t row) {
    const char *at = (const char *)column->view.buf + row * column->view.strides[0];
    switch (column->format) {
    case 'b': return api.bind_int64(stmt, index, *(const signed char *)at);
    case 'B': return api.bind_int64(stmt, index, *(const unsigned char *)at);
    case 'h': return api.bind_int64(stmt, index, *(const short *)at);
    case 'H': return api.bind_int64(stmt, index, *(const unsigned short *)at);
    case 'i': return api.bind_int64(stmt, index, *(const int *)at);
    case 'I': return api.bind_int64(stmt, index, *(const unsigned int *)at);
    case 'l': return api.bind_int64(stmt, index, *(const long *)at);
    case 'L': return api.bind_int64(stmt, index, (int64_t)*(const unsigned long *)at);
    case 'q': return api.bind_int64(stmt, index, *(const long long *)at);
    case 'Q': return api.bind_int64(stmt, index, (int64_t)*(const unsigned long long *)at);
    case 'f': return api.bind_double(stmt, index, *(const float *)at);
    case 'd': return api.bind_double(stmt, index, *(const double *)at);
    case '?': return api.bind_bool(stmt, index, *(const _Bool *)at);
    default: return EGGDB_ERR_INVALID_ARGUMENT;
    }
}

static char buffer_format(const char *format) {
    if (format == NULL) {
        return 'B';
    }
    if (format[0] == '@' || format[0] == '=' || format[0] == '<' || format[0] == '!') {
        format++;
    }
    return format[1] == '\0' && strchr("bBhHiIlLqQfd?", format[0]) != NULL ? format[0] : '\0';
}

static PyObject *Statement_execute_columns(Statement *self, PyObject *args) {
    PyObject *columns;
    if (!live(self) || !PyArg_ParseTuple(args, "O", &columns)) {
        return NULL;
    }
    PyObject *sequence = PySequence_Fast(columns, "columns must be a sequence of buffers or sequences");
    if (sequence == NULL) {
        return NULL;
    }
    Py_ssize_t width = PySequence_Fast_GET_SIZE(sequence);
    if ((size_t)width != self->parameter_count) {
        Py_DECREF(sequence);
        PyErr_Format(PyExc_ValueError, "%zd columns given for %zu placeholders", width, self->parameter_count);
        return NULL;
    }
    column_buffer *buffers = PyMem_Calloc(width > 0 ? (size_t)width : 1, sizeof(column_buffer));
    if (buffers == NULL) {
        Py_DECREF(sequence);
        return PyErr_NoMemory();
    }
    Py_ssize_t height = -1;
    PyObject *result = NULL;
    for (Py_ssize_t column = 0; column < width; column++) {
        PyObject *item = PySequence_Fast_GET_ITEM(sequence, column);
        Py_ssize_t length;
        if (PyObject_CheckBuffer(item) && !PyBytes_Check(item) && !PyUnicode_Check(item) &&
            PyObject_GetBuffer(item, &buffers[column].view, PyBUF_FORMAT | PyBUF_STRIDES) == 0) {
            buffers[column].held = 1;
            buffers[column].format = buffer_format(buffers[column].view.format);
            if (buffers[column].view.ndim != 1 || buffers[column].format == '\0') {
                PyErr_Format(PyExc_TypeError, "column %zd is not a one-dimensional buffer of numbers", column);
                goto done;
            }
            length = buffers[column].view.shape[0];
        } else {
            PyErr_Clear();
            length = PySequence_Length(item);
            if (length < 0) {
                goto done;
            }
        }
        if (height >= 0 && length != height) {
            PyErr_SetString(PyExc_ValueError, "every column needs the same number of rows");
            goto done;
        }
        height = length;
    }
    long long total = 0;
    for (Py_ssize_t row = 0; row < (height < 0 ? 0 : height); row++) {
        int status = api.reset(self->stmt);
        if (status == EGGDB_OK) {
            status = api.clear_bindings(self->stmt);
        }
        for (Py_ssize_t column = 0; column < width && status == EGGDB_OK; column++) {
            if (buffers[column].held) {
                status = bind_buffer_cell(self->stmt, (size_t)column + 1, &buffers[column], row);
            } else {
                PyObject *cell = PySequence_GetItem(PySequence_Fast_GET_ITEM(sequence, column), row);
                if (cell == NULL) {
                    goto done;
                }
                status = bind_value(self->stmt, (size_t)column + 1, cell);
                Py_DECREF(cell);
                if (status < 0) {
                    goto done;
                }
            }
        }
        const eggdb_result *results = NULL;
        size_t count = 0;
        if (status == EGGDB_OK) {
            status = api.stmt_execute(self->stmt, &results, &count);
        }
        if (status != EGGDB_OK) {
            raise_status(status);
            goto done;
        }
        total += changed_rows(results, count);
    }
    result = PyLong_FromLongLong(total);
done:
    for (Py_ssize_t column = 0; column < width; column++) {
        if (buffers[column].held) {
            PyBuffer_Release(&buffers[column].view);
        }
    }
    PyMem_Free(buffers);
    Py_DECREF(sequence);
    return result;
}

static PyObject *Statement_columns(Statement *self, PyObject *unused) {
    (void)unused;
    if (!live(self)) {
        return NULL;
    }
    size_t count = 0;
    int status = api.column_count(self->stmt, &count);
    if (status != EGGDB_OK) {
        return raise_status(status);
    }
    PyObject *names = PyList_New((Py_ssize_t)count);
    for (size_t column = 0; names != NULL && column < count; column++) {
        const char *name = NULL;
        status = api.column_name(self->stmt, column, &name);
        if (status != EGGDB_OK) {
            Py_DECREF(names);
            return raise_status(status);
        }
        PyList_SET_ITEM(names, (Py_ssize_t)column, PyUnicode_FromString(name ? name : ""));
    }
    return names;
}

static PyObject *Statement_start(Statement *self, PyObject *args) {
    PyObject *parameters = NULL;
    if (!live(self) || !PyArg_ParseTuple(args, "|O", &parameters)) {
        return NULL;
    }
    PyObject *empty = NULL;
    if (parameters == NULL) {
        empty = PyTuple_New(0);
        parameters = empty;
    }
    int bound = bind_row(self, parameters);
    Py_XDECREF(empty);
    if (bound != 0) {
        return NULL;
    }
    self->stepping = 1;
    Py_INCREF(self);
    return (PyObject *)self;
}

static PyObject *Statement_iternext(Statement *self) {
    if (self->stmt == NULL || !self->stepping) {
        return NULL;
    }
    int has_row = 0;
    int status;
    Py_BEGIN_ALLOW_THREADS
    status = api.step(self->stmt, &has_row);
    Py_END_ALLOW_THREADS
    if (status != EGGDB_OK) {
        self->stepping = 0;
        return raise_status(status);
    }
    if (!has_row) {
        self->stepping = 0;
        api.reset(self->stmt);
        return NULL;
    }
    const eggdb_value *values = NULL;
    size_t count = 0;
    status = api.row(self->stmt, &values, &count);
    if (status != EGGDB_OK) {
        self->stepping = 0;
        return raise_status(status);
    }
    return row_tuple(values, count);
}

typedef struct {
    char *bytes;
    size_t length;
    size_t capacity;
    char kind;
    int mixed;
    PyObject *values;
} column_gather;

static int gather_number(column_gather *column, const void *data, size_t size) {
    if (column->length + size > column->capacity) {
        size_t capacity = column->capacity ? column->capacity * 2 : 4096;
        while (capacity < column->length + size) {
            capacity *= 2;
        }
        char *grown = PyMem_Realloc(column->bytes, capacity);
        if (grown == NULL) {
            PyErr_NoMemory();
            return -1;
        }
        column->bytes = grown;
        column->capacity = capacity;
    }
    memcpy(column->bytes + column->length, data, size);
    column->length += size;
    return 0;
}

static PyObject *Statement_fetch_columns(Statement *self, PyObject *args) {
    PyObject *parameters = NULL;
    if (!live(self) || !PyArg_ParseTuple(args, "|O", &parameters)) {
        return NULL;
    }
    PyObject *empty = NULL;
    if (parameters == NULL) {
        empty = PyTuple_New(0);
        parameters = empty;
    }
    int bound = bind_row(self, parameters);
    Py_XDECREF(empty);
    if (bound != 0) {
        return NULL;
    }
    PyObject *names = Statement_columns(self, NULL);
    if (names == NULL) {
        return NULL;
    }
    Py_ssize_t width = PyList_GET_SIZE(names);
    column_gather *columns = PyMem_Calloc(width > 0 ? (size_t)width : 1, sizeof(column_gather));
    PyObject *out = NULL;
    if (columns == NULL) {
        Py_DECREF(names);
        return PyErr_NoMemory();
    }
    for (Py_ssize_t column = 0; column < width; column++) {
        columns[column].values = PyList_New(0);
        if (columns[column].values == NULL) {
            goto done;
        }
    }
    for (;;) {
        int has_row = 0;
        int status = api.step(self->stmt, &has_row);
        if (status != EGGDB_OK) {
            raise_status(status);
            goto done;
        }
        if (!has_row) {
            break;
        }
        const eggdb_value *values = NULL;
        size_t count = 0;
        status = api.row(self->stmt, &values, &count);
        if (status != EGGDB_OK) {
            raise_status(status);
            goto done;
        }
        for (Py_ssize_t column = 0; column < width && (size_t)column < count; column++) {
            column_gather *gather = &columns[column];
            const eggdb_value *value = &values[column];
            char kind = value->kind == EGGDB_INTEGER ? 'q' : value->kind == EGGDB_REAL ? 'd' : '\0';
            if (gather->kind == '\0' && gather->length == 0 && !gather->mixed && PyList_GET_SIZE(gather->values) == 0) {
                gather->kind = kind;
            }
            if (!gather->mixed && kind != '\0' && kind == gather->kind) {
                const void *data = kind == 'q' ? (const void *)&value->integer : (const void *)&value->real;
                if (gather_number(gather, data, 8) != 0) {
                    goto done;
                }
                continue;
            }
            if (!gather->mixed) {
                gather->mixed = 1;
                size_t gathered_count = gather->length / 8;
                for (size_t at = 0; at < gathered_count; at++) {
                    PyObject *earlier = gather->kind == 'q'
                        ? PyLong_FromLongLong(((const long long *)gather->bytes)[at])
                        : PyFloat_FromDouble(((const double *)gather->bytes)[at]);
                    if (earlier == NULL || PyList_Append(gather->values, earlier) != 0) {
                        Py_XDECREF(earlier);
                        goto done;
                    }
                    Py_DECREF(earlier);
                }
                gather->length = 0;
            }
            PyObject *cell = converted(value);
            if (cell == NULL || PyList_Append(gather->values, cell) != 0) {
                Py_XDECREF(cell);
                goto done;
            }
            Py_DECREF(cell);
        }
    }
    api.reset(self->stmt);
    PyObject *array_module = PyImport_ImportModule("array");
    if (array_module == NULL) {
        goto done;
    }
    out = PyDict_New();
    for (Py_ssize_t column = 0; out != NULL && column < width; column++) {
        column_gather *gather = &columns[column];
        PyObject *value;
        if (!gather->mixed && gather->kind != '\0') {
            PyObject *data = PyBytes_FromStringAndSize(gather->bytes ? gather->bytes : "", (Py_ssize_t)gather->length);
            value = data == NULL ? NULL : PyObject_CallMethod(array_module, "array", "CN", gather->kind, data);
        } else {
            value = gather->values;
            Py_INCREF(value);
        }
        if (value == NULL || PyDict_SetItem(out, PyList_GET_ITEM(names, column), value) != 0) {
            Py_XDECREF(value);
            Py_CLEAR(out);
            break;
        }
        Py_DECREF(value);
    }
    Py_DECREF(array_module);
done:
    for (Py_ssize_t column = 0; column < width; column++) {
        PyMem_Free(columns[column].bytes);
        Py_XDECREF(columns[column].values);
    }
    PyMem_Free(columns);
    Py_DECREF(names);
    if (out == NULL && self->stmt != NULL) {
        api.reset(self->stmt);
    }
    return out;
}

static PyObject *Statement_reset(Statement *self, PyObject *unused) {
    (void)unused;
    if (self->stmt != NULL) {
        api.reset(self->stmt);
        self->stepping = 0;
    }
    Py_RETURN_NONE;
}

static PyObject *Statement_close(Statement *self, PyObject *unused) {
    (void)unused;
    if (self->stmt != NULL) {
        api.finalize(self->stmt);
        self->stmt = NULL;
    }
    Py_RETURN_NONE;
}

static PyObject *Statement_parameter_count(Statement *self, void *closure) {
    (void)closure;
    return PyLong_FromSize_t(self->parameter_count);
}

static PyMethodDef Statement_methods[] = {
    {"execute", (PyCFunction)Statement_execute, METH_VARARGS, "Runs the statement once with a sequence of parameters."},
    {"executemany", (PyCFunction)Statement_executemany, METH_VARARGS, "Runs the statement once per row of an iterable; returns the rows changed."},
    {"execute_columns", (PyCFunction)Statement_execute_columns, METH_VARARGS,
     "Runs the statement once per row of parameter columns, each a one-dimensional numeric buffer or a sequence; returns the rows changed."},
    {"fetch_columns", (PyCFunction)Statement_fetch_columns, METH_VARARGS,
     "Runs the statement and returns its columns by name: array('q') or array('d') for all-integer or all-real columns, lists otherwise."},
    {"columns", (PyCFunction)Statement_columns, METH_NOARGS, "The names of the columns the statement returns."},
    {"start", (PyCFunction)Statement_start, METH_VARARGS, "Binds parameters and returns the statement as an iterator over its rows."},
    {"reset", (PyCFunction)Statement_reset, METH_NOARGS, "Ends a run being stepped through."},
    {"close", (PyCFunction)Statement_close, METH_NOARGS, "Frees the statement."},
    {NULL, NULL, 0, NULL},
};

static PyGetSetDef Statement_getset[] = {
    {"parameter_count", (getter)Statement_parameter_count, NULL, "The number of placeholders.", NULL},
    {NULL, NULL, NULL, NULL, NULL},
};

static PyTypeObject StatementType = {
    PyVarObject_HEAD_INIT(NULL, 0)
    .tp_name = "eggdb._native.Statement",
    .tp_basicsize = sizeof(Statement),
    .tp_dealloc = (destructor)Statement_dealloc,
    .tp_flags = Py_TPFLAGS_DEFAULT,
    .tp_doc = "A prepared statement on a session, bound and stepped in C.",
    .tp_iter = PyObject_SelfIter,
    .tp_iternext = (iternextfunc)Statement_iternext,
    .tp_methods = Statement_methods,
    .tp_getset = Statement_getset,
    .tp_new = Statement_new,
};

static PyObject *copy_finished(eggdb_copy *copy) {
    const eggdb_result *results = NULL;
    size_t count = 0;
    int status = api.copy_end(copy, &results, &count);
    PyObject *out = status == EGGDB_OK ? gathered(results, count) : raise_status(status);
    api.copy_free(copy);
    return out;
}

static PyObject *copy_in(PyObject *module, PyObject *args) {
    (void)module;
    PyObject *session;
    const char *sql;
    PyObject *pieces;
    if (!ready() || !PyArg_ParseTuple(args, "OsO", &session, &sql, &pieces)) {
        return NULL;
    }
    eggdb_copy *copy = NULL;
    int status = api.copy_in_begin(address_of(session), sql, &copy);
    if (status != EGGDB_OK) {
        return raise_status(status);
    }
    PyObject *iterator = PyObject_GetIter(pieces);
    if (iterator == NULL) {
        api.copy_free(copy);
        return NULL;
    }
    PyObject *piece;
    while ((piece = PyIter_Next(iterator)) != NULL) {
        Py_buffer view;
        PyObject *encoded = PyUnicode_Check(piece) ? PyUnicode_AsUTF8String(piece) : (Py_INCREF(piece), piece);
        Py_DECREF(piece);
        if (encoded == NULL || PyObject_GetBuffer(encoded, &view, PyBUF_SIMPLE) != 0) {
            Py_XDECREF(encoded);
            Py_DECREF(iterator);
            api.copy_free(copy);
            return NULL;
        }
        Py_BEGIN_ALLOW_THREADS
        status = view.len > 0 ? api.copy_put(copy, view.buf, (size_t)view.len) : EGGDB_OK;
        Py_END_ALLOW_THREADS
        PyBuffer_Release(&view);
        Py_DECREF(encoded);
        if (status != EGGDB_OK) {
            Py_DECREF(iterator);
            raise_status(status);
            api.copy_free(copy);
            return NULL;
        }
    }
    Py_DECREF(iterator);
    if (PyErr_Occurred()) {
        api.copy_free(copy);
        return NULL;
    }
    return copy_finished(copy);
}

static PyObject *copy_out(PyObject *module, PyObject *args) {
    (void)module;
    PyObject *session;
    const char *sql;
    PyObject *sink;
    if (!ready() || !PyArg_ParseTuple(args, "OsO", &session, &sql, &sink)) {
        return NULL;
    }
    eggdb_copy *copy = NULL;
    int status = api.copy_out_begin(address_of(session), sql, &copy);
    if (status != EGGDB_OK) {
        return raise_status(status);
    }
    for (;;) {
        const uint8_t *data = NULL;
        size_t length = 0;
        Py_BEGIN_ALLOW_THREADS
        status = api.copy_read(copy, &data, &length);
        Py_END_ALLOW_THREADS
        if (status != EGGDB_OK) {
            raise_status(status);
            api.copy_free(copy);
            return NULL;
        }
        if (length == 0) {
            break;
        }
        PyObject *done = PyObject_CallFunction(sink, "y#", (const char *)data, (Py_ssize_t)length);
        if (done == NULL) {
            api.copy_free(copy);
            return NULL;
        }
        Py_DECREF(done);
    }
    return copy_finished(copy);
}

static PyMethodDef module_methods[] = {
    {"load", load, METH_VARARGS, "Opens the EggDB C library at a path and sets the error factory."},
    {"execute", execute, METH_VARARGS, "Runs SQL with text parameters on a session address."},
    {"copy_in", copy_in, METH_VARARGS, "Runs a COPY ... FROM STDIN fed by an iterable of bytes-like or str pieces."},
    {"copy_out", copy_out, METH_VARARGS, "Runs a COPY ... TO STDOUT, calling a function with each piece."},
    {NULL, NULL, 0, NULL},
};

static struct PyModuleDef native_module = {
    PyModuleDef_HEAD_INIT,
    "eggdb._native",
    "EggDB's Python driver's C part: statements bound, stepped and batched through the CPython C API.",
    -1,
    module_methods,
    NULL,
    NULL,
    NULL,
    NULL,
};

PyMODINIT_FUNC PyInit__native(void) {
    if (PyType_Ready(&StatementType) < 0) {
        return NULL;
    }
    PyObject *module = PyModule_Create(&native_module);
    if (module == NULL) {
        return NULL;
    }
    Py_INCREF(&StatementType);
    if (PyModule_AddObject(module, "Statement", (PyObject *)&StatementType) < 0) {
        Py_DECREF(&StatementType);
        Py_DECREF(module);
        return NULL;
    }
    return module;
}
