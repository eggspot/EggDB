package eggdb

/*
#cgo CFLAGS: -I${SRCDIR}/../../crates/eggdb-capi/include
#cgo LDFLAGS: -leggdb_capi
#include <stdlib.h>
#include "eggdb.h"

static const eggdb_value *value_at(const eggdb_result *results, size_t result, size_t row, size_t column) {
    return eggdb_result_value(&results[result], row, column);
}

static const eggdb_result *result_at(const eggdb_result *results, size_t index) {
    return &results[index];
}

static const char *column_name(const eggdb_result *result, size_t column) {
    return result->column_names[column];
}
*/
import "C"

import (
	"strings"
	"unsafe"
)

type nativeSession struct {
	database *C.eggdb_db
	session  *C.eggdb_sql
}

type statementResult struct {
	tag     string
	columns []string
	rows    [][]any
}

func (result statementResult) changedRows() int64 {
	words := strings.Fields(result.tag)
	if len(words) < 2 {
		return 0
	}
	switch words[0] {
	case "INSERT", "UPDATE", "DELETE":
	default:
		return 0
	}
	var count int64
	for _, digit := range words[len(words)-1] {
		if digit < '0' || digit > '9' {
			return 0
		}
		count = count*10 + int64(digit-'0')
	}
	return count
}

func lastError(status C.int) error {
	message := C.GoString(C.eggdb_last_error())
	if message == "" {
		message = "EggDB failed"
	}
	return &Error{Message: message, SQLState: C.GoString(C.eggdb_last_sqlstate()), Status: int(status)}
}

func openSession(path, dialect string, readOnly bool) (*nativeSession, error) {
	cPath := C.CString(path)
	defer C.free(unsafe.Pointer(cPath))
	if isServerURL(path) {
		opened := &nativeSession{}
		if status := C.eggdb_sql_connect(cPath, &opened.session); status != C.EGGDB_OK {
			return nil, lastError(status)
		}
		return opened, nil
	}
	cDialect := C.CString(dialect)
	defer C.free(unsafe.Pointer(cDialect))
	var options C.eggdb_options
	if readOnly {
		options.flags = C.EGGDB_OPEN_READ_ONLY
	}
	opened := &nativeSession{}
	if status := C.eggdb_open(cPath, &options, &opened.database); status != C.EGGDB_OK {
		return nil, lastError(status)
	}
	if status := C.eggdb_sql_open(opened.database, cDialect, &opened.session); status != C.EGGDB_OK {
		err := lastError(status)
		C.eggdb_close(opened.database)
		return nil, err
	}
	return opened, nil
}

func isServerURL(path string) bool {
	return strings.HasPrefix(path, "postgresql://") || strings.HasPrefix(path, "postgres://")
}

func (s *nativeSession) close() {
	if s.session != nil {
		C.eggdb_sql_close(s.session)
		s.session = nil
	}
	if s.database != nil {
		C.eggdb_close(s.database)
		s.database = nil
	}
}

func (s *nativeSession) execute(sql string, parameters []*string) ([]statementResult, error) {
	if s.session == nil {
		return nil, &Error{Message: "the connection is closed", SQLState: "08003"}
	}
	cSQL := C.CString(sql)
	defer C.free(unsafe.Pointer(cSQL))
	count := len(parameters)
	if count == 0 {
		count = 1
	}
	array := unsafe.Slice((*C.eggdb_parameter)(C.calloc(C.size_t(count), C.size_t(unsafe.Sizeof(C.eggdb_parameter{})))), count)
	defer C.free(unsafe.Pointer(&array[0]))
	for index, text := range parameters {
		if text == nil {
			continue
		}
		bytes := C.CBytes([]byte(*text))
		defer C.free(bytes)
		array[index].text = (*C.uint8_t)(bytes)
		array[index].len = C.size_t(len(*text))
	}
	var results *C.eggdb_result
	var resultCount C.size_t
	status := C.eggdb_sql_execute_with_parameters(s.session, cSQL, &array[0], C.size_t(len(parameters)), &results, &resultCount)
	if status != C.EGGDB_OK {
		return nil, lastError(status)
	}
	return gathered(results, int(resultCount)), nil
}

func gathered(results *C.eggdb_result, count int) []statementResult {
	out := make([]statementResult, 0, count)
	for index := 0; index < count; index++ {
		result := C.result_at(results, C.size_t(index))
		columnCount := int(result.column_count)
		columns := make([]string, columnCount)
		for column := range columns {
			columns[column] = C.GoString(C.column_name(result, C.size_t(column)))
		}
		rows := make([][]any, int(result.row_count))
		for row := range rows {
			cells := make([]any, columnCount)
			for column := range cells {
				cells[column] = converted(C.value_at(results, C.size_t(index), C.size_t(row), C.size_t(column)))
			}
			rows[row] = cells
		}
		out = append(out, statementResult{tag: C.GoString(result.tag), columns: columns, rows: rows})
	}
	return out
}

func converted(value *C.eggdb_value) any {
	switch value.kind {
	case C.EGGDB_INTEGER:
		return int64(value.integer)
	case C.EGGDB_REAL:
		return float64(value.real)
	case C.EGGDB_TEXT:
		return C.GoStringN((*C.char)(unsafe.Pointer(value.data)), C.int(value.len))
	case C.EGGDB_BLOB:
		return C.GoBytes(unsafe.Pointer(value.data), C.int(value.len))
	default:
		return nil
	}
}

type nativeStatement struct {
	session    *nativeSession
	handle     *C.eggdb_stmt
	parameters int
	columns    []string
}

func (s *nativeSession) prepare(sql string) (*nativeStatement, error) {
	if s.session == nil {
		return nil, &Error{Message: "the connection is closed", SQLState: "08003"}
	}
	cSQL := C.CString(sql)
	defer C.free(unsafe.Pointer(cSQL))
	prepared := &nativeStatement{session: s}
	if status := C.eggdb_sql_prepare(s.session, cSQL, C.size_t(len(sql)), &prepared.handle); status != C.EGGDB_OK {
		return nil, lastError(status)
	}
	var count C.size_t
	if status := C.eggdb_stmt_parameter_count(prepared.handle, &count); status != C.EGGDB_OK {
		err := lastError(status)
		prepared.finalize()
		return nil, err
	}
	prepared.parameters = int(count)
	return prepared, nil
}

func (p *nativeStatement) finalize() {
	if p.handle != nil {
		C.eggdb_stmt_finalize(p.handle)
		p.handle = nil
	}
}

func (p *nativeStatement) bindAll(values []any, dialect string) error {
	if p.handle == nil {
		return &Error{Message: "the statement is closed", SQLState: "26000"}
	}
	if status := C.eggdb_stmt_reset(p.handle); status != C.EGGDB_OK {
		return lastError(status)
	}
	if status := C.eggdb_stmt_clear_bindings(p.handle); status != C.EGGDB_OK {
		return lastError(status)
	}
	for index, value := range values {
		if status, err := p.bind(C.size_t(index+1), value, dialect); err != nil {
			return err
		} else if status != C.EGGDB_OK {
			return lastError(status)
		}
	}
	return nil
}

func (p *nativeStatement) bind(index C.size_t, value any, dialect string) (C.int, error) {
	switch typed := value.(type) {
	case nil:
		return C.eggdb_stmt_bind_null(p.handle, index), nil
	case bool:
		flag := C.int(0)
		if typed {
			flag = 1
		}
		return C.eggdb_stmt_bind_bool(p.handle, index, flag), nil
	case int64:
		return C.eggdb_stmt_bind_int64(p.handle, index, C.int64_t(typed)), nil
	case float64:
		return C.eggdb_stmt_bind_double(p.handle, index, C.double(typed)), nil
	case []byte:
		if len(typed) == 0 {
			return C.eggdb_stmt_bind_blob(p.handle, index, nil, 0), nil
		}
		data := C.CBytes(typed)
		defer C.free(data)
		return C.eggdb_stmt_bind_blob(p.handle, index, (*C.uint8_t)(data), C.size_t(len(typed))), nil
	default:
		text, err := parameterText(value, dialect)
		if err != nil {
			return 0, err
		}
		cText := C.CString(*text)
		defer C.free(unsafe.Pointer(cText))
		return C.eggdb_stmt_bind_text(p.handle, index, cText, C.size_t(len(*text))), nil
	}
}

func (p *nativeStatement) execute() ([]statementResult, error) {
	var results *C.eggdb_result
	var count C.size_t
	if status := C.eggdb_stmt_execute(p.handle, &results, &count); status != C.EGGDB_OK {
		return nil, lastError(status)
	}
	return gathered(results, int(count)), nil
}

func (p *nativeStatement) columnNames() ([]string, error) {
	var count C.size_t
	if status := C.eggdb_stmt_column_count(p.handle, &count); status != C.EGGDB_OK {
		return nil, lastError(status)
	}
	names := make([]string, int(count))
	for column := range names {
		var name *C.char
		if status := C.eggdb_stmt_column_name(p.handle, C.size_t(column), &name); status != C.EGGDB_OK {
			return nil, lastError(status)
		}
		names[column] = C.GoString(name)
	}
	return names, nil
}

func (p *nativeStatement) step(dest []any) (bool, error) {
	var hasRow C.int
	if status := C.eggdb_stmt_step(p.handle, &hasRow); status != C.EGGDB_OK {
		return false, lastError(status)
	}
	if hasRow == 0 {
		return false, nil
	}
	var values *C.eggdb_value
	var count C.size_t
	if status := C.eggdb_stmt_row(p.handle, &values, &count); status != C.EGGDB_OK {
		return false, lastError(status)
	}
	cells := unsafe.Slice(values, int(count))
	for index := range cells {
		if index < len(dest) {
			dest[index] = converted(&cells[index])
		}
	}
	return true, nil
}

func (p *nativeStatement) reset() {
	if p.handle != nil {
		C.eggdb_stmt_reset(p.handle)
	}
}

func (s *nativeSession) copyIn(sql string, pieces func(put func([]byte) error) error) ([]statementResult, error) {
	if s.session == nil {
		return nil, &Error{Message: "the connection is closed", SQLState: "08003"}
	}
	cSQL := C.CString(sql)
	defer C.free(unsafe.Pointer(cSQL))
	var copy *C.eggdb_copy
	if status := C.eggdb_sql_copy_in_begin(s.session, cSQL, &copy); status != C.EGGDB_OK {
		return nil, lastError(status)
	}
	defer C.eggdb_copy_free(copy)
	put := func(piece []byte) error {
		if len(piece) == 0 {
			return nil
		}
		data := C.CBytes(piece)
		defer C.free(data)
		if status := C.eggdb_copy_put(copy, (*C.uint8_t)(data), C.size_t(len(piece))); status != C.EGGDB_OK {
			return lastError(status)
		}
		return nil
	}
	if err := pieces(put); err != nil {
		return nil, err
	}
	return endCopy(copy)
}

func (s *nativeSession) copyOut(sql string, each func([]byte) error) ([]statementResult, error) {
	if s.session == nil {
		return nil, &Error{Message: "the connection is closed", SQLState: "08003"}
	}
	cSQL := C.CString(sql)
	defer C.free(unsafe.Pointer(cSQL))
	var copy *C.eggdb_copy
	if status := C.eggdb_sql_copy_out_begin(s.session, cSQL, &copy); status != C.EGGDB_OK {
		return nil, lastError(status)
	}
	defer C.eggdb_copy_free(copy)
	for {
		var data *C.uint8_t
		var length C.size_t
		if status := C.eggdb_copy_read(copy, &data, &length); status != C.EGGDB_OK {
			return nil, lastError(status)
		}
		if length == 0 {
			break
		}
		if err := each(C.GoBytes(unsafe.Pointer(data), C.int(length))); err != nil {
			return nil, err
		}
	}
	return endCopy(copy)
}

func endCopy(copy *C.eggdb_copy) ([]statementResult, error) {
	var results *C.eggdb_result
	var count C.size_t
	if status := C.eggdb_copy_end(copy, &results, &count); status != C.EGGDB_OK {
		return nil, lastError(status)
	}
	return gathered(results, int(count)), nil
}
