// Package eggdb is EggDB's database/sql driver, embedded in the process through EggDB's C API.
//
// Register it with a blank import and open a database with sql.Open("eggdb", "app.db"). A data
// source name is a path, ":memory:" for a database in memory, with optional
// "?dialect=postgresql&mode=ro" settings; the dialect is sqlite unless given. Placeholders are
// "?" in every dialect. Build with CGO_LDFLAGS="-L<directory of libeggdb_capi>".
package eggdb

import (
	"context"
	"database/sql"
	"database/sql/driver"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Error is an error EggDB reported, with its SQLSTATE.
type Error struct {
	Message  string
	SQLState string
	Status   int
}

func (e *Error) Error() string {
	if e.SQLState == "" {
		return e.Message
	}
	return e.SQLState + ": " + e.Message
}

// Driver is EggDB's database/sql driver, registered as "eggdb".
type Driver struct{}

func init() {
	sql.Register("eggdb", &Driver{})
}

var dialects = map[string]bool{"sqlite": true, "postgresql": true, "mysql": true, "oracle": true, "sqlserver": true}

// Open opens the database a data source name names.
func (Driver) Open(name string) (driver.Conn, error) {
	if isServerURL(name) {
		session, err := openSession(name, "postgresql", false)
		if err != nil {
			return nil, err
		}
		return &conn{session: session, dialect: "postgresql"}, nil
	}
	path, query, _ := strings.Cut(name, "?")
	settings, err := url.ParseQuery(query)
	if err != nil {
		return nil, fmt.Errorf("eggdb: the settings after ? are not a query string: %w", err)
	}
	dialect := strings.ToLower(settings.Get("dialect"))
	if dialect == "" {
		dialect = "sqlite"
	}
	if !dialects[dialect] {
		return nil, &Error{Message: "unknown dialect " + dialect, SQLState: "08001"}
	}
	session, err := openSession(path, dialect, strings.EqualFold(settings.Get("mode"), "ro"))
	if err != nil {
		return nil, err
	}
	return &conn{session: session, dialect: dialect}, nil
}

type conn struct {
	mutex   sync.Mutex
	session *nativeSession
	dialect string
}

func (c *conn) run(query string, args []driver.NamedValue) ([]statementResult, error) {
	values := make([]driver.Value, len(args))
	for index, arg := range args {
		if arg.Name != "" {
			return nil, &Error{Message: "named parameters are not supported; use ?", SQLState: "0A000"}
		}
		values[index] = arg.Value
	}
	query, values = bound(query, c.dialect, values)
	texts := make([]*string, len(values))
	for index, value := range values {
		text, err := parameterText(value, c.dialect)
		if err != nil {
			return nil, err
		}
		texts[index] = text
	}
	c.mutex.Lock()
	defer c.mutex.Unlock()
	return c.session.execute(query, texts)
}

func (c *conn) Prepare(query string) (driver.Stmt, error) {
	numbered, _ := bound(query, c.dialect, nil)
	c.mutex.Lock()
	defer c.mutex.Unlock()
	prepared, err := c.session.prepare(numbered)
	if err != nil {
		return nil, err
	}
	return &stmt{conn: c, query: query, prepared: prepared}, nil
}

func (c *conn) CheckNamedValue(value *driver.NamedValue) error {
	if value.Name != "" {
		return &Error{Message: "named parameters are not supported; use ?", SQLState: "0A000"}
	}
	switch typed := value.Value.(type) {
	case int:
		value.Value = int64(typed)
		return nil
	case int32:
		value.Value = int64(typed)
		return nil
	case float32:
		value.Value = float64(typed)
		return nil
	}
	return driver.ErrSkip
}

func (c *conn) Close() error {
	c.mutex.Lock()
	defer c.mutex.Unlock()
	c.session.close()
	return nil
}

func (c *conn) Begin() (driver.Tx, error) {
	return c.BeginTx(context.Background(), driver.TxOptions{})
}

func (c *conn) BeginTx(ctx context.Context, options driver.TxOptions) (driver.Tx, error) {
	if options.ReadOnly {
		return nil, &Error{Message: "read-only transactions are not supported yet; open with mode=ro", SQLState: "0A000"}
	}
	if _, err := c.run("BEGIN", nil); err != nil {
		return nil, err
	}
	return &tx{conn: c}, nil
}

func (c *conn) ExecContext(ctx context.Context, query string, args []driver.NamedValue) (driver.Result, error) {
	results, err := c.run(query, args)
	if err != nil {
		return nil, err
	}
	var total int64
	for _, result := range results {
		total += result.changedRows()
	}
	return changed(total), nil
}

func (c *conn) QueryContext(ctx context.Context, query string, args []driver.NamedValue) (driver.Rows, error) {
	results, err := c.run(query, args)
	if err != nil {
		return nil, err
	}
	for index := len(results) - 1; index >= 0; index-- {
		if len(results[index].columns) > 0 {
			return &rows{result: results[index]}, nil
		}
	}
	return &rows{}, nil
}

type changed int64

func (changed) LastInsertId() (int64, error) {
	return 0, errors.New("eggdb: LastInsertId is not supported; use RETURNING")
}

func (c changed) RowsAffected() (int64, error) {
	return int64(c), nil
}

type tx struct {
	conn *conn
}

func (t *tx) Commit() error {
	_, err := t.conn.run("COMMIT", nil)
	return err
}

func (t *tx) Rollback() error {
	_, err := t.conn.run("ROLLBACK", nil)
	return err
}

type stmt struct {
	conn     *conn
	query    string
	prepared *nativeStatement
}

func (s *stmt) Close() error {
	s.conn.mutex.Lock()
	defer s.conn.mutex.Unlock()
	s.prepared.finalize()
	return nil
}

func (s *stmt) NumInput() int {
	if s.conn.dialect != "postgresql" && strings.ContainsRune(s.query, '?') {
		return s.prepared.parameters
	}
	return -1
}

func (s *stmt) Exec(args []driver.Value) (driver.Result, error) {
	return s.ExecContext(context.Background(), named(args))
}

func (s *stmt) Query(args []driver.Value) (driver.Rows, error) {
	return s.QueryContext(context.Background(), named(args))
}

func (s *stmt) ExecContext(ctx context.Context, args []driver.NamedValue) (driver.Result, error) {
	values, err := positional(args)
	if err != nil {
		return nil, err
	}
	s.conn.mutex.Lock()
	defer s.conn.mutex.Unlock()
	if err := s.prepared.bindAll(values, s.conn.dialect); err != nil {
		return nil, err
	}
	results, err := s.prepared.execute()
	if err != nil {
		return nil, err
	}
	var total int64
	for _, result := range results {
		total += result.changedRows()
	}
	return changed(total), nil
}

func (s *stmt) QueryContext(ctx context.Context, args []driver.NamedValue) (driver.Rows, error) {
	values, err := positional(args)
	if err != nil {
		return nil, err
	}
	s.conn.mutex.Lock()
	defer s.conn.mutex.Unlock()
	if err := s.prepared.bindAll(values, s.conn.dialect); err != nil {
		return nil, err
	}
	columns, err := s.prepared.columnNames()
	if err != nil {
		return nil, err
	}
	return &steppedRows{stmt: s, columns: columns}, nil
}

func positional(args []driver.NamedValue) ([]any, error) {
	values := make([]any, len(args))
	for index, arg := range args {
		if arg.Name != "" {
			return nil, &Error{Message: "named parameters are not supported; use ?", SQLState: "0A000"}
		}
		values[index] = arg.Value
	}
	return values, nil
}

type steppedRows struct {
	stmt    *stmt
	columns []string
	done    bool
}

func (r *steppedRows) Columns() []string {
	return r.columns
}

func (r *steppedRows) Close() error {
	r.stmt.conn.mutex.Lock()
	defer r.stmt.conn.mutex.Unlock()
	r.stmt.prepared.reset()
	r.done = true
	return nil
}

func (r *steppedRows) Next(dest []driver.Value) error {
	if r.done {
		return io.EOF
	}
	r.stmt.conn.mutex.Lock()
	defer r.stmt.conn.mutex.Unlock()
	cells := make([]any, len(dest))
	found, err := r.stmt.prepared.step(cells)
	if err != nil {
		r.done = true
		return err
	}
	if !found {
		r.done = true
		return io.EOF
	}
	for index := range dest {
		dest[index] = cells[index]
	}
	return nil
}

// CopyFrom runs a COPY ... FROM STDIN on the connection with the data read from source, sent in
// pieces as it is read, and returns the number of rows loaded.
func CopyFrom(ctx context.Context, connection *sql.Conn, query string, source io.Reader) (int64, error) {
	var loaded int64
	err := connection.Raw(func(raw any) error {
		c, ok := raw.(*conn)
		if !ok {
			return errors.New("eggdb: CopyFrom needs a connection from the eggdb driver")
		}
		c.mutex.Lock()
		defer c.mutex.Unlock()
		results, err := c.session.copyIn(query, func(put func([]byte) error) error {
			buffer := make([]byte, 64*1024)
			for {
				read, err := source.Read(buffer)
				if read > 0 {
					if putErr := put(buffer[:read]); putErr != nil {
						return putErr
					}
				}
				if err == io.EOF {
					return nil
				}
				if err != nil {
					return err
				}
			}
		})
		if err != nil {
			return err
		}
		loaded = copiedRows(results)
		return nil
	})
	return loaded, err
}

// CopyTo runs a COPY ... TO STDOUT on the connection and writes its data to sink as it is read.
func CopyTo(ctx context.Context, connection *sql.Conn, query string, sink io.Writer) error {
	return connection.Raw(func(raw any) error {
		c, ok := raw.(*conn)
		if !ok {
			return errors.New("eggdb: CopyTo needs a connection from the eggdb driver")
		}
		c.mutex.Lock()
		defer c.mutex.Unlock()
		_, err := c.session.copyOut(query, func(piece []byte) error {
			_, err := sink.Write(piece)
			return err
		})
		return err
	})
}

func copiedRows(results []statementResult) int64 {
	for index := len(results) - 1; index >= 0; index-- {
		words := strings.Fields(results[index].tag)
		if len(words) == 2 && words[0] == "COPY" {
			if count, err := strconv.ParseInt(words[1], 10, 64); err == nil {
				return count
			}
		}
	}
	return -1
}

func named(args []driver.Value) []driver.NamedValue {
	out := make([]driver.NamedValue, len(args))
	for index, value := range args {
		out[index] = driver.NamedValue{Ordinal: index + 1, Value: value}
	}
	return out
}

type rows struct {
	result statementResult
	next   int
}

func (r *rows) Columns() []string {
	return r.result.columns
}

func (r *rows) Close() error {
	r.next = len(r.result.rows)
	return nil
}

func (r *rows) Next(dest []driver.Value) error {
	if r.next >= len(r.result.rows) {
		return io.EOF
	}
	for index, value := range r.result.rows[r.next] {
		if index < len(dest) {
			dest[index] = value
		}
	}
	r.next++
	return nil
}

func parameterText(value driver.Value, dialect string) (*string, error) {
	var text string
	switch typed := value.(type) {
	case nil:
		return nil, nil
	case bool:
		switch {
		case dialect == "postgresql" && typed:
			text = "true"
		case dialect == "postgresql":
			text = "false"
		case typed:
			text = "1"
		default:
			text = "0"
		}
	case int64:
		text = strconv.FormatInt(typed, 10)
	case float64:
		text = strconv.FormatFloat(typed, 'g', -1, 64)
	case string:
		text = typed
	case []byte:
		text = `\x` + hex.EncodeToString(typed)
	case time.Time:
		text = typed.UTC().Format("2006-01-02 15:04:05.999999999")
	default:
		return nil, fmt.Errorf("eggdb: cannot bind a %T", value)
	}
	return &text, nil
}

func bound(query, dialect string, values []driver.Value) (string, []driver.Value) {
	postgresql := dialect == "postgresql"
	kept := make([]driver.Value, 0, len(values))
	var out strings.Builder
	var quote rune
	given := 0
	for _, character := range query {
		switch {
		case quote != 0:
			if character == quote {
				quote = 0
			}
			out.WriteRune(character)
		case character == '\'' || character == '"':
			quote = character
			out.WriteRune(character)
		case character == '?':
			var value driver.Value
			if given < len(values) {
				value = values[given]
			}
			given++
			if blob, isBlob := value.([]byte); isBlob && !postgresql {
				out.WriteString("X'" + hex.EncodeToString(blob) + "'")
				continue
			}
			kept = append(kept, value)
			if postgresql {
				out.WriteString("$" + strconv.Itoa(len(kept)))
			} else {
				out.WriteRune('?')
			}
		default:
			out.WriteRune(character)
		}
	}
	if given < len(values) {
		kept = append(kept, values[given:]...)
	}
	return out.String(), kept
}
