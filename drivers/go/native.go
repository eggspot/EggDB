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
