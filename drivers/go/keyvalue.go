package eggdb

/*
#cgo CFLAGS: -I${SRCDIR}/../../crates/eggdb-capi/include
#cgo LDFLAGS: -leggdb_capi
#include <stdlib.h>
#include "eggdb.h"

static const eggdb_entry *entry_at(const eggdb_entry *entries, size_t index) {
    return &entries[index];
}
*/
import "C"

import (
	"bytes"
	"time"
	"unsafe"
)

// KeyValue is an embedded database file's key-value store: transactions over its default key
// space and its named collections.
type KeyValue struct {
	database *C.eggdb_db
}

// Txn is one key-value transaction: a read over one snapshot, or a write that commits
// atomically. Methods take a collection name; "" is the default key space.
type Txn struct {
	handle  *C.eggdb_txn
	writing bool
}

// Entry is one key and its value from a scan.
type Entry struct {
	Key   []byte
	Value []byte
}

// ScanOptions chooses a scan's range: keys from Start up to End (End left out unless
// EndInclusive), or the keys beginning with Prefix, in key order or the other way with Reverse.
type ScanOptions struct {
	Start        []byte
	End          []byte
	Prefix       []byte
	Reverse      bool
	EndInclusive bool
}

// CollectionStats is what a collection holds.
type CollectionStats struct {
	Entries    uint64
	KeyBytes   uint64
	ValueBytes uint64
}

func closedError(what string) error {
	return &Error{Message: what}
}

func bytesPointer(data []byte) *C.uint8_t {
	if len(data) == 0 {
		return nil
	}
	return (*C.uint8_t)(C.CBytes(data))
}

func freeBytes(pointer *C.uint8_t) {
	if pointer != nil {
		C.free(unsafe.Pointer(pointer))
	}
}

func collectionName(collection string) *C.char {
	if collection == "" {
		return nil
	}
	return C.CString(collection)
}

func freeName(name *C.char) {
	if name != nil {
		C.free(unsafe.Pointer(name))
	}
}

// OpenKeyValue opens the key-value store of the database file at path.
func OpenKeyValue(path string, readOnly bool) (*KeyValue, error) {
	cPath := C.CString(path)
	defer C.free(unsafe.Pointer(cPath))
	var options C.eggdb_options
	if readOnly {
		options.flags = C.EGGDB_OPEN_READ_ONLY
	}
	store := &KeyValue{}
	if status := C.eggdb_open(cPath, &options, &store.database); status != C.EGGDB_OK {
		return nil, lastError(status)
	}
	return store, nil
}

// Close closes the file.
func (kv *KeyValue) Close() error {
	if kv.database == nil {
		return nil
	}
	status := C.eggdb_close(kv.database)
	kv.database = nil
	if status != C.EGGDB_OK {
		return lastError(status)
	}
	return nil
}

func (kv *KeyValue) begin(writing bool) (*Txn, error) {
	if kv.database == nil {
		return nil, closedError("the store is closed")
	}
	txn := &Txn{writing: writing}
	var status C.int
	if writing {
		status = C.eggdb_begin_write(kv.database, &txn.handle)
	} else {
		status = C.eggdb_begin_read(kv.database, &txn.handle)
	}
	if status != C.EGGDB_OK {
		return nil, lastError(status)
	}
	return txn, nil
}

// Read begins a read transaction.
func (kv *KeyValue) Read() (*Txn, error) {
	return kv.begin(false)
}

// Write begins a write transaction.
func (kv *KeyValue) Write() (*Txn, error) {
	return kv.begin(true)
}

// Update runs work in a write transaction and commits it when work returns nil, or rolls it
// back when work returns an error.
func (kv *KeyValue) Update(work func(*Txn) error) error {
	txn, err := kv.Write()
	if err != nil {
		return err
	}
	if err := work(txn); err != nil {
		txn.Rollback()
		return err
	}
	_, err = txn.Commit()
	return err
}

// View runs work in a read transaction.
func (kv *KeyValue) View(work func(*Txn) error) error {
	txn, err := kv.Read()
	if err != nil {
		return err
	}
	defer txn.Rollback()
	return work(txn)
}

func (txn *Txn) live() error {
	if txn.handle == nil {
		return closedError("the transaction has ended")
	}
	return nil
}

// Get returns the value of key, or nil when it is absent or has expired.
func (txn *Txn) Get(collection string, key []byte) ([]byte, error) {
	if err := txn.live(); err != nil {
		return nil, err
	}
	name := collectionName(collection)
	defer freeName(name)
	cKey := bytesPointer(key)
	defer freeBytes(cKey)
	var data *C.uint8_t
	var length C.size_t
	if status := C.eggdb_get(txn.handle, name, cKey, C.size_t(len(key)), &data, &length); status != C.EGGDB_OK {
		return nil, lastError(status)
	}
	if data == nil {
		return nil, nil
	}
	defer C.eggdb_free(data, length)
	return C.GoBytes(unsafe.Pointer(data), C.int(length)), nil
}

// Put stores value under key; a ttl above zero gives it a time to live.
func (txn *Txn) Put(collection string, key, value []byte, ttl time.Duration) error {
	if err := txn.live(); err != nil {
		return err
	}
	name := collectionName(collection)
	defer freeName(name)
	cKey := bytesPointer(key)
	defer freeBytes(cKey)
	cValue := bytesPointer(value)
	defer freeBytes(cValue)
	var status C.int
	if ttl > 0 {
		status = C.eggdb_put_with_expiry(txn.handle, name, cKey, C.size_t(len(key)), cValue, C.size_t(len(value)), C.uint64_t(ttl.Milliseconds()))
	} else {
		status = C.eggdb_put(txn.handle, name, cKey, C.size_t(len(key)), cValue, C.size_t(len(value)))
	}
	if status != C.EGGDB_OK {
		return lastError(status)
	}
	return nil
}

// Delete removes key and reports whether it was there.
func (txn *Txn) Delete(collection string, key []byte) (bool, error) {
	if err := txn.live(); err != nil {
		return false, err
	}
	name := collectionName(collection)
	defer freeName(name)
	cKey := bytesPointer(key)
	defer freeBytes(cKey)
	var deleted C.int
	if status := C.eggdb_delete(txn.handle, name, cKey, C.size_t(len(key)), &deleted); status != C.EGGDB_OK {
		return false, lastError(status)
	}
	return deleted != 0, nil
}

// Scan calls visit with each entry of the range in order, stopping early when visit returns
// false.
func (txn *Txn) Scan(collection string, options ScanOptions, visit func(Entry) bool) error {
	if err := txn.live(); err != nil {
		return err
	}
	var flags C.uint32_t
	start, end := options.Start, options.End
	if options.Reverse {
		flags |= C.EGGDB_SCAN_REVERSE
	}
	if options.EndInclusive {
		flags |= C.EGGDB_SCAN_END_INCLUSIVE
	}
	if options.Prefix != nil {
		flags |= C.EGGDB_SCAN_PREFIX
		start, end = options.Prefix, nil
	}
	name := collectionName(collection)
	defer freeName(name)
	cStart := bytesPointer(start)
	defer freeBytes(cStart)
	cEnd := bytesPointer(end)
	defer freeBytes(cEnd)
	var scan *C.eggdb_scan
	if status := C.eggdb_scan_open(txn.handle, name, cStart, C.size_t(len(start)), cEnd, C.size_t(len(end)), flags, &scan); status != C.EGGDB_OK {
		return lastError(status)
	}
	defer C.eggdb_scan_free(scan)
	for {
		var entries *C.eggdb_entry
		var count C.size_t
		if status := C.eggdb_scan_next(txn.handle, scan, 0, &entries, &count); status != C.EGGDB_OK {
			return lastError(status)
		}
		if count == 0 {
			return nil
		}
		for index := C.size_t(0); index < count; index++ {
			entry := C.entry_at(entries, index)
			found := Entry{
				Key:   C.GoBytes(unsafe.Pointer(entry.key), C.int(entry.key_len)),
				Value: C.GoBytes(unsafe.Pointer(entry.value), C.int(entry.value_len)),
			}
			if !visit(found) {
				return nil
			}
		}
	}
}

// CreateCollection creates the collection name and reports whether it was made.
func (txn *Txn) CreateCollection(name string) (bool, error) {
	if err := txn.live(); err != nil {
		return false, err
	}
	cName := C.CString(name)
	defer C.free(unsafe.Pointer(cName))
	var created C.int
	if status := C.eggdb_create_collection(txn.handle, cName, &created); status != C.EGGDB_OK {
		return false, lastError(status)
	}
	return created != 0, nil
}

// DropCollection drops the collection name with its keys and reports whether it was there.
func (txn *Txn) DropCollection(name string) (bool, error) {
	if err := txn.live(); err != nil {
		return false, err
	}
	cName := C.CString(name)
	defer C.free(unsafe.Pointer(cName))
	var dropped C.int
	if status := C.eggdb_drop_collection(txn.handle, cName, &dropped); status != C.EGGDB_OK {
		return false, lastError(status)
	}
	return dropped != 0, nil
}

// RenameCollection renames the collection name to newName, keeping its keys.
func (txn *Txn) RenameCollection(name, newName string) error {
	if err := txn.live(); err != nil {
		return err
	}
	cName := C.CString(name)
	defer C.free(unsafe.Pointer(cName))
	cNew := C.CString(newName)
	defer C.free(unsafe.Pointer(cNew))
	if status := C.eggdb_rename_collection(txn.handle, cName, cNew); status != C.EGGDB_OK {
		return lastError(status)
	}
	return nil
}

// Collections lists the collections' names in name order.
func (txn *Txn) Collections() ([]string, error) {
	if err := txn.live(); err != nil {
		return nil, err
	}
	var data *C.uint8_t
	var length, count C.size_t
	if status := C.eggdb_collection_names(txn.handle, &data, &length, &count); status != C.EGGDB_OK {
		return nil, lastError(status)
	}
	if data == nil {
		return nil, nil
	}
	defer C.eggdb_free(data, length)
	joined := C.GoBytes(unsafe.Pointer(data), C.int(length))
	names := make([]string, 0, int(count))
	for _, name := range bytes.Split(joined, []byte{0}) {
		if len(names) == int(count) {
			break
		}
		names = append(names, string(name))
	}
	return names, nil
}

// Stats counts the collection name; it needs a read transaction.
func (txn *Txn) Stats(name string) (CollectionStats, error) {
	if err := txn.live(); err != nil {
		return CollectionStats{}, err
	}
	cName := C.CString(name)
	defer C.free(unsafe.Pointer(cName))
	var stats C.eggdb_collection_stats
	if status := C.eggdb_collection_stats(txn.handle, cName, &stats); status != C.EGGDB_OK {
		return CollectionStats{}, lastError(status)
	}
	return CollectionStats{Entries: uint64(stats.entries), KeyBytes: uint64(stats.key_bytes), ValueBytes: uint64(stats.value_bytes)}, nil
}

// Commit ends the transaction, committing its writes, and returns the commit's stamp.
func (txn *Txn) Commit() (uint64, error) {
	if err := txn.live(); err != nil {
		return 0, err
	}
	var stamp C.uint64_t
	status := C.eggdb_commit(txn.handle, &stamp)
	txn.handle = nil
	if status != C.EGGDB_OK {
		return 0, lastError(status)
	}
	return uint64(stamp), nil
}

// Rollback ends the transaction without committing; after Commit it does nothing.
func (txn *Txn) Rollback() {
	if txn.handle != nil {
		C.eggdb_rollback(txn.handle)
		txn.handle = nil
	}
}
