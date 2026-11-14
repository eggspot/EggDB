package main

import (
	"database/sql"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"time"

	_ "github.com/eggspot/EggDB/drivers/go"
	_ "github.com/mattn/go-sqlite3"
)

func setting(name string, fallback int) int {
	if value, err := strconv.Atoi(os.Getenv(name)); err == nil {
		return value
	}
	return fallback
}

var rows = setting("DRIVER_BENCH_ROWS", 20000)
var lookups = setting("DRIVER_BENCH_LOOKUPS", 5000)

func timed(work func() error) float64 {
	started := time.Now()
	if err := work(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	return float64(time.Since(started).Microseconds()) / 1000
}

func side(driverName, file string) [3]float64 {
	db, err := sql.Open(driverName, file)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	defer db.Close()
	db.SetMaxOpenConns(1)
	if _, err := db.Exec("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)"); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	var out [3]float64
	out[0] = timed(func() error {
		tx, err := db.Begin()
		if err != nil {
			return err
		}
		statement, err := tx.Prepare("INSERT INTO t VALUES (?, ?, ?)")
		if err != nil {
			return err
		}
		for at := 1; at <= rows; at++ {
			if _, err := statement.Exec(int64(at), fmt.Sprintf("name %d", at), float64(at)*0.25); err != nil {
				return err
			}
		}
		statement.Close()
		return tx.Commit()
	})
	out[1] = timed(func() error {
		statement, err := db.Prepare("SELECT name FROM t WHERE id = ?")
		if err != nil {
			return err
		}
		defer statement.Close()
		var name string
		for at := 1; at <= lookups; at++ {
			if err := statement.QueryRow(int64(at*7%rows + 1)).Scan(&name); err != nil {
				return err
			}
		}
		return nil
	})
	out[2] = timed(func() error {
		statement, err := db.Prepare("SELECT name FROM t")
		if err != nil {
			return err
		}
		defer statement.Close()
		found, err := statement.Query()
		if err != nil {
			return err
		}
		defer found.Close()
		total := 0
		var name string
		for found.Next() {
			if err := found.Scan(&name); err != nil {
				return err
			}
			total += len(name)
		}
		return found.Err()
	})
	return out
}

func main() {
	directory, err := os.MkdirTemp("", "eggdb-go-bench")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	defer os.RemoveAll(directory)
	egg := side("eggdb", filepath.Join(directory, "bench.eggdb"))
	lite := side("sqlite3", filepath.Join(directory, "bench.sqlite"))
	for at, workload := range []string{"insert", "lookup", "scan"} {
		fmt.Printf("go %s %.2f %.2f\n", workload, egg[at], lite[at])
	}
}
