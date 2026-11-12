package eggdb_test

import (
	"path/filepath"
	"testing"

	"github.com/eggspot/EggDB/drivers/go"
)

func TestCollectionsHoldKeysApartAndRenameAndCount(t *testing.T) {
	store, err := eggdb.OpenKeyValue(filepath.Join(t.TempDir(), "kv.db"), false)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	err = store.Update(func(txn *eggdb.Txn) error {
		if _, err := txn.CreateCollection("sessions"); err != nil {
			return err
		}
		if err := txn.Put("sessions", []byte("s:1"), []byte("ada"), 0); err != nil {
			return err
		}
		return txn.Put("", []byte("plain"), []byte("default"), 0)
	})
	if err != nil {
		t.Fatal(err)
	}
	err = store.View(func(txn *eggdb.Txn) error {
		value, err := txn.Get("sessions", []byte("s:1"))
		if err != nil || string(value) != "ada" {
			t.Fatalf("got %q, %v", value, err)
		}
		missing, err := txn.Get("", []byte("s:1"))
		if err != nil || missing != nil {
			t.Fatalf("the default key space holds %q, %v", missing, err)
		}
		stats, err := txn.Stats("sessions")
		if err != nil || stats.Entries != 1 {
			t.Fatalf("stats %+v, %v", stats, err)
		}
		var keys []string
		err = txn.Scan("sessions", eggdb.ScanOptions{Prefix: []byte("s:")}, func(entry eggdb.Entry) bool {
			keys = append(keys, string(entry.Key))
			return true
		})
		if err != nil || len(keys) != 1 {
			t.Fatalf("scanned %v, %v", keys, err)
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := store.Update(func(txn *eggdb.Txn) error { return txn.RenameCollection("sessions", "logins") }); err != nil {
		t.Fatal(err)
	}
	err = store.View(func(txn *eggdb.Txn) error {
		names, err := txn.Collections()
		if err != nil || len(names) != 1 || names[0] != "logins" {
			t.Fatalf("collections %v, %v", names, err)
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
}
