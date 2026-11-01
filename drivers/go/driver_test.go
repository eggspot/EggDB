package eggdb_test

import (
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
