package eggdb_test

import (
	"bytes"
	"context"
	"database/sql"
	"errors"
	"path/filepath"
	"strings"
	"testing"

	"github.com/eggspot/EggDB/drivers/go"
)

func open(t *testing.T, name string) *sql.DB {
	t.Helper()
	db, err := sql.Open("eggdb", name)
	if err != nil {
		t.Fatal(err)
	}
	db.SetMaxOpenConns(1)
	t.Cleanup(func() { db.Close() })
	return db
}

func TestRowsComeBackWithGoTypes(t *testing.T) {
	db := open(t, filepath.Join(t.TempDir(), "app.db"))
	if _, err := db.Exec("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL, photo BLOB)"); err != nil {
		t.Fatal(err)
	}
	result, err := db.Exec("INSERT INTO people (name, score, photo) VALUES (?, ?, ?)", "Ada", 1.5, []byte{0, 255})
	if err != nil {
		t.Fatal(err)
	}
	if count, _ := result.RowsAffected(); count != 1 {
		t.Fatalf("rows affected: %d", count)
	}
	var id int64
	var name string
	var score float64
	var photo []byte
	if err := db.QueryRow("SELECT id, name, score, photo FROM people WHERE name = ?", "Ada").Scan(&id, &name, &score, &photo); err != nil {
		t.Fatal(err)
	}
	if id != 1 || name != "Ada" || score != 1.5 || len(photo) != 2 || photo[1] != 255 {
		t.Fatalf("read back %d %q %v %v", id, name, score, photo)
	}
}

func TestARolledBackTransactionLeavesNothing(t *testing.T) {
	db := open(t, filepath.Join(t.TempDir(), "app.db"))
	if _, err := db.Exec("CREATE TABLE t (n INTEGER)"); err != nil {
		t.Fatal(err)
	}
	tx, err := db.Begin()
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Exec("INSERT INTO t VALUES (?)", 1); err != nil {
		t.Fatal(err)
	}
	if err := tx.Rollback(); err != nil {
		t.Fatal(err)
	}
	var count int
	if err := db.QueryRow("SELECT count(*) FROM t").Scan(&count); err != nil || count != 0 {
		t.Fatalf("count %d, error %v", count, err)
	}
}

func TestErrorsCarryTheSQLState(t *testing.T) {
	db := open(t, ":memory:")
	_, err := db.Query("SELECT * FROM nowhere")
	var eggErr *eggdb.Error
	if !errors.As(err, &eggErr) || len(eggErr.SQLState) != 5 || !strings.Contains(eggErr.Message, "nowhere") {
		t.Fatalf("error %v", err)
	}
}

func TestQuestionMarksAreNumberedForPostgreSQL(t *testing.T) {
	db := open(t, ":memory:?dialect=postgresql")
	var sum int
	var mark string
	if err := db.QueryRow("SELECT CAST(? AS integer) + CAST(? AS integer), '?'", 2, 3).Scan(&sum, &mark); err != nil {
		t.Fatal(err)
	}
	if sum != 5 || mark != "?" {
		t.Fatalf("got %d %q", sum, mark)
	}
}

func TestPreparedStatementsStepRowsAndCopyMovesData(t *testing.T) {
	db := open(t, ":memory:?dialect=postgresql")
	ctx := context.Background()
	connection, err := db.Conn(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if _, err := connection.ExecContext(ctx, "CREATE TABLE t (id integer, name text)"); err != nil {
		t.Fatal(err)
	}
	insert, err := connection.PrepareContext(ctx, "INSERT INTO t VALUES (?, ?)")
	if err != nil {
		t.Fatal(err)
	}
	for id, name := range []string{"ada", "grace", "edsger"} {
		if _, err := insert.ExecContext(ctx, int64(id+1), name); err != nil {
			t.Fatal(err)
		}
	}
	insert.Close()
	loaded, err := eggdb.CopyFrom(ctx, connection, "COPY t FROM STDIN", strings.NewReader("4\tbarbara\n5\tdonald\n"))
	if err != nil || loaded != 2 {
		t.Fatalf("copy from: %d, %v", loaded, err)
	}
	selecting, err := connection.PrepareContext(ctx, "SELECT name FROM t WHERE id > ? ORDER BY id")
	if err != nil {
		t.Fatal(err)
	}
	defer selecting.Close()
	rows, err := selecting.QueryContext(ctx, int64(2))
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	for rows.Next() {
		var name string
		if err := rows.Scan(&name); err != nil {
			t.Fatal(err)
		}
		names = append(names, name)
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	if strings.Join(names, ",") != "edsger,barbara,donald" {
		t.Fatalf("stepped rows: %v", names)
	}
	var out bytes.Buffer
	if err := eggdb.CopyTo(ctx, connection, "COPY (SELECT id FROM t WHERE id <= 2 ORDER BY id) TO STDOUT", &out); err != nil {
		t.Fatal(err)
	}
	if out.String() != "1\n2\n" {
		t.Fatalf("copy to: %q", out.String())
	}
}
