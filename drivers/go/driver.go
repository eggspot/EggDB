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
	texts := make([]*string, len(args))
	for index, arg := range args {
		if arg.Name != "" {
			return nil, &Error{Message: "named parameters are not supported; use ?", SQLState: "0A000"}
		}
		text, err := parameterText(arg.Value, c.dialect)
		if err != nil {
			return nil, err
		}
		texts[index] = text
	}
	c.mutex.Lock()
	defer c.mutex.Unlock()
	return c.session.execute(numbered(query, c.dialect), texts)
}

func (c *conn) Prepare(query string) (driver.Stmt, error) {
	return &stmt{conn: c, query: query}, nil
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
	conn  *conn
	query string
}

func (s *stmt) Close() error {
	return nil
}

func (s *stmt) NumInput() int {
	return -1
}

func (s *stmt) Exec(args []driver.Value) (driver.Result, error) {
	return s.conn.ExecContext(context.Background(), s.query, named(args))
}

func (s *stmt) Query(args []driver.Value) (driver.Rows, error) {
	return s.conn.QueryContext(context.Background(), s.query, named(args))
}

func (s *stmt) ExecContext(ctx context.Context, args []driver.NamedValue) (driver.Result, error) {
	return s.conn.ExecContext(ctx, s.query, args)
}

func (s *stmt) QueryContext(ctx context.Context, args []driver.NamedValue) (driver.Rows, error) {
	return s.conn.QueryContext(ctx, s.query, args)
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
	copy(dest, r.result.rows[r.next])
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

func numbered(query, dialect string) string {
	if dialect != "postgresql" {
		return query
	}
	var out strings.Builder
	var quote rune
	number := 0
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
			number++
			out.WriteString("$" + strconv.Itoa(number))
		default:
			out.WriteRune(character)
		}
	}
	return out.String()
}
